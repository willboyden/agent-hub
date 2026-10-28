//! Port of backend/src/agent_hub/adapters/turnstone.py: the Turnstone adapter (id `turnstone`). Owner: WS-A.
//!
//! Turnstone keeps skills, MCP servers and settings in its database and takes them through its console's admin API, so
//! the hub never writes into it. `render` produces a STAGE tree (root `stage`):
//!   skills/<name>/...      the skills tree (SKILL.md + resources)
//!   skills-meta.json       {"<skill>": {"category": ..., "tags": [...]}} (only skills that have a label)
//!   instructions.md        the default system instruction
//!   mcp.json               {"mcpServers": {...}}, the format of Turnstone's MCP import
//! `verify` checks the stage files and then asks the running console through `deploy/turnstone-deliver.py list`
//! (PORTING.md T-1; DEVIATION D-TURNSTONE: [`interpreter`] instead of `sys.executable`). The API token is read by
//! that script from a 0600 file; it never appears in the hub, in a command line or in output. Params from the client
//! YAML are checked first (loopback console URL, a `*.token` file under ~/.config/agent-hub, not a symlink); a refused
//! param is a failed check and nothing runs.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use hub_core::py::{os as pyos, text};
use hub_core::value::PyValue;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value, json};

use super::_util as u;
use super::base::{
    AdapterCaps, AdapterError, AdapterResult, Artifact, Check, ClientAdapter, ClientConfig, CommandRunner, Diag,
    DiscoveredContent, Expected, FileSystem, Files, McpItem, RenderContext, RenderResult,
};
use super::claude_code::flags_value;

pub const ID: &str = "turnstone";
pub const DEFAULT_MANAGE: [(&str, bool); 6] = [
    ("skills", true),
    ("agents", false),
    ("instructions", false),
    ("mcp", false),
    ("permissions", false),
    ("memory", false),
];
pub const DEFAULT_CONSOLE: &str = "http://127.0.0.1:8796";
/// the path of the token file, not a secret
pub const DEFAULT_TOKEN_FILE: &str = "~/.config/agent-hub/turnstone.token";
/// Turnstone refuses a larger SKILL.md / skill body
pub const MAX_SKILL_MD: usize = 32 * 1024;
/// per-skill category + tags; read by deploy/turnstone-deliver.py
pub const META_FILE: &str = "skills-meta.json";
pub const MAX_CATEGORY: usize = 64;
pub const MAX_TAGS: usize = 20;
/// Turnstone 1.8.4's rules for a skill's other files (kept equal to the delivery script's copies; a test compares them)
pub const RESOURCE_DIRS: [&str; 3] = ["scripts/", "references/", "assets/"];
pub const MAX_RESOURCE_CHARS: usize = 100 * 1024;
pub const MAX_RESOURCES_PER_SKILL: usize = 10;

static IPV_FUTURE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"(?s)^v[a-fA-F0-9]+\..+$"));

/// `TOKEN_DIR`: `Path.home() / ".config" / "agent-hub"`.
pub fn default_token_dir() -> PathBuf {
    crate::config::home().join(".config").join("agent-hub")
}

/// `DELIVER_SCRIPT`: `<app root>/deploy/turnstone-deliver.py`.
pub fn default_deliver_script() -> PathBuf {
    crate::config::app_root().join("deploy").join("turnstone-deliver.py")
}

/// `undelivered_resources(files)`: (path, why) for every file of a skill that deploy/turnstone-deliver.py will skip:
/// its rules, in its order.
pub fn undelivered_resources(files: &Files) -> Vec<(String, String)> {
    let skill_md = files.get("SKILL.md").map(|b| u::decode_replace(b)).unwrap_or_default();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut ok: Vec<String> = Vec::new();
    let mut paths: Vec<&String> = files.keys().collect();
    paths.sort();
    for path in paths {
        if path == "SKILL.md" {
            continue;
        }
        if !RESOURCE_DIRS.iter().any(|d| path.starts_with(d)) {
            out.push((path.clone(), format!("outside {}", RESOURCE_DIRS.join(", "))));
            continue;
        }
        let Ok(s) = std::str::from_utf8(&files[path.as_str()]) else {
            out.push((path.clone(), "not UTF-8 text".into()));
            continue;
        };
        if text::char_len(s) > MAX_RESOURCE_CHARS {
            out.push((path.clone(), format!("over {MAX_RESOURCE_CHARS} characters")));
            continue;
        }
        ok.push(path.clone());
    }
    // past the cap, files SKILL.md names are kept first
    ok.sort_by(|a, b| (!skill_md.contains(a.as_str()), a).cmp(&(!skill_md.contains(b.as_str()), b)));
    out.extend(
        ok.into_iter()
            .skip(MAX_RESOURCES_PER_SKILL)
            .map(|p| (p, format!("past {MAX_RESOURCES_PER_SKILL} files per skill"))),
    );
    out
}

// ---- urllib.parse.urlsplit (CPython 3.13) and ipaddress, as far as console_problem needs them ----------------------------
struct SplitUrl {
    scheme: String,
    netloc: String,
    path: String,
    query: String,
    fragment: String,
}

fn invalid_ipv6() -> String {
    "Invalid IPv6 URL".into()
}

/// `ipaddress.ip_address(s)` (IPv4 strict dotted quad, IPv6 with an optional `%scope`).
fn ip_address(s: &str) -> Option<IpAddr> {
    if let Ok(v4) = s.parse::<Ipv4Addr>()
        && s.split('.').all(|o| o.len() == 1 || !o.starts_with('0'))
    {
        return Some(IpAddr::V4(v4));
    }
    let (addr, scope) = match s.split_once('%') {
        Some((a, sc)) => (a, Some(sc)),
        None => (s, None),
    };
    if scope.is_some_and(|sc| sc.is_empty() || sc.contains('%')) {
        return None;
    }
    addr.parse::<Ipv6Addr>().ok().map(IpAddr::V6)
}

fn is_loopback(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        // CPython 3.13: an IPv4-mapped address answers for its IPv4 address
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or_else(|| v6.is_loopback(), |m| m.is_loopback()),
    }
}

fn check_bracketed_netloc(netloc: &str) -> Result<(), String> {
    let hostname_and_port = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let hostname = match hostname_and_port.split_once('[') {
        Some((before, bracketed)) => {
            if !before.is_empty() {
                return Err(invalid_ipv6());
            }
            let (hostname, port) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            if !port.is_empty() && !port.starts_with(':') {
                return Err(invalid_ipv6());
            }
            hostname
        }
        None => hostname_and_port.split_once(']').map_or(hostname_and_port, |(h, _)| h),
    };
    if hostname.starts_with('v') {
        if !IPV_FUTURE.is_match(hostname) {
            return Err("IPvFuture address is invalid".into());
        }
    } else {
        match ip_address(hostname) {
            None => return Err(format!("{} does not appear to be an IPv4 or IPv6 address", text::repr(hostname))),
            Some(IpAddr::V4(_)) => return Err("An IPv4 address cannot be in brackets".into()),
            Some(IpAddr::V6(_)) => {}
        }
    }
    Ok(())
}

fn urlsplit(url: &str) -> Result<SplitUrl, String> {
    let mut url: String = url.trim_start_matches(|c: char| (c as u32) <= 0x20).to_string();
    url.retain(|c| !matches!(c, '\t' | '\r' | '\n'));
    let mut scheme = String::new();
    if let Some(i) = url.find(':')
        && i > 0
        && url.as_bytes()[0].is_ascii_alphabetic()
        && url[..i].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        scheme = url[..i].to_ascii_lowercase();
        url = url[i + 1..].to_string();
    }
    let mut netloc = String::new();
    if url.starts_with("//") {
        let rest = &url[2..];
        let delim = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        netloc = rest[..delim].to_string();
        url = rest[delim..].to_string();
        let (open, close) = (netloc.contains('['), netloc.contains(']'));
        if open != close {
            return Err(invalid_ipv6());
        }
        if open && close {
            check_bracketed_netloc(&netloc)?;
        }
    }
    let mut fragment = String::new();
    if let Some((u_, f)) = url.split_once('#') {
        fragment = f.to_string();
        url = u_.to_string();
    }
    let mut query = String::new();
    if let Some((u_, q)) = url.split_once('?') {
        query = q.to_string();
        url = u_.to_string();
    }
    // `_checknetloc`: a non-ASCII netloc is checked under NFKC normalisation in Python (DEVIATION D-TURNSTONE-NFKC:
    // not reproduced; such a host is refused below as "must be a loopback address" instead of "not a valid URL").
    Ok(SplitUrl {
        scheme,
        netloc,
        path: url,
        query,
        fragment,
    })
}

impl SplitUrl {
    fn userinfo(&self) -> (Option<&str>, Option<&str>) {
        match self.netloc.rsplit_once('@') {
            None => (None, None),
            Some((info, _)) => match info.split_once(':') {
                Some((user, pw)) => (Some(user), Some(pw)),
                None => (Some(info), None),
            },
        }
    }

    fn hostinfo(&self) -> (&str, Option<&str>) {
        let hostinfo = self.netloc.rsplit_once('@').map_or(self.netloc.as_str(), |(_, h)| h);
        let (hostname, port) = match hostinfo.split_once('[') {
            Some((_, bracketed)) => {
                let (h, rest) = bracketed.split_once(']').unwrap_or((bracketed, ""));
                (h, rest.split_once(':').map_or("", |(_, p)| p))
            }
            None => hostinfo.split_once(':').unwrap_or((hostinfo, "")),
        };
        (hostname, if port.is_empty() { None } else { Some(port) })
    }

    fn hostname(&self) -> Option<String> {
        let (h, _) = self.hostinfo();
        if h.is_empty() {
            return None;
        }
        Some(match h.split_once('%') {
            Some((host, zone)) => format!("{}%{zone}", host.to_lowercase()),
            None => h.to_lowercase(),
        })
    }

    fn port(&self) -> Result<Option<u32>, String> {
        let Some(p) = self.hostinfo().1 else {
            return Ok(None);
        };
        if !p.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("Port could not be cast to integer value as {}", text::repr(p)));
        }
        match p.trim_start_matches('0').len() {
            0 => Ok(Some(0)),
            n if n > 5 => Err("Port out of range 0-65535".into()),
            _ => match p.parse::<u32>() {
                Ok(n) if n <= 65535 => Ok(Some(n)),
                _ => Err("Port out of range 0-65535".into()),
            },
        }
    }
}

/// `console_problem(url)`: `None` if `url` is a loopback http(s) base URL; else why not.
pub fn console_problem(url: &str) -> Option<String> {
    let parsed = urlsplit(url).and_then(|s| {
        let host = s.hostname().unwrap_or_default();
        let port_ok = s.port()?.is_none_or(|p| (1..=65535).contains(&p));
        Ok((s, host, port_ok))
    });
    let Ok((s, host, port_ok)) = parsed else {
        return Some("not a valid URL".into());
    };
    if !matches!(s.scheme.as_str(), "http" | "https") || host.is_empty() || !port_ok {
        return Some("must be an http(s) URL with a host".into());
    }
    let (user, pw) = s.userinfo();
    if user.is_some_and(|x| !x.is_empty())
        || pw.is_some_and(|x| !x.is_empty())
        || !(s.path.is_empty() || s.path == "/")
        || !s.query.is_empty()
        || !s.fragment.is_empty()
    {
        return Some("must be a bare base URL (no credentials, path, query or fragment)".into());
    }
    if host != "localhost" && !ip_address(&host).is_some_and(|ip| is_loopback(&ip)) {
        return Some("must be a loopback address".into());
    }
    None
}

/// `Path(p)` normalised the way pathlib does (no empty or `.` components, no trailing slash).
fn pathlib_norm(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    let lead = if p.starts_with("//") && !p.starts_with("///") {
        "//"
    } else if p.starts_with('/') {
        "/"
    } else {
        ""
    };
    let joined = format!("{lead}{}", parts.join("/"));
    if joined.is_empty() { ".".into() } else { joined }
}

/// `os.path.commonpath([a, b])` of two absolute normalised paths.
fn commonpath(a: &str, b: &str) -> String {
    let ca: Vec<&str> = a.split('/').filter(|c| !c.is_empty()).collect();
    let cb: Vec<&str> = b.split('/').filter(|c| !c.is_empty()).collect();
    let common: Vec<&str> = ca.iter().zip(cb.iter()).take_while(|(x, y)| x == y).map(|(x, _)| *x).collect();
    format!("/{}", common.join("/"))
}

/// `token_file_problem(path)`: `None` if `path` is a non-symlink `*.token` file inside the token dir; else why not.
pub fn token_file_problem(path: &str, token_dir: &Path) -> Option<String> {
    let p = pathlib_norm(path);
    if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
        return Some("must not be a symlink".into());
    }
    let real = pyos::realpath(Path::new(&p)).to_string_lossy().to_string();
    let base = pyos::realpath(token_dir).to_string_lossy().to_string();
    if commonpath(&real, &base) != base || real == base {
        return Some(format!("must be inside {}", pathlib_norm(&token_dir.to_string_lossy())));
    }
    let name = p.rsplit_once('/').map_or(p.as_str(), |(_, n)| n);
    if !name.ends_with(".token") {
        return Some("must be a *.token file".into());
    }
    None
}

/// `TurnstoneAdapter`. The paths are seams for tests (Python monkeypatches `TOKEN_DIR`); `None` = the defaults. `python`
/// stands in for `HUB_PYTHON` and is checked the same way.
#[derive(Debug, Default, Clone)]
pub struct TurnstoneAdapter {
    pub token_dir: Option<PathBuf>,
    pub deliver_script: Option<PathBuf>,
    pub python: Option<String>,
}

fn unhashable(v: &PyValue) -> Option<AdapterError> {
    matches!(v, PyValue::List(_) | PyValue::Dict(_) | PyValue::Set(_))
        .then(|| AdapterError::new("TypeError", format!("unhashable type: '{}'", v.type_name())))
}

/// `set(listed.get(key, []))` of a dict (a str iterates its characters, a dict its keys; `TypeError` as Python).
fn listed_set(doc: &PyValue, key: &str) -> AdapterResult<Vec<PyValue>> {
    if !matches!(doc, PyValue::Dict(_)) {
        return Ok(Vec::new());
    }
    match doc.get(key) {
        None => Ok(Vec::new()),
        Some(v) => {
            let items = u::py_iter(v)?;
            if let Some(e) = items.iter().find_map(unhashable) {
                return Err(e);
            }
            Ok(items)
        }
    }
}

impl TurnstoneAdapter {
    fn token_dir(&self) -> PathBuf {
        self.token_dir.clone().unwrap_or_else(default_token_dir)
    }

    fn deliver_script(&self) -> PathBuf {
        self.deliver_script.clone().unwrap_or_else(default_deliver_script)
    }

    fn interpreter(&self) -> Result<String, String> {
        match &self.python {
            Some(p) => interpreter_from(Some(p.clone()), &SYSTEM_PYTHONS),
            None => interpreter(),
        }
    }

    /// `_skill_meta(ctx, staged, diags)`: the category its hub source maps to and its hub tags plus that category.
    fn skill_meta(ctx: &RenderContext, staged: &BTreeSet<String>, diags: &mut Vec<Diag>) -> Map<String, Value> {
        let raw = ctx.client.param("category_from_source").filter(|v| u::truthy(v));
        let valid = |raw: &PyValue| {
            raw.as_dict().is_some_and(|d| {
                d.iter().all(|(k, v)| {
                    matches!(k, PyValue::Str(_))
                        && v.as_str()
                            .is_some_and(|s| text::char_len(s) <= MAX_CATEGORY && u::valid_name(s))
                })
            })
        };
        let mut map: Vec<(String, String)> = Vec::new();
        match raw {
            None => {}
            Some(r) if valid(r) => {
                map = r
                    .as_dict()
                    .unwrap_or_default()
                    .iter()
                    .map(|(k, v)| (k.py_str(), v.py_str()))
                    .collect();
            }
            Some(_) => diags.push(u::diag(
                "error",
                "invalid_param",
                format!(
                    "category_from_source must map a hub source to a category name (lowercase letters, digits, - _ ., \
                     at most {MAX_CATEGORY}); no categories rendered"
                ),
                None,
            )),
        }
        let mut out = Map::new();
        let mut skills: Vec<_> = ctx.skills.iter().collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        for s in skills {
            if !staged.contains(&s.name) {
                continue;
            }
            let category = map
                .iter()
                .find(|(k, _)| *k == s.source)
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let ok: Vec<String> = u::sorted_set(
                s.tags
                    .iter()
                    .filter(|t| text::char_len(t) <= MAX_CATEGORY && u::valid_name(t) && **t != category)
                    .cloned(),
            );
            let room = MAX_TAGS - usize::from(!category.is_empty());
            let mut dropped = u::sorted_set(s.tags.iter().filter(|t| **t != category && !ok.contains(t)).cloned());
            dropped.extend(ok.iter().skip(room).cloned());
            if !dropped.is_empty() {
                let shown: Vec<String> = dropped.iter().take(5).map(|t| text::repr(t)).collect();
                diags.push(u::diag(
                    "warn",
                    "tag_not_delivered",
                    format!(
                        "{} tag(s) left out (Turnstone takes lowercase letters, digits, - _ ., at most {MAX_CATEGORY} \
                         characters, {MAX_TAGS} per skill): {}",
                        dropped.len(),
                        shown.join(", ")
                    ),
                    Some(&s.name),
                ));
            }
            let mut tags: Vec<String> = ok.iter().take(room).cloned().collect();
            if !category.is_empty() {
                tags.push(category.clone());
            }
            let tags = u::sorted_set(tags);
            let mut entry = Map::new();
            if !category.is_empty() {
                entry.insert("category".into(), Value::from(category));
            }
            if !tags.is_empty() {
                entry.insert("tags".into(), Value::from(tags));
            }
            if !entry.is_empty() {
                out.insert(s.name.clone(), Value::Object(entry));
            }
        }
        out
    }

    fn mcp(items: &[McpItem], diags: &mut Vec<Diag>) -> Map<String, Value> {
        let mut out = Map::new();
        let mut sorted: Vec<&McpItem> = items.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for m in sorted {
            let name = m.name.as_str();
            if !u::valid_name(name) {
                diags.push(u::diag(
                    "error",
                    "invalid_name",
                    format!("mcp name {} is not valid", text::repr(name)),
                    Some(name),
                ));
                continue;
            }
            if m.scan_status != "clean" {
                diags.push(u::diag(
                    "info",
                    "mcp_not_clean",
                    format!("scan_status={}; the core blocks enabling unscanned servers", m.scan_status),
                    Some(name),
                ));
            }
            if !m.env_names.is_empty() {
                diags.push(u::diag(
                    "info",
                    "env_names_omitted",
                    "env var names not rendered: set credentials in Turnstone, never in the hub",
                    Some(name),
                ));
            }
            if m.sandbox_profile.as_deref().is_some_and(|p| !p.is_empty()) {
                diags.push(u::diag(
                    "info",
                    "sandbox_profile_ignored",
                    "Turnstone runs MCP servers itself; sandbox wrapping is not applied",
                    Some(name),
                ));
            }
            let entry = if m.transport == "stdio" {
                let Some(command) = m.command.as_deref().filter(|c| !c.is_empty()) else {
                    diags.push(u::diag("error", "mcp_no_command", "stdio server without a command", Some(name)));
                    continue;
                };
                diags.push(u::diag(
                    "warn",
                    "mcp_stdio_in_node",
                    "a stdio server runs inside the Turnstone node; anything it fetches at start (npx, uvx) needs \
                     network the node may not have. Prefer an http server run in its own sandbox",
                    Some(name),
                ));
                json!({"command": command, "args": m.args})
            } else {
                let Some(url) = m.url.as_deref().filter(|x| !x.is_empty()) else {
                    diags.push(u::diag(
                        "error",
                        "mcp_no_url",
                        format!("{} server without a url", m.transport),
                        Some(name),
                    ));
                    continue;
                };
                let t = if m.transport == "http" { "streamable-http" } else { "sse" };
                json!({"type": t, "url": url})
            };
            out.insert(name.to_string(), entry);
        }
        out
    }

    fn ask_console(&self, cfg: &ClientConfig, skills: &[String], run: &dyn CommandRunner) -> AdapterResult<Check> {
        let check = |ok: bool, detail: String| Check {
            name: "turnstone skills (console API)".into(),
            ok,
            detail,
        };
        let pick = |key: &str, default: &str| {
            cfg.param(key)
                .filter(|v| u::truthy(v))
                .map_or_else(|| default.to_string(), PyValue::py_str)
        };
        let console = pick("console_url", DEFAULT_CONSOLE);
        let home = crate::config::home().to_string_lossy().to_string();
        let token_file = pyos::expanduser(&pick("token_file", DEFAULT_TOKEN_FILE), &home);
        if let Some(prob) = console_problem(&console) {
            return Ok(check(false, format!("params.console_url {prob}: not checked")));
        }
        if let Some(prob) = token_file_problem(&token_file, &self.token_dir()) {
            return Ok(check(false, format!("params.token_file {prob}: not checked")));
        }
        let script = self.deliver_script();
        if !script.is_file() {
            return Ok(check(
                false,
                "the delivery script is not installed next to the hub: not checked".into(),
            ));
        }
        let python = match self.interpreter() {
            Ok(p) => p,
            Err(e) => return Ok(check(false, format!("{e}: not checked"))),
        };
        let argv: Vec<String> = vec![
            python,
            script.to_string_lossy().to_string(),
            "list".into(),
            "--console".into(),
            console,
            "--token-file".into(),
            token_file,
        ];
        let Ok((rc, out, err)) = run.run(&argv, 30.0) else {
            return Ok(check(false, "could not run the delivery script: unverified".into()));
        };
        if rc != 0 {
            return Ok(check(
                false,
                format!("listing skills failed: {}", text::take_chars(text::strip(&err), 200)),
            ));
        }
        let Ok(listed) = u::json_loads(&out) else {
            return Ok(check(false, "the delivery script printed no JSON: unverified".into()));
        };
        let hub = listed_set(&listed, "hub_skills")?;
        let unsafe_ = listed_set(&listed, "unsafe")?;
        let has = |set: &[PyValue], s: &str| set.iter().any(|x| x.as_str() == Some(s));
        let flipped: Vec<&str> = skills.iter().map(String::as_str).filter(|s| has(&unsafe_, s)).collect();
        if !flipped.is_empty() {
            let shown: Vec<&str> = flipped.iter().take(10).copied().collect();
            return Ok(check(
                false,
                format!(
                    "auto_approve or is_default was switched on in Turnstone for: {} (the next push resets it)",
                    shown.join(", ")
                ),
            ));
        }
        let missing: Vec<&str> = skills.iter().map(String::as_str).filter(|s| !has(&hub, s)).collect();
        if !missing.is_empty() {
            let shown: Vec<&str> = missing.iter().take(10).copied().collect();
            return Ok(check(
                false,
                format!(
                    "Turnstone has no hub-delivered skill for {} staged skill(s): {} (run deploy/turnstone-deliver.py \
                     push)",
                    missing.len(),
                    shown.join(", ")
                ),
            ));
        }
        Ok(check(
            true,
            format!("Turnstone lists all {} staged skill(s) as hub-delivered", skills.len()),
        ))
    }
}

impl ClientAdapter for TurnstoneAdapter {
    fn id(&self) -> &str {
        ID
    }

    fn display_name(&self) -> &str {
        "Turnstone"
    }

    fn docs(&self) -> &str {
        "" // the Python class has no docstring
    }

    fn caps(&self) -> AdapterCaps {
        AdapterCaps {
            skills: true,
            agents: false,
            instructions: true,
            mcp: true,
            permissions: false,
            egress: false,
            memory: false,
            tool_map: IndexMap::new(),
            model_tiers: IndexMap::new(),
            notes: "Renders to a hub-owned stage dir; delivery into Turnstone's database is the opt-in \
                    deploy/turnstone-deliver.py (admin API). Personas, governance policies and model selection stay \
                    in Turnstone."
                .into(),
        }
    }

    fn default_config(&self) -> ClientConfig {
        ClientConfig {
            id: "turnstone".into(),
            adapter: ID.into(),
            display_name: "Turnstone".into(),
            description: "Turnstone. Renders a stage tree; deploy/turnstone-deliver.py pushes it through the admin API."
                .into(),
            roots: [("stage".to_string(), "~/path/to/hub/out/turnstone".to_string())].into_iter().collect(),
            params: PyValue::Dict(vec![
                (PyValue::str("manage"), flags_value(&DEFAULT_MANAGE)),
                (PyValue::str("console_url"), PyValue::str(DEFAULT_CONSOLE)),
                (PyValue::str("token_file"), PyValue::str(DEFAULT_TOKEN_FILE)),
            ]),
            strict: true,
            icon: Some("box".into()),
            ..ClientConfig::default()
        }
    }

    fn render(&self, ctx: &RenderContext) -> AdapterResult<RenderResult> {
        let cfg = &ctx.client;
        let mut diags = Vec::new();
        let mut arts = u::skill_artifacts(&ctx.skills, "stage", "skills", &mut diags, "SKILL.md")?;
        for s in &ctx.skills {
            let md = s.files.get("SKILL.md").cloned().unwrap_or_default();
            if md.len() > MAX_SKILL_MD {
                diags.push(u::diag(
                    "error",
                    "skill_too_large",
                    format!("SKILL.md is {} bytes; Turnstone accepts at most {MAX_SKILL_MD}", md.len()),
                    Some(&s.name),
                ));
            }
            let skipped = undelivered_resources(&s.files);
            if !skipped.is_empty() {
                let shown: Vec<String> = skipped.iter().take(6).map(|(p, why)| format!("{p} ({why})")).collect();
                let more = if skipped.len() > 6 {
                    format!("; and {} more", skipped.len() - 6)
                } else {
                    String::new()
                };
                diags.push(u::diag(
                    "warn",
                    "resources_not_delivered",
                    format!(
                        "{} file(s) are staged but will not reach Turnstone (the delivery script skips them): {}{more}",
                        skipped.len(),
                        shown.join("; ")
                    ),
                    Some(&s.name),
                ));
            }
            let (fm, _) = u::parse_frontmatter(&u::decode_replace(&md))?;
            let set = |k: &str| fm.get(k).is_some_and(u::truthy);
            if set("allowed-tools") || set("allowed_tools") {
                diags.push(u::diag(
                    "warn",
                    "allowed_tools_dropped",
                    "allowed-tools is not delivered to Turnstone (with auto_approve it would let the listed tools run \
                     unasked); Turnstone's approval gates apply",
                    Some(&s.name),
                ));
            }
            if set("auto_approve") || set("auto-approve") {
                diags.push(u::diag(
                    "info",
                    "auto_approve_ignored",
                    "the delivery script always creates skills with auto_approve off",
                    Some(&s.name),
                ));
            }
        }
        let staged: BTreeSet<String> = arts
            .iter()
            .filter(|a| a.path.starts_with("skills/"))
            .filter_map(|a| a.path.split('/').nth(1).map(str::to_string))
            .collect();
        let meta = Self::skill_meta(ctx, &staged, &mut diags);
        if !meta.is_empty() {
            let ids = u::sorted_set(meta.keys().cloned());
            let mut a = Artifact::new("stage", META_FILE, u::dump_json(&Value::Object(meta)), "skill");
            a.source_ids = ids;
            arts.push(a);
        }
        if !ctx.agents.is_empty() {
            diags.push(u::diag(
                u::unsupported_severity(cfg),
                "unsupported_capability",
                "Turnstone has no per-agent definitions the hub can map (personas stay in Turnstone); agents are not \
                 rendered",
                None,
            ));
        }
        if !ctx.memory.is_empty() {
            diags.push(u::diag(
                u::unsupported_severity(cfg),
                "unsupported_capability",
                "memory is not rendered for Turnstone",
                None,
            ));
        }
        if !ctx.permissions.rules.is_empty() {
            diags.push(u::diag(
                "info",
                "permissions_not_rendered",
                "Turnstone's own approval gates and governance policies apply; hub rules are not rendered",
                None,
            ));
        }
        if !ctx.instructions.is_empty() {
            let content = format!("{}\n", u::render_instructions(&ctx.instructions, true));
            let mut a = Artifact::new("stage", "instructions.md", u::to_bytes(&content), "instruction");
            a.source_ids = u::sorted_instructions(&ctx.instructions)
                .iter()
                .map(|i| i.id.clone())
                .collect();
            arts.push(a);
        }
        if !ctx.mcp_servers.is_empty() {
            let servers = Self::mcp(&ctx.mcp_servers, &mut diags);
            let ids = u::sorted_set(servers.keys().cloned());
            let mut a = Artifact::new(
                "stage",
                "mcp.json",
                u::dump_json(&json!({"mcpServers": Value::Object(servers)})),
                "mcp",
            );
            a.source_ids = ids;
            arts.push(a);
        }
        arts.sort_by(|a, b| (&a.root, &a.path).cmp(&(&b.root, &b.path)));
        Ok(RenderResult {
            artifacts: arts,
            diagnostics: diags,
        })
    }

    /// Turnstone's real content lives in its database; import reads only the hub's own stage.
    fn discover(&self, cfg: &ClientConfig, fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent> {
        let mut res = DiscoveredContent {
            notes: vec!["Turnstone keeps skills in its database; import reads only the hub stage dir".into()],
            ..DiscoveredContent::default()
        };
        res.skills = u::discover_skills(fs, cfg, "stage", "skills", &mut res.notes)?;
        Ok(res)
    }

    fn verify(
        &self,
        cfg: &ClientConfig,
        expected: &[Expected],
        fs: &dyn FileSystem,
        run: &dyn CommandRunner,
    ) -> AdapterResult<Vec<Check>> {
        let mut checks = u::verify_expected(cfg, expected, fs)?;
        let names = u::sorted_set(
            expected
                .iter()
                .filter(|e| e.kind == "skill")
                .filter_map(|e| e.source_ids.first().cloned()),
        );
        checks.push(self.ask_console(cfg, &names, run)?);
        Ok(checks)
    }
}

/// The registry entry (Python: module attribute `ADAPTER`).
pub fn adapter() -> Option<Result<Arc<dyn ClientAdapter>, String>> {
    Some(Ok(Arc::new(TurnstoneAdapter::default())))
}

/// Where the interpreter for `deploy/turnstone-deliver.py` may come from when `HUB_PYTHON` is not set.
pub const SYSTEM_PYTHONS: [&str; 2] = ["/usr/bin/python3", "/bin/python3"];

/// The Python interpreter that runs `deploy/turnstone-deliver.py` (DEVIATION D-TURNSTONE, audit F6): `HUB_PYTHON` from
/// the hub's process environment (an absolute path to an executable file), else `/usr/bin/python3` or
/// `/bin/python3`. Never a PATH lookup: a directory the user can write, such as `~/.local/bin`, is often first on PATH,
/// and a `python3` placed there would run with the hub's rights.
pub fn interpreter() -> Result<String, String> {
    interpreter_from(std::env::var("HUB_PYTHON").ok(), &SYSTEM_PYTHONS)
}

/// [`interpreter`] for a given `HUB_PYTHON` value and fallback list.
pub fn interpreter_from(configured: Option<String>, fallbacks: &[&str]) -> Result<String, String> {
    if let Some(p) = configured.filter(|p| !p.is_empty()) {
        return if is_absolute_executable(&p) {
            Ok(p)
        } else {
            Err("HUB_PYTHON must be an absolute path to an executable file".into())
        };
    }
    fallbacks
        .iter()
        .find(|c| is_absolute_executable(c))
        .map(|c| (*c).to_string())
        .ok_or_else(|| "python3 was not found in /usr/bin or /bin (set HUB_PYTHON to its absolute path)".into())
}

fn is_absolute_executable(p: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    !p.contains('\0')
        && Path::new(p).is_absolute()
        && std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
