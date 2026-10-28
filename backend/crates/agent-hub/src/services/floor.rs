//! Port of services/floor.py: the policy floor (app repo policy/floor.yaml, never content), enforced on every
//! effective config. Floor wins; content may only be stricter. Security control S-FLOOR.
//! Owner: WS-F. Every decision uses the exact `py::fnmatchcase`, `norm_path` and `overlaps` ports; the decisions are
//! compared with a Python-generated golden table in tests/test_floor.rs.

use std::path::Path;

use hub_core::models::Finding;
use hub_core::pyd::{self, Extra, Model, Ty, optional};
use hub_core::value::PyValue;
use hub_core::{HubResult, Problem, yamlsafe};
use indexmap::IndexMap;
use serde_json::Value;

use crate::adapters::base::{McpItem, PermissionSet, Rule};

#[derive(Debug, Clone, PartialEq)]
pub struct ToolPolicy {
    pub default: String,
    pub max: String,
}

/// `FloorPolicy` (extra="forbid").
#[derive(Debug, Clone, PartialEq)]
pub struct FloorPolicy {
    pub version: i64,
    pub network_default: String,
    pub egress: IndexMap<String, Vec<String>>,
    pub deny_path_read: Vec<String>,
    pub deny_path_write: Vec<String>,
    pub tools: IndexMap<String, ToolPolicy>,
    pub default_tool_decision: String,
    pub command_allow_forbid_patterns: Vec<String>,
    pub command_allow_probes: Vec<String>,
    pub command_allow_warn_probes: Vec<String>,
    pub mcp: IndexMap<String, bool>,
    /// the validated document (for `describe()` = `model_dump(mode="json")`)
    pub raw: PyValue,
}

fn tool_policy() -> &'static Model {
    static M: std::sync::LazyLock<Model> = std::sync::LazyLock::new(|| Model {
        name: "ToolPolicy",
        fields: vec![
            optional("default", Ty::str(), || PyValue::str("ask")),
            optional("max", Ty::str(), || PyValue::str("ask")),
        ],
        extra: Extra::Forbid,
        check: None,
        transform: None,
    });
    &M
}

pub fn floor_policy_model() -> &'static Model {
    static M: std::sync::LazyLock<Model> = std::sync::LazyLock::new(|| {
        let el = || PyValue::List(vec![]);
        let ed = || PyValue::Dict(vec![]);
        Model {
            name: "FloorPolicy",
            fields: vec![
                optional("version", Ty::int(), || PyValue::Int(1)),
                optional("network_default", Ty::str(), || PyValue::str("deny")),
                optional("egress", Ty::dict(Ty::str(), Ty::list(Ty::str())), ed),
                optional("deny_path_read", Ty::list(Ty::str()), el),
                optional("deny_path_write", Ty::list(Ty::str()), el),
                optional("tools", Ty::dict(Ty::str(), Ty::Model(tool_policy)), ed),
                optional("default_tool_decision", Ty::str(), || PyValue::str("ask")),
                optional("command_allow_forbid_patterns", Ty::list(Ty::str()), el),
                optional("command_allow_probes", Ty::list(Ty::str()), el),
                optional("command_allow_warn_probes", Ty::list(Ty::str()), el),
                optional("mcp", Ty::dict(Ty::str(), Ty::Bool), ed),
            ],
            extra: Extra::Forbid,
            check: None,
            transform: None,
        }
    });
    &M
}

fn strs(v: Option<&PyValue>) -> Vec<String> {
    v.and_then(PyValue::as_list)
        .map(|l| l.iter().map(PyValue::py_str).collect())
        .unwrap_or_default()
}

/// `load_floor(path)`: fail closed. A missing or invalid floor is a 503 `floor_unavailable` (the hub does not start).
pub fn load_floor(path: &Path) -> HubResult<FloorPolicy> {
    let fail = |msg: String| {
        Problem::unavailable(format!(
            "policy floor unusable ({}): {}",
            path.display(),
            hub_core::py::text::take_chars(&msg, 200)
        ))
        .code("floor_unavailable")
    };
    let text = std::fs::read_to_string(path)
        .map_err(|e| fail(hub_core::py::os::error_str(&e, Some(&path.to_string_lossy()))))?;
    let doc = yamlsafe::load(&text, yamlsafe::DEFAULT_MAX_BYTES).map_err(|e| fail(e.0))?;
    let v = pyd::validate(floor_policy_model(), &doc).map_err(|e| fail(e.to_string()))?;
    let egress = v
        .get("egress")
        .and_then(PyValue::as_dict)
        .map(|d| d.iter().map(|(k, x)| (k.py_str(), strs(Some(x)))).collect())
        .unwrap_or_default();
    let tools = v
        .get("tools")
        .and_then(PyValue::as_dict)
        .map(|d| {
            d.iter()
                .map(|(k, x)| {
                    let g = |f: &str| x.get(f).map(PyValue::py_str).unwrap_or_default();
                    (
                        k.py_str(),
                        ToolPolicy {
                            default: g("default"),
                            max: g("max"),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let mcp = v
        .get("mcp")
        .and_then(PyValue::as_dict)
        .map(|d| {
            d.iter()
                .map(|(k, x)| (k.py_str(), matches!(x, PyValue::Bool(true))))
                .collect()
        })
        .unwrap_or_default();
    let s = |k: &str| v.get(k).map(PyValue::py_str).unwrap_or_default();
    Ok(FloorPolicy {
        version: if let Some(PyValue::Int(i)) = v.get("version") {
            *i
        } else {
            1
        },
        network_default: s("network_default"),
        egress,
        deny_path_read: strs(v.get("deny_path_read")),
        deny_path_write: strs(v.get("deny_path_write")),
        tools,
        default_tool_decision: s("default_tool_decision"),
        command_allow_forbid_patterns: strs(v.get("command_allow_forbid_patterns")),
        command_allow_probes: strs(v.get("command_allow_probes")),
        command_allow_warn_probes: strs(v.get("command_allow_warn_probes")),
        mcp,
        raw: v,
    })
}

/// `norm_path(p)`: expand a leading ~ (the home dir) and normpath absolute paths containing `/.` or `//`, so a
/// `..` detour cannot dodge a deny glob.
pub fn norm_path(p: &str) -> String {
    let home = crate::config::home().to_string_lossy().to_string();
    let mut p = if p == "~" || p.starts_with("~/") {
        format!("{home}{}", &p[1..])
    } else {
        p.to_string()
    };
    if p.starts_with('/') && (p.contains("/.") || p.contains("//")) {
        p = hub_core::py::os::normpath(&p);
    }
    p
}

fn stem(p: &str) -> &str {
    match p.find(['*', '?', '[']) {
        Some(i) => &p[..i],
        None => p,
    }
}

/// `overlaps(a, b)`: could one path glob match anything the other matches? Deliberately over-approximate (fail closed).
pub fn overlaps(a: &str, b: &str) -> bool {
    use hub_core::py::fnmatchcase as m;
    let (a, b) = (norm_path(a), norm_path(b));
    if m(&a, &b) || m(&b, &a) {
        return true;
    }
    for (x, y) in [(&a, &b), (&b, &a)] {
        if let Some(tail) = y.strip_prefix("**/")
            && (m(x, tail) || m(&format!("/{}", x.trim_start_matches('/')), y))
        {
            return true;
        }
    }
    let (sa, sb) = (stem(&a), stem(&b));
    if sa.is_empty() || sb.is_empty() {
        return false;
    }
    sa.starts_with(sb) || sb.starts_with(sa)
}

/// `host_matches(pattern, host)`: case-insensitive fnmatch.
pub fn host_matches(pattern: &str, host: &str) -> bool {
    hub_core::py::fnmatchcase(&host.to_lowercase(), &pattern.to_lowercase())
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct FloorResult {
    pub permissions: PermissionSet,
    pub findings: Vec<Finding>,
    pub allow_hosts: Vec<String>,
    pub kept: Vec<Rule>,
    pub kept_ids: Vec<String>,
}

/// `ORDER.get(decision, default)`: deny < ask < allow.
fn order(decision: &str, default: u8) -> u8 {
    match decision {
        "deny" => 0,
        "ask" => 1,
        "allow" => 2,
        _ => default,
    }
}

/// Python's `repr()` of a `list[str]` (messages embed `{wide[:3]}` and friends).
pub fn list_repr(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|s| hub_core::py::repr(s)).collect();
    format!("[{}]", inner.join(", "))
}

fn first_n(items: &[String], n: usize) -> &[String] {
    &items[..items.len().min(n)]
}

fn finding(code: &str, severity: &str, message: String) -> Finding {
    Finding {
        code: code.into(),
        severity: severity.into(),
        message,
        item: None,
        concern: None,
        rule: None,
    }
}

/// Characters whose NFKC form contains one of `/?#@:` (Unicode 15.1, CPython 3.13 `unicodedata`); `urlsplit`'s
/// `_checknetloc` refuses a netloc holding one of them. Generated with
/// `[c for c in map(chr, range(0x110000)) if c not in "/?#@:" and any(x in unicodedata.normalize("NFKC", c) for x in "/?#@:")]`.
const NFKC_NETLOC_BREAKERS: [char; 19] = [
    '\u{2047}', '\u{2048}', '\u{2049}', '\u{2100}', '\u{2101}', '\u{2105}', '\u{2106}', '\u{2A74}', '\u{FE13}',
    '\u{FE16}', '\u{FE55}', '\u{FE56}', '\u{FE5F}', '\u{FE6B}', '\u{FF03}', '\u{FF0F}', '\u{FF1A}', '\u{FF1F}',
    '\u{FF20}',
];

/// The `ValueError` urllib raises for a malformed URL (callers turn it into a 500, like Python's unhandled exception).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlError(pub &'static str);

/// `ipaddress.IPv4Address(s)` parses (CPython 3.13: 4 ASCII-decimal octets, at most 3 digits, no leading zero, <= 255).
fn py_ipv4_ok(s: &str) -> bool {
    if s.is_empty() || s.contains('/') {
        return false;
    }
    let octets: Vec<&str> = s.split('.').collect();
    octets.len() == 4
        && octets.iter().all(|o| {
            !o.is_empty()
                && o.len() <= 3
                && o.bytes().all(|b| b.is_ascii_digit())
                && !(o.len() > 1 && o.starts_with('0'))
                && o.parse::<u16>().is_ok_and(|v| v <= 255)
        })
}

/// `ipaddress.IPv6Address(s)` parses (CPython 3.13 `_split_scope_id` + `_ip_int_from_string`, exactly).
fn py_ipv6_ok(s: &str) -> bool {
    if s.contains('/') {
        return false;
    }
    let (addr, scope) = match s.split_once('%') {
        Some((a, sc)) => (a, Some(sc)),
        None => (s, None),
    };
    if let Some(sc) = scope
        && (sc.is_empty() || sc.contains('%'))
    {
        return false;
    }
    if addr.is_empty() || addr.chars().count() > 45 {
        return false;
    }
    let mut parts: Vec<String> = addr.split(':').map(str::to_string).collect();
    if parts.len() < 3 {
        return false;
    }
    if parts.last().is_some_and(|p| p.contains('.')) {
        let v4 = parts.pop().unwrap_or_default();
        if !py_ipv4_ok(&v4) {
            return false;
        }
        parts.push("0".into());
        parts.push("0".into());
    }
    let hextets = 8usize;
    if parts.len() > hextets + 1 {
        return false;
    }
    let mut skip: Option<usize> = None;
    for (i, p) in parts.iter().enumerate().take(parts.len() - 1).skip(1) {
        if p.is_empty() {
            if skip.is_some() {
                return false;
            }
            skip = Some(i);
        }
    }
    let n = parts.len();
    let (hi, lo) = if let Some(si) = skip {
        let mut hi = si;
        let mut lo = n - si - 1;
        if parts[0].is_empty() {
            hi -= 1;
            if hi != 0 {
                return false;
            }
        }
        if parts[n - 1].is_empty() {
            lo -= 1;
            if lo != 0 {
                return false;
            }
        }
        if hextets < hi + lo + 1 {
            return false;
        }
        (hi, lo)
    } else {
        if n != hextets || parts[0].is_empty() || parts[n - 1].is_empty() {
            return false;
        }
        (n, 0)
    };
    let hextet_ok = |h: &str| !h.is_empty() && h.len() <= 4 && h.chars().all(|c| c.is_ascii_hexdigit());
    parts[..hi].iter().all(|h| hextet_ok(h)) && parts[n - lo..].iter().all(|h| hextet_ok(h))
}

/// `urllib.parse._check_bracketed_host`.
fn check_bracketed_host(host: &str) -> Result<(), UrlError> {
    if let Some(rest) = host.strip_prefix('v') {
        // re.match(r"\Av[a-fA-F0-9]+\..+\Z", hostname): `.` is anything but "\n"
        let hex_len = rest.chars().take_while(char::is_ascii_hexdigit).count();
        let after = &rest[hex_len..];
        let ok = hex_len > 0
            && after
                .strip_prefix('.')
                .is_some_and(|t| !t.is_empty() && !t.contains('\n'));
        return if ok {
            Ok(())
        } else {
            Err(UrlError("IPvFuture address is invalid"))
        };
    }
    if py_ipv4_ok(host) {
        return Err(UrlError("An IPv4 address cannot be in brackets"));
    }
    if py_ipv6_ok(host) {
        Ok(())
    } else {
        Err(UrlError("does not appear to be an IPv4 or IPv6 address"))
    }
}

/// `urllib.parse.urlsplit(url).hostname or ""` (CPython 3.13), including the `ValueError`s urlsplit raises.
pub fn url_hostname(url: &str) -> Result<String, UrlError> {
    let url = url.trim_start_matches(|c: char| c <= ' ');
    let url: String = url.chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();
    let mut rest: &str = &url;
    if let Some(i) = url.find(':')
        && i > 0
    {
        let first = url.chars().next().unwrap_or(' ');
        if first.is_ascii_alphabetic()
            && url[..i]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            rest = &url[i + 1..];
        }
    }
    let mut netloc = "";
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = &after[..end];
        let (open, close) = (netloc.contains('['), netloc.contains(']'));
        if open != close {
            return Err(UrlError("Invalid IPv6 URL"));
        }
        if open && close {
            // _check_bracketed_netloc
            let hp = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
            let host = if let Some((before, bracketed)) = hp.split_once('[') {
                if !before.is_empty() {
                    return Err(UrlError("Invalid IPv6 URL"));
                }
                let (h, port) = bracketed.split_once(']').unwrap_or((bracketed, ""));
                if !port.is_empty() && !port.starts_with(':') {
                    return Err(UrlError("Invalid IPv6 URL"));
                }
                h
            } else {
                hp.split_once(':').map_or(hp, |(h, _)| h)
            };
            check_bracketed_host(host)?;
        }
    }
    // _checknetloc
    if !netloc.is_ascii()
        && netloc
            .chars()
            .filter(|c| !matches!(c, '@' | ':' | '#' | '?'))
            .any(|c| NFKC_NETLOC_BREAKERS.contains(&c))
    {
        return Err(UrlError("netloc contains invalid characters under NFKC normalization"));
    }
    // _hostinfo + hostname
    let hostinfo = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let hostname = match hostinfo.split_once('[') {
        Some((_, bracketed)) => bracketed.split_once(']').map_or(bracketed, |(h, _)| h),
        None => hostinfo.split_once(':').map_or(hostinfo, |(h, _)| h),
    };
    if hostname.is_empty() {
        return Ok(String::new());
    }
    Ok(match hostname.split_once('%') {
        Some((h, zone)) => format!("{}%{zone}", h.to_lowercase()),
        None => hostname.to_lowercase(),
    })
}

pub struct Floor {
    pub p: FloorPolicy,
}

impl Floor {
    pub fn new(policy: FloorPolicy) -> Self {
        Self { p: policy }
    }

    // -- rules ---------------------------------------------------------------------------------------------------
    /// The floor's own deny rules (secrets and credentials), reads first.
    pub fn floor_rules(&self) -> Vec<Rule> {
        let deny = |kind: &str, g: &String| Rule {
            kind: kind.into(),
            matches: g.clone(),
            decision: "deny".into(),
            reason: Some("floor: secrets and credentials".into()),
        };
        let mut rules: Vec<Rule> = self.p.deny_path_read.iter().map(|g| deny("path_read", g)).collect();
        rules.extend(self.p.deny_path_write.iter().map(|g| deny("path_write", g)));
        rules
    }

    fn egress_list(&self, key: &str) -> &[String] {
        self.p.egress.get(key).map_or(&[], Vec::as_slice)
    }

    /// The pure decision behind `check_rule` (the locked signature wraps it in `HubResult`).
    pub fn rule_finding(&self, r: &Rule, concern: &str, rid: Option<&str>) -> Option<Finding> {
        let rule_label = format!("{}:{}", r.kind, r.matches);
        let bad = |code: &str, msg: String| Finding {
            code: code.into(),
            severity: "error".into(),
            message: msg,
            item: rid.map(str::to_string),
            concern: Some(concern.into()),
            rule: Some(rule_label.clone()),
        };
        let m = hub_core::py::repr(&r.matches);
        if r.decision == "allow" {
            if r.kind == "path_read" {
                // `next(...)` then `if hit:` -- an empty deny glob that overlaps first is falsy and hides later ones
                let hit = self.p.deny_path_read.iter().find(|g| overlaps(&r.matches, g));
                if let Some(h) = hit.filter(|h| !h.is_empty()) {
                    return Some(bad(
                        "floor_path_read",
                        format!(
                            "allowing reads of {m} overlaps the floor-denied {}",
                            hub_core::py::repr(h)
                        ),
                    ));
                }
            }
            if r.kind == "path_write" {
                let hit = self.p.deny_path_write.iter().find(|g| overlaps(&r.matches, g));
                if let Some(h) = hit.filter(|h| !h.is_empty()) {
                    return Some(bad(
                        "floor_path_write",
                        format!(
                            "allowing writes to {m} overlaps the floor-denied {}",
                            hub_core::py::repr(h)
                        ),
                    ));
                }
            }
            if r.kind == "egress_host"
                && (self.egress_list("forbid_allow_patterns").contains(&r.matches)
                    || self
                        .egress_list("forbid_allow_probes")
                        .iter()
                        .any(|pr| host_matches(&r.matches, pr)))
            {
                return Some(bad(
                    "floor_egress_wildcard",
                    format!("a catch-all egress allow ({m}) defeats deny-by-default"),
                ));
            }
            let stripped = hub_core::py::text::strip(&r.matches);
            if r.kind == "command"
                && (self.p.command_allow_forbid_patterns.iter().any(|f| f == stripped)
                    || stripped.is_empty()
                    || self
                        .p
                        .command_allow_probes
                        .iter()
                        .any(|pr| hub_core::py::fnmatchcase(pr, stripped)))
            {
                return Some(bad(
                    "floor_command_pattern",
                    "allow on a command rule needs a specific pattern, not a catch-all".into(),
                ));
            }
            if r.kind == "command"
                && self
                    .p
                    .command_allow_warn_probes
                    .iter()
                    .any(|pr| hub_core::py::fnmatchcase(pr, stripped))
            {
                return Some(Finding {
                    code: "floor_command_pattern".into(),
                    severity: "warn".into(),
                    message: format!(
                        "allow {m} lets the agent run arbitrary programs through a general-purpose launcher; narrow it to specific arguments"
                    ),
                    item: rid.map(str::to_string),
                    concern: Some(concern.into()),
                    rule: Some(rule_label.clone()),
                });
            }
        }
        if r.kind == "tool"
            && let Some(pol) = self.p.tools.get(&r.matches)
            && order(&r.decision, 2) > order(&pol.max, 2)
        {
            return Some(bad(
                "floor_tool",
                format!(
                    "tool {m} may be at most {}, rule says {}",
                    hub_core::py::repr(&pol.max),
                    hub_core::py::repr(&r.decision)
                ),
            ));
        }
        None
    }

    /// None when the rule is acceptable, else the violation. Used for content rules and for live-config audits.
    pub fn check_rule(&self, r: &Rule, concern: &str, rid: Option<&str>) -> HubResult<Option<Finding>> {
        Ok(self.rule_finding(r, concern, rid))
    }

    /// Every rule of a (live) permission set, plus its default tool decision and network default.
    pub fn check_permission_set(&self, ps: &PermissionSet, concern: &str) -> HubResult<Vec<Finding>> {
        let mut out: Vec<Finding> = ps
            .rules
            .iter()
            .filter_map(|r| self.rule_finding(r, concern, None))
            .collect();
        if order(&ps.default_tool_decision, 2) > order(&self.p.default_tool_decision, 1) {
            let mut f = finding(
                "floor_default_tool",
                "error",
                format!(
                    "default tool decision {} is weaker than the floor's {}",
                    hub_core::py::repr(&ps.default_tool_decision),
                    hub_core::py::repr(&self.p.default_tool_decision)
                ),
            );
            f.concern = Some(concern.into());
            out.push(f);
        }
        if ps.network_default != self.p.network_default {
            let mut f = finding(
                "floor_network_default",
                "error",
                format!(
                    "network default must be {}",
                    hub_core::py::repr(&self.p.network_default)
                ),
            );
            f.concern = Some(concern.into());
            out.push(f);
        }
        Ok(out)
    }

    /// content_rules: [(rule id, Rule)]; mcp_clean: server -> clean (None = MCP rules not checked).
    /// Violating rules are dropped (the output stays safe) and reported; warnings keep the rule.
    pub fn enforce(
        &self,
        content_rules: &[(String, Rule)],
        mcp_clean: Option<&IndexMap<String, bool>>,
    ) -> HubResult<FloorResult> {
        let mut findings = Vec::new();
        let mut kept: Vec<Rule> = Vec::new();
        let mut kept_ids = Vec::new();
        for (rid, r) in content_rules {
            let f = self
                .rule_finding(r, "permissions", Some(rid))
                .or_else(|| self.mcp_rule(r, Some(rid), mcp_clean));
            let keep = f.as_ref().is_none_or(|f| f.severity != "error");
            if let Some(f) = f {
                findings.push(f);
            }
            if keep {
                kept.push(r.clone());
                kept_ids.push(rid.clone());
            }
        }
        let named: std::collections::HashSet<&str> = kept
            .iter()
            .filter(|r| r.kind == "tool")
            .map(|r| r.matches.as_str())
            .collect();
        let defaults: Vec<Rule> = self
            .p
            .tools
            .iter()
            .filter(|(t, p)| !named.contains(t.as_str()) && p.default == "deny")
            .map(|(t, p)| Rule {
                kind: "tool".into(),
                matches: t.clone(),
                decision: p.default.clone(),
                reason: Some("floor default".into()),
            })
            .collect();
        let mut rules = self.floor_rules();
        rules.extend(defaults);
        rules.extend(kept.iter().cloned());
        let hosts: std::collections::BTreeSet<String> = kept
            .iter()
            .filter(|r| r.kind == "egress_host" && r.decision == "allow")
            .map(|r| r.matches.clone())
            .collect();
        Ok(FloorResult {
            permissions: PermissionSet {
                rules,
                default_tool_decision: self.p.default_tool_decision.clone(),
                network_default: self.p.network_default.clone(),
            },
            findings,
            allow_hosts: hosts.into_iter().collect(),
            kept,
            kept_ids,
        })
    }

    /// MCP allow rules. A WILDCARD allow (`tool:mcp`, `mcp__*`, a globbed server name) is always an error. A NAMED
    /// allow (`mcp__<server>[__tool]`, kind mcp_server) is valid only for a server that is enabled AND clean here.
    fn mcp_rule(&self, r: &Rule, rid: Option<&str>, mcp_clean: Option<&IndexMap<String, bool>>) -> Option<Finding> {
        let mcp_clean = mcp_clean?;
        if r.decision != "allow" {
            return None;
        }
        let server: Option<String> = if r.kind == "mcp_server" {
            Some(r.matches.clone())
        } else if r.kind == "tool" && (r.matches == "mcp" || r.matches.starts_with("mcp__")) {
            if r.matches == "mcp" {
                None
            } else {
                let tail = &r.matches[5..];
                Some(tail.split_once("__").map_or(tail, |(s, _)| s).to_string())
            }
        } else {
            return None;
        };
        let what = format!("{}:{}", r.kind, hub_core::py::repr(&r.matches));
        let err = |msg: String| Finding {
            code: "floor_mcp_rule".into(),
            severity: "error".into(),
            message: msg,
            item: rid.map(str::to_string),
            concern: Some("permissions".into()),
            rule: Some(format!("{}:{}", r.kind, r.matches)),
        };
        let server = match server {
            Some(s) if !s.is_empty() && !s.contains(['*', '?', '[']) => s,
            _ => {
                return Some(err(format!(
                    "allow {what} is a wildcard over MCP servers; name the server, and it must be enabled and clean"
                )));
            }
        };
        let sr = hub_core::py::repr(&server);
        match mcp_clean.get(&server) {
            None => Some(err(format!(
                "allow {what} names MCP server {sr}, which is not enabled for this client"
            ))),
            Some(false) => Some(err(format!(
                "allow {what} would enable MCP server {sr}, which is not scan_status=clean"
            ))),
            Some(true) => None,
        }
    }

    // -- skills ----------------------------------------------------------------------------------------------------
    /// allowed-tools entries: split on commas AND whitespace, but not inside parentheses (`Bash(git status:*)`).
    pub fn split_tools(raw: Option<&PyValue>) -> Vec<String> {
        match raw {
            Some(PyValue::List(items)) => items
                .iter()
                .map(PyValue::py_str)
                .filter_map(|s| {
                    let t = hub_core::py::text::strip(&s);
                    (!t.is_empty()).then(|| t.to_string())
                })
                .collect(),
            Some(PyValue::Str(raw)) => {
                let mut out = Vec::new();
                let mut cur = String::new();
                let mut depth: u64 = 0;
                for ch in raw.chars() {
                    if ch == '(' {
                        depth += 1;
                    } else if ch == ')' {
                        depth = depth.saturating_sub(1);
                    }
                    if depth == 0 && (ch == ',' || hub_core::py::text::is_space(ch)) {
                        if !cur.is_empty() {
                            out.push(std::mem::take(&mut cur));
                        }
                        cur.clear();
                    } else {
                        cur.push(ch);
                    }
                }
                if !cur.is_empty() {
                    out.push(cur);
                }
                out
            }
            _ => Vec::new(),
        }
    }

    /// `re.fullmatch(r"Bash\(\s*[*:]*\s*\)", e)` (`\s` = `str.isspace`).
    fn bash_catch_all(e: &str) -> bool {
        let Some(inner) = e.strip_prefix("Bash(").and_then(|x| x.strip_suffix(')')) else {
            return false;
        };
        let core = inner.trim_start_matches(hub_core::py::text::is_space);
        let core = core.trim_end_matches(hub_core::py::text::is_space);
        core.chars().all(|c| c == '*' || c == ':')
    }

    /// SKILL.md frontmatter extras (`allowed-tools`, `hooks`) judged, never rewritten (SKILL.md is client-owned and
    /// delivered byte-for-byte).
    pub fn check_skill(
        &self,
        name: &str,
        extra: &PyValue,
        clean_servers: Option<&[String]>,
    ) -> HubResult<Vec<Finding>> {
        let mut out: Vec<Finding> = Vec::new();
        let nr = hub_core::py::repr(name);
        let skill_finding = |code: &str, sev: &str, msg: String| Finding {
            code: code.into(),
            severity: sev.into(),
            message: msg,
            item: Some(name.into()),
            concern: Some("skills".into()),
            rule: None,
        };
        if extra.get("hooks").is_some() {
            out.push(skill_finding(
                "skill_declares_hooks",
                "error",
                format!("skill {nr} declares hooks in its frontmatter: hooks run code and are not allowed via skills"),
            ));
        }
        let entries = Self::split_tools(extra.get("allowed-tools"));
        let wide: Vec<String> = entries
            .iter()
            .filter(|e| matches!(e.as_str(), "Bash" | "*" | "mcp__*") || Self::bash_catch_all(e))
            .cloned()
            .collect();
        if !wide.is_empty() {
            out.push(skill_finding(
                "skill_unrestricted_tools",
                "error",
                format!(
                    "skill {nr} pre-approves unrestricted tools {} via allowed-tools",
                    list_repr(first_n(&wide, 3))
                ),
            ));
        }
        let mcp_named: Vec<&String> = entries
            .iter()
            .filter(|e| e.starts_with("mcp__") && !wide.contains(e))
            .collect();
        let mut unscanned = false;
        if let Some(clean) = clean_servers {
            let bad: Vec<String> = mcp_named
                .iter()
                .filter(|e| {
                    let tail = &e[5..];
                    let server = tail.split_once("__").map_or(tail, |(s, _)| s);
                    let server = if server.is_empty() { "*" } else { server };
                    !clean.iter().any(|c| c == server)
                })
                .map(|e| (*e).clone())
                .collect();
            if !bad.is_empty() {
                unscanned = true;
                out.push(skill_finding(
                    "skill_unscanned_mcp_tool",
                    "error",
                    format!(
                        "skill {nr} pre-approves MCP tools {} of servers that are not enabled and clean here",
                        list_repr(first_n(&bad, 3))
                    ),
                ));
            }
        }
        if !entries.is_empty() && wide.is_empty() && !unscanned {
            out.push(skill_finding(
                "skill_preapproves_tools",
                "warn",
                format!(
                    "skill {nr} pre-approves tools via allowed-tools: {}",
                    list_repr(first_n(&entries, 5))
                ),
            ));
        }
        Ok(out)
    }

    // -- mcp -------------------------------------------------------------------------------------------------------
    /// A malformed URL raises in Python (`ValueError` from urlsplit, an unexpected error = HTTP 500); the same here.
    pub fn check_mcp(&self, m: &McpItem, rules: &[Rule], concern: &str) -> HubResult<Vec<Finding>> {
        let mut out: Vec<Finding> = Vec::new();
        let name = hub_core::py::repr(&m.name);
        let mut err = |code: &str, msg: String| {
            out.push(Finding {
                code: code.into(),
                severity: "error".into(),
                message: msg,
                item: Some(m.name.clone()),
                concern: Some(concern.into()),
                rule: None,
            });
        };
        let flag = |k: &str| self.p.mcp.get(k).copied().unwrap_or(true);
        if flag("require_scan_clean") && m.scan_status != "clean" {
            err(
                "mcp_not_clean",
                format!(
                    "MCP server {name} has scan_status={}; it must be 'clean' (run your MCP scanner yourself, then `hubctl attest-mcp NAME`; edits to command/args/url/pinned_ref/transport, or a missing/foreign attestation, reset it)",
                    hub_core::py::repr(&m.scan_status)
                ),
            );
        }
        let allowed: Vec<&str> = rules
            .iter()
            .filter(|r| r.kind == "egress_host" && r.decision == "allow")
            .map(|r| r.matches.as_str())
            .collect();
        let denied: Vec<&str> = rules
            .iter()
            .filter(|r| r.kind == "egress_host" && r.decision == "deny")
            .map(|r| r.matches.as_str())
            .collect();
        if flag("require_explicit_egress") {
            for h in &m.egress_hosts {
                if denied.iter().any(|d| host_matches(d, h)) || !allowed.iter().any(|a| host_matches(a, h)) {
                    err(
                        "mcp_egress_not_allowed",
                        format!(
                            "egress host {} of MCP server {name} has no explicit egress_host allow rule",
                            hub_core::py::repr(h)
                        ),
                    );
                }
            }
        }
        if m.transport == "stdio"
            && flag("require_sandbox_stdio")
            && m.sandbox_profile.as_deref().unwrap_or("").is_empty()
        {
            err(
                "mcp_no_sandbox",
                format!("stdio MCP server {name} has no sandbox_profile (each MCP server runs in its own sandbox)"),
            );
        }
        if m.transport != "stdio"
            && let Some(url) = m.url.as_deref().filter(|u| !u.is_empty())
        {
            let host = url_hostname(url).map_err(|_| Problem::internal())?;
            if !self.egress_list("loopback_hosts").contains(&host) {
                if m.egress_hosts.is_empty() {
                    err(
                        "mcp_no_egress_declared",
                        format!("{} MCP server {name} must declare its egress_hosts", m.transport),
                    );
                } else if !m.egress_hosts.iter().any(|h| host_matches(h, &host)) {
                    err(
                        "mcp_url_host_undeclared",
                        format!(
                            "URL host {} of MCP server {name} is not in its egress_hosts",
                            hub_core::py::repr(&host)
                        ),
                    );
                }
            }
        }
        Ok(out)
    }

    // -- audit of a live config (advisory concerns included) --------------------------------------------------------
    /// One `floor_deny_missing` warning per floor deny glob that no deny rule of the live set covers.
    pub fn missing_denies(&self, ps: &PermissionSet) -> HubResult<Vec<Finding>> {
        let mut out = Vec::new();
        for (kind, globs) in [
            ("path_read", &self.p.deny_path_read),
            ("path_write", &self.p.deny_path_write),
        ] {
            for g in globs {
                let ng = norm_path(g);
                let covered = ps.rules.iter().any(|r| {
                    r.kind == kind && r.decision == "deny" && hub_core::py::fnmatchcase(&ng, &norm_path(&r.matches))
                });
                if !covered {
                    let mut f = finding(
                        "floor_deny_missing",
                        "warn",
                        format!("live config has no {kind} deny covering {}", hub_core::py::repr(g)),
                    );
                    f.concern = Some("permissions".into());
                    f.rule = Some(format!("{kind}:{g}"));
                    out.push(f);
                }
            }
        }
        Ok(out)
    }

    /// `describe()` = `model_dump(mode="json")` of the policy (GET /policy/floor), including the data-dir denies the
    /// Hub appends at startup.
    pub fn describe(&self) -> Value {
        let p = &self.p;
        let tools: serde_json::Map<String, Value> = p
            .tools
            .iter()
            .map(|(k, t)| (k.clone(), serde_json::json!({"default": t.default, "max": t.max})))
            .collect();
        serde_json::json!({
            "version": p.version,
            "network_default": p.network_default,
            "egress": p.egress,
            "deny_path_read": p.deny_path_read,
            "deny_path_write": p.deny_path_write,
            "tools": tools,
            "default_tool_decision": p.default_tool_decision,
            "command_allow_forbid_patterns": p.command_allow_forbid_patterns,
            "command_allow_probes": p.command_allow_probes,
            "command_allow_warn_probes": p.command_allow_warn_probes,
            "mcp": p.mcp,
        })
    }
}
