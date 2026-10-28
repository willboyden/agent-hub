//! Port of backend/tests/core/test_floor.py (security control S-FLOOR), plus the Rust-native extras of WS-F:
//! every floor decision replayed against a Python-generated golden table (tests/golden/floor_decisions.json, made by
//! tests/golden/gen_floor_golden.py) and proptests whose oracle is that table.
//!
//! The floor expands `~` with the home directory, so the golden tests re-run themselves in a child process with
//! HOME=/home/goldenuser (the generator's HOME); the ported Python tests use the test process's own HOME, like pytest.

mod support_wsf;

use std::collections::BTreeSet;
use std::path::PathBuf;

use agent_hub::adapters::base::{McpItem, PermissionSet, Rule};
use agent_hub::services::floor::{
    Floor, FloorPolicy, ToolPolicy, host_matches, load_floor, norm_path, overlaps, url_hostname,
};
use hub_core::models::Finding;
use hub_core::value::PyValue;
use indexmap::IndexMap;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use serde_json::{Value, json};
use support_wsf::{E, R, home, p, real_floor, rule};

fn codes(fs: &[Finding]) -> BTreeSet<String> {
    fs.iter().map(|f| f.code.clone()).collect()
}

fn mcp(name: &str, transport: &str) -> McpItem {
    McpItem {
        name: name.into(),
        transport: transport.into(),
        scan_status: "unscanned".into(),
        ..McpItem::default()
    }
}

#[test]
fn test_floor_file_loads_and_is_strict() -> R {
    let f = load_floor(&agent_hub::config::app_root().join("policy").join("floor.yaml")).map_err(p)?;
    assert!(f.network_default == "deny" && f.deny_path_read.contains(&"~/.aws/**".to_string()));
    let d = tempfile::tempdir()?;
    let bad = d.path().join("floor.yaml");
    for text in ["", "version: 1\nunknown_key: 1\n", "{{{"] {
        std::fs::write(&bad, text)?;
        let e = load_floor(&bad).err().ok_or("a bad floor must not load")?;
        assert_eq!((e.status, e.code.as_str()), (503, "floor_unavailable"), "{text:?}");
    }
    let e = load_floor(&d.path().join("missing.yaml")).err().ok_or("fails CLOSED")?;
    assert_eq!(e.status, 503);
    Ok(())
}

#[test]
fn test_allow_read_of_secrets_is_a_violation() -> R {
    if !support_wsf::reexec(
        "test_allow_read_of_secrets_is_a_violation",
        &[("HOME", support_wsf::FIXED_HOME)],
    )? {
        return Ok(()); // it ran, and passed, in a child process with HOME = FIXED_HOME
    }
    let floor = real_floor()?;
    let h = home().to_string_lossy().to_string();
    let aws = format!("{h}/.aws/credentials");
    for m in [
        "~/.ssh/id_ed25519",
        "~/.ssh/**",
        aws.as_str(),
        "~/**",
        "**",
        "/home/**",
        "~/.config/gcloud/*",
        "~/.gnupg/",
        "~/.netrc",
        "~/.mozilla/firefox/x/logins.json",
        "/proj/.env",
        "~/.config/google-chrome/Default/Cookies",
    ] {
        let f = floor
            .check_rule(&rule("path_read", m, "allow"), "permissions", Some("x"))
            .map_err(p)?;
        let f = f.ok_or(format!("{m} must be refused"))?;
        assert!(f.code == "floor_path_read" && f.severity == "error", "{m}");
    }
    Ok(())
}

#[test]
fn test_allow_read_elsewhere_is_fine() -> R {
    if !support_wsf::reexec(
        "test_allow_read_elsewhere_is_fine",
        &[("HOME", support_wsf::FIXED_HOME)],
    )? {
        return Ok(()); // it ran, and passed, in a child process with HOME = FIXED_HOME
    }
    let floor = real_floor()?;
    for m in ["~/src/**", "/work/repo/**", "~/.config/opencode/**", "/tmp/**"] {
        assert_eq!(
            floor
                .check_rule(&rule("path_read", m, "allow"), "permissions", None)
                .map_err(p)?,
            None,
            "{m}"
        );
    }
    Ok(())
}

#[test]
fn test_stricter_content_is_fine() -> R {
    let floor = real_floor()?;
    for r in [
        rule("path_read", "~/.ssh/**", "deny"),
        rule("path_read", "~/notes/**", "ask"),
        rule("tool", "web_fetch", "deny"),
        rule("tool", "web_fetch", "ask"),
    ] {
        assert_eq!(floor.check_rule(&r, "permissions", None).map_err(p)?, None, "{r:?}");
    }
    Ok(())
}

#[test]
fn test_tool_allow_beyond_max_is_a_violation() -> R {
    let floor = real_floor()?;
    for tool in ["web_fetch", "web_search", "shell"] {
        let f = floor
            .check_rule(&rule("tool", tool, "allow"), "permissions", None)
            .map_err(p)?;
        assert_eq!(f.map(|f| f.code), Some("floor_tool".to_string()), "{tool}");
    }
    Ok(())
}

#[test]
fn test_catch_all_egress_allow_is_a_violation() -> R {
    let floor = real_floor()?;
    for m in ["*", "**", "*.*"] {
        let f = floor
            .check_rule(&rule("egress_host", m, "allow"), "permissions", None)
            .map_err(p)?;
        assert_eq!(f.map(|f| f.code), Some("floor_egress_wildcard".to_string()), "{m}");
    }
    Ok(())
}

#[test]
fn test_shell_allow_needs_a_command_pattern() -> R {
    let floor = real_floor()?;
    for m in ["*", "**", "  ", "?*"] {
        let f = floor
            .check_rule(&rule("command", m, "allow"), "permissions", None)
            .map_err(p)?;
        assert_eq!(f.map(|f| f.code), Some("floor_command_pattern".to_string()), "{m:?}");
        assert_eq!(
            floor
                .check_rule(&rule("command", "git status*", "allow"), "permissions", None)
                .map_err(p)?,
            None
        );
    }
    Ok(())
}

#[test]
fn test_enforce_drops_violations_and_adds_floor_rules() -> R {
    let floor = real_floor()?;
    let res = floor
        .enforce(
            &[
                ("good".into(), rule("path_read", "~/work/**", "allow")),
                ("evil".into(), rule("path_read", "~/.ssh/**", "allow")),
                ("web".into(), rule("tool", "web_fetch", "ask")),
            ],
            None,
        )
        .map_err(p)?;
    assert_eq!(
        res.findings.iter().map(|f| f.item.clone()).collect::<Vec<_>>(),
        vec![Some("evil".to_string())]
    );
    let rules = &res.permissions.rules;
    assert!(
        rules
            .iter()
            .any(|x| x.kind == "path_read" && x.matches == "~/.ssh/**" && x.decision == "deny")
    );
    assert!(!rules.iter().any(|x| x.decision == "allow" && x.matches == "~/.ssh/**"));
    assert!(rules.iter().any(|x| x.matches == "~/work/**"));
    // web_search has no content rule -> the floor default (deny) is emitted; web_fetch's content `ask` replaces it
    assert!(
        rules
            .iter()
            .any(|x| x.kind == "tool" && x.matches == "web_search" && x.decision == "deny")
    );
    assert!(
        !rules
            .iter()
            .any(|x| x.kind == "tool" && x.matches == "web_fetch" && x.decision == "deny")
    );
    assert!(res.permissions.network_default == "deny" && res.permissions.default_tool_decision == "ask");
    Ok(())
}

#[test]
fn test_mcp_needs_clean_scan_and_explicit_egress() -> R {
    let floor = real_floor()?;
    let m = McpItem {
        command: Some("c".into()),
        sandbox_profile: Some("srt".into()),
        egress_hosts: vec!["api.example.com".into()],
        ..mcp("s", "stdio")
    };
    let check = |m: &McpItem, rules: &[Rule]| floor.check_mcp(m, rules, "mcp").map(|f| codes(&f)).map_err(p);
    assert_eq!(
        check(&m, &[])?,
        BTreeSet::from(["mcp_not_clean".into(), "mcp_egress_not_allowed".into()])
    );
    let clean = McpItem {
        scan_status: "clean".into(),
        ..m
    };
    assert!(check(&clean, &[rule("egress_host", "api.example.com", "allow")])?.is_empty());
    assert!(check(&clean, &[rule("egress_host", "*.example.com", "allow")])?.is_empty());
    let both = [
        rule("egress_host", "*.example.com", "allow"),
        rule("egress_host", "api.example.com", "deny"),
    ];
    assert_eq!(check(&clean, &both)?, BTreeSet::from(["mcp_egress_not_allowed".into()]));
    assert_eq!(
        check(&clean, &[rule("egress_host", "other.com", "allow")])?,
        BTreeSet::from(["mcp_egress_not_allowed".into()])
    );
    Ok(())
}

#[test]
fn test_mcp_url_host_must_be_declared() -> R {
    let floor = real_floor()?;
    let m = McpItem {
        url: Some("https://evil.example.net/mcp".into()),
        scan_status: "clean".into(),
        ..mcp("s", "http")
    };
    let fs = floor.check_mcp(&m, &[], "mcp").map_err(p)?;
    assert_eq!(
        fs.iter().map(|f| f.code.as_str()).collect::<Vec<_>>(),
        vec!["mcp_no_egress_declared"]
    );
    let declared = McpItem {
        egress_hosts: vec!["other.example.org".into()],
        ..m.clone()
    };
    assert!(codes(&floor.check_mcp(&declared, &[], "mcp").map_err(p)?).contains("mcp_url_host_undeclared"));
    let local = McpItem {
        url: Some("http://127.0.0.1:9000/mcp".into()),
        ..m
    };
    assert!(floor.check_mcp(&local, &[], "mcp").map_err(p)?.is_empty());
    Ok(())
}

#[test]
fn test_live_permission_set_audit() -> R {
    let floor = real_floor()?;
    let ps = PermissionSet {
        rules: vec![
            rule("path_read", "~/.ssh/**", "allow"),
            rule("tool", "web_fetch", "allow"),
        ],
        default_tool_decision: "allow".into(),
        network_default: "allow".into(),
    };
    assert_eq!(
        codes(&floor.check_permission_set(&ps, "permissions").map_err(p)?),
        BTreeSet::from([
            "floor_path_read".into(),
            "floor_tool".into(),
            "floor_default_tool".into(),
            "floor_network_default".into()
        ])
    );
    let missing = floor
        .missing_denies(&PermissionSet {
            rules: vec![rule("path_read", "~/.ssh/**", "deny")],
            ..PermissionSet::default()
        })
        .map_err(p)?;
    let reads: Vec<String> = missing
        .iter()
        .filter_map(|f| f.rule.clone())
        .filter(|r| r.starts_with("path_read:"))
        .collect();
    assert!(reads.contains(&"path_read:~/.aws/**".to_string()) && !reads.contains(&"path_read:~/.ssh/**".to_string()));
    Ok(())
}

#[test]
fn test_overlap_is_symmetric_and_conservative() {
    assert!(overlaps("~/**", "~/.ssh/**") && overlaps("~/.ssh/**", "~/**"));
    assert!(!overlaps("~/a/**", "~/b/**"));
}

#[test]
fn test_hub_and_knowledge_state_is_unreadable_to_clients() -> R {
    let floor = real_floor()?;
    for m in [
        "~/.config/agent-hub/env",
        "~/.local/share/agent-hub/backups/**",
        "~/.local/share/agent-hub/bootstrap-admin.key",
        "~/.local/share/agent-knowledge/**",
        "~/.config/agent-knowledge/tokens.json",
        "/srv/x/admin.key",
    ] {
        let f = floor
            .check_rule(&rule("path_read", m, "allow"), "permissions", None)
            .map_err(p)?;
        assert_eq!(f.map(|f| f.code), Some("floor_path_read".to_string()), "{m}");
    }
    Ok(())
}

#[test]
fn test_profile_allowing_hub_state_is_a_floor_violation() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let home = support_wsf::client_home(d.path())?;
    support_wsf::add_client(&hub, &home, "fake1", &json!({"manage": {"permissions": true}}))?;
    let data = hub.cfg().data_dir.to_string_lossy().to_string();
    for (i, m) in [
        format!("{data}/backups/**"),
        "~/.local/share/agent-hub/**".into(),
        "~/.config/agent-knowledge/**".into(),
    ]
    .iter()
    .enumerate()
    {
        support_wsf::put(
            &hub,
            "rule",
            &format!("r{i}"),
            json!({"title": "x", "kind": "path_read", "match": m, "decision": "allow"}),
        )?;
    }
    support_wsf::put_profile(&hub, "fake1", json!({"enable": {"rules": ["r0", "r1", "r2"]}}))?;
    support_wsf::commit(&hub)?;
    let plan = support_wsf::plan(&hub, None)?;
    let cp = plan.clients.first().ok_or("one client")?;
    assert!(cp.blocked);
    assert_eq!(
        cp.floor_violations.iter().map(|f| f.code.as_str()).collect::<Vec<_>>(),
        vec!["floor_path_read"; 3]
    );
    let (_, store) = hub.approved().map_err(p)?;
    let res = hub.resolver.resolve(&store.catalog().map_err(p)?, "fake1").map_err(p)?;
    assert!(
        res.permissions
            .rules
            .iter()
            .any(|x| x.decision == "deny" && x.matches.contains(&data))
    );
    Ok(())
}

#[test]
fn test_knowledge_admin_key_file_is_unreadable_to_clients() -> R {
    let floor = real_floor()?;
    for m in [
        "~/.local/share/agent-knowledge/admin.key",
        "~/.local/share/agent-knowledge/**",
        "/anywhere/admin.key",
    ] {
        let f = floor
            .check_rule(&rule("path_read", m, "allow"), "permissions", None)
            .map_err(p)?;
        assert_eq!(f.map(|f| f.code), Some("floor_path_read".to_string()), "{m}");
    }
    assert!(
        floor
            .floor_rules()
            .iter()
            .any(|x| x.kind == "path_read" && x.matches == "~/.local/share/agent-knowledge/**")
    );
    Ok(())
}

// ---- explicit tests of the floor's individual controls ---------------------------------------------------------------

/// S-FLOOR: an `allow` that only overlaps a deny glob through `..`, `.` or `//` is still refused (paths are normalised).
#[test]
fn floor_control_normalised_paths_cannot_dodge_a_deny() -> R {
    let floor = real_floor()?;
    for m in ["~/x/../.aws/credentials", "~/./.ssh/id", "~//.netrc"] {
        assert!(
            floor
                .check_rule(&rule("path_write", m, "allow"), "permissions", None)
                .map_err(p)?
                .is_some(),
            "{m}"
        );
    }
    Ok(())
}

/// S-FLOOR: an empty deny glob that overlaps first is falsy in Python's `if hit:` and hides later ones (kept as is).
#[test]
fn floor_control_empty_deny_glob_quirk_is_kept() {
    let mut pol = policy(&json!({"deny_path_read": ["", "/x/**"]}));
    let floor = Floor::new(pol.clone());
    assert!(
        floor
            .rule_finding(&rule("path_read", "", "allow"), "permissions", None)
            .is_none()
    );
    pol.deny_path_read = vec!["/x/**".into()];
    assert!(
        Floor::new(pol)
            .rule_finding(&rule("path_read", "/x/y", "allow"), "permissions", None)
            .is_some()
    );
}

/// S-FLOOR: a malformed MCP URL is an unexpected error (HTTP 500), never a silently accepted server.
#[test]
fn floor_control_malformed_mcp_url_fails_closed() -> R {
    let floor = real_floor()?;
    let m = McpItem {
        url: Some("http://[::1/mcp".into()),
        scan_status: "clean".into(),
        ..mcp("s", "http")
    };
    let e = floor.check_mcp(&m, &[], "mcp").err().ok_or("must raise")?;
    assert_eq!(e.status, 500);
    assert_eq!(
        url_hostname("http://a\u{2100}b/"),
        Err(agent_hub::services::floor::UrlError(
            "netloc contains invalid characters under NFKC normalization"
        ))
    );
    Ok(())
}

// ---- golden table (Python-generated) ----------------------------------------------------------------------------------

const GOLDEN_HOME: &str = "/home/goldenuser";

fn golden() -> Result<Value, E> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/floor_decisions.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p)?)?;
    if v["home"] != GOLDEN_HOME {
        return Err("golden table made with another HOME".into());
    }
    Ok(v)
}

/// Run only in a process whose HOME is the golden one (re-running the test there otherwise).
fn in_golden_home(name: &str) -> Result<bool, E> {
    if std::env::var("HOME").as_deref() == Ok(GOLDEN_HOME) {
        return Ok(true);
    }
    support_wsf::reexec(name, &[("HOME", GOLDEN_HOME)])
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// A FloorPolicy from its `model_dump(mode="json")`.
fn policy(v: &Value) -> FloorPolicy {
    let d = |k: &str, def: &str| v.get(k).and_then(Value::as_str).unwrap_or(def).to_string();
    let obj = |k: &str| v.get(k).and_then(Value::as_object).cloned().unwrap_or_default();
    FloorPolicy {
        version: v.get("version").and_then(Value::as_i64).unwrap_or(1),
        network_default: d("network_default", "deny"),
        egress: obj("egress").iter().map(|(k, x)| (k.clone(), strs(x))).collect(),
        deny_path_read: v.get("deny_path_read").map(strs).unwrap_or_default(),
        deny_path_write: v.get("deny_path_write").map(strs).unwrap_or_default(),
        tools: obj("tools")
            .iter()
            .map(|(k, t)| {
                (
                    k.clone(),
                    ToolPolicy {
                        default: t.get("default").map_or_else(|| "ask".into(), s),
                        max: t.get("max").map_or_else(|| "ask".into(), s),
                    },
                )
            })
            .collect(),
        default_tool_decision: d("default_tool_decision", "ask"),
        command_allow_forbid_patterns: v.get("command_allow_forbid_patterns").map(strs).unwrap_or_default(),
        command_allow_probes: v.get("command_allow_probes").map(strs).unwrap_or_default(),
        command_allow_warn_probes: v.get("command_allow_warn_probes").map(strs).unwrap_or_default(),
        mcp: obj("mcp")
            .iter()
            .map(|(k, x)| (k.clone(), x.as_bool().unwrap_or(false)))
            .collect(),
        raw: PyValue::from_json(v),
    }
}

fn floors(g: &Value) -> IndexMap<String, Floor> {
    g["floors"]
        .as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), Floor::new(policy(v)))).collect())
        .unwrap_or_default()
}

fn grule(v: &Value) -> Rule {
    Rule {
        kind: s(&v[0]),
        matches: s(&v[1]),
        decision: s(&v[2]),
        reason: v[3].as_str().map(str::to_string),
    }
}

fn rule_v(r: &Rule) -> Value {
    json!([r.kind, r.matches, r.decision, r.reason])
}

fn finding_v(f: &Finding) -> Value {
    json!([f.code, f.severity, f.message, f.item, f.concern, f.rule])
}

fn findings_v(fs: &[Finding]) -> Value {
    Value::Array(fs.iter().map(finding_v).collect())
}

fn cases<'a>(g: &'a Value, key: &str) -> &'a [Value] {
    g[key].as_array().map_or(&[], Vec::as_slice)
}

/// Compare every case, report the first few mismatches with their inputs.
fn expect_all(name: &str, mismatches: &[String], total: usize) -> R {
    if mismatches.is_empty() && total > 0 {
        return Ok(());
    }
    Err(format!(
        "{name}: {} of {total} cases differ from Python; first: {}",
        mismatches.len(),
        mismatches.iter().take(5).cloned().collect::<Vec<_>>().join("\n  ")
    )
    .into())
}

#[test]
fn golden_check_rule_matches_python() -> R {
    if !in_golden_home("golden_check_rule_matches_python")? {
        return Ok(());
    }
    let g = golden()?;
    let fl = floors(&g);
    let mut bad = Vec::new();
    let all = cases(&g, "check_rule");
    for c in all {
        let f = &fl[&s(&c["floor"])];
        let r = grule(&c["rule"]);
        let got = f
            .check_rule(&r, &s(&c["concern"]), c["rid"].as_str())
            .map_err(p)?
            .map_or(Value::Null, |x| finding_v(&x));
        if got != c["finding"] {
            bad.push(format!("{c} -> {got}"));
        }
    }
    expect_all("check_rule", &bad, all.len())
}

#[test]
fn golden_overlaps_norm_path_and_host_matches_match_python() -> R {
    if !in_golden_home("golden_overlaps_norm_path_and_host_matches_match_python")? {
        return Ok(());
    }
    let g = golden()?;
    let mut bad = Vec::new();
    let mut n = 0;
    for c in cases(&g, "overlaps") {
        n += 1;
        if overlaps(&s(&c["a"]), &s(&c["b"])) != c["overlaps"] {
            bad.push(c.to_string());
        }
    }
    for c in cases(&g, "norm_path") {
        n += 1;
        if norm_path(&s(&c["p"])) != s(&c["norm"]) {
            bad.push(c.to_string());
        }
    }
    for c in cases(&g, "host_matches") {
        n += 1;
        if host_matches(&s(&c["pattern"]), &s(&c["host"])) != c["matches"] {
            bad.push(c.to_string());
        }
    }
    expect_all("overlaps/norm_path/host_matches", &bad, n)
}

fn mcp_clean_of(v: &Value) -> Option<IndexMap<String, bool>> {
    v.as_object().map(|m| {
        m.iter()
            .map(|(k, x)| (k.clone(), x.as_bool().unwrap_or(false)))
            .collect()
    })
}

#[test]
fn golden_enforce_matches_python() -> R {
    if !in_golden_home("golden_enforce_matches_python")? {
        return Ok(());
    }
    let g = golden()?;
    let fl = floors(&g);
    let mut bad = Vec::new();
    let all = cases(&g, "enforce");
    for c in all {
        let name = s(&c["floor"]);
        let f = &fl[&name];
        let rules: Vec<(String, Rule)> = c["rules"]
            .as_array()
            .map(|a| a.iter().map(|x| (s(&x[0]), grule(&x[1]))).collect())
            .unwrap_or_default();
        let clean = mcp_clean_of(&c["mcp_clean"]);
        let res = f.enforce(&rules, clean.as_ref()).map_err(p)?;
        let floor_part: Vec<Value> = f.floor_rules().iter().map(rule_v).collect();
        let n = floor_part.len();
        let got = json!({
            "floor_rules": res.permissions.rules[..n.min(res.permissions.rules.len())].iter().map(rule_v).collect::<Vec<_>>(),
            "rules_after_floor": res.permissions.rules[n.min(res.permissions.rules.len())..].iter().map(rule_v).collect::<Vec<_>>(),
            "default_tool_decision": res.permissions.default_tool_decision,
            "network_default": res.permissions.network_default,
            "findings": findings_v(&res.findings),
            "allow_hosts": res.allow_hosts,
            "kept": res.kept.iter().map(rule_v).collect::<Vec<_>>(),
            "kept_ids": res.kept_ids,
        });
        let want = json!({
            "floor_rules": g["floor_rules"][&name],
            "rules_after_floor": c["rules_after_floor"],
            "default_tool_decision": c["default_tool_decision"],
            "network_default": c["network_default"],
            "findings": c["findings"],
            "allow_hosts": c["allow_hosts"],
            "kept": c["kept"],
            "kept_ids": c["kept_ids"],
        });
        if got != want || Value::Array(floor_part) != g["floor_rules"][&name] {
            bad.push(format!("case {} -> {got}", c["case"]));
        }
    }
    expect_all("enforce", &bad, all.len())
}

#[test]
fn golden_check_permission_set_and_missing_denies_match_python() -> R {
    if !in_golden_home("golden_check_permission_set_and_missing_denies_match_python")? {
        return Ok(());
    }
    let g = golden()?;
    let fl = floors(&g);
    let mut bad = Vec::new();
    let all = cases(&g, "permission_set");
    for c in all {
        let f = &fl[&s(&c["floor"])];
        let ps = PermissionSet {
            rules: c["rules"]
                .as_array()
                .map(|a| a.iter().map(grule).collect())
                .unwrap_or_default(),
            default_tool_decision: s(&c["default_tool_decision"]),
            network_default: s(&c["network_default"]),
        };
        let check = findings_v(&f.check_permission_set(&ps, &s(&c["concern"])).map_err(p)?);
        let missing = f.missing_denies(&ps).map_err(p)?;
        let well_formed = missing.iter().all(|m| {
            let r = m.rule.clone().unwrap_or_default();
            let (kind, glob) = r.split_once(':').unwrap_or(("", ""));
            m.code == "floor_deny_missing"
                && m.severity == "warn"
                && m.concern.as_deref() == Some("permissions")
                && m.item.is_none()
                && m.message == format!("live config has no {kind} deny covering {}", hub_core::py::repr(glob))
        });
        let rules: Vec<Value> = missing.iter().map(|m| json!(m.rule)).collect();
        if check != c["check"] || Value::Array(rules) != c["missing_rules"] || !well_formed {
            bad.push(c.to_string());
        }
    }
    expect_all("check_permission_set/missing_denies", &bad, all.len())
}

#[test]
fn golden_check_skill_matches_python() -> R {
    if !in_golden_home("golden_check_skill_matches_python")? {
        return Ok(());
    }
    let g = golden()?;
    let fl = floors(&g);
    let mut bad = Vec::new();
    let all = cases(&g, "check_skill");
    for c in all {
        let f = &fl[&s(&c["floor"])];
        let clean: Option<Vec<String>> = c["clean"].as_array().map(|_| strs(&c["clean"]));
        let got = f
            .check_skill(&s(&c["name"]), &PyValue::from_json(&c["extra"]), clean.as_deref())
            .map_err(p)?;
        if findings_v(&got) != c["findings"] {
            bad.push(format!("{c} -> {}", findings_v(&got)));
        }
    }
    expect_all("check_skill", &bad, all.len())
}

fn gmcp(v: &Value) -> McpItem {
    let o = |k: &str| v[k].as_str().map(str::to_string);
    McpItem {
        name: s(&v["name"]),
        transport: s(&v["transport"]),
        command: o("command"),
        args: strs(&v["args"]),
        url: o("url"),
        env_names: strs(&v["env_names"]),
        sandbox_profile: o("sandbox_profile"),
        egress_hosts: strs(&v["egress_hosts"]),
        scan_status: s(&v["scan_status"]),
        pinned_ref: o("pinned_ref"),
    }
}

#[test]
fn golden_check_mcp_and_url_hostname_match_python() -> R {
    if !in_golden_home("golden_check_mcp_and_url_hostname_match_python")? {
        return Ok(());
    }
    let g = golden()?;
    let fl = floors(&g);
    let mut bad = Vec::new();
    let mut n = 0;
    for c in cases(&g, "check_mcp") {
        n += 1;
        let f = &fl[&s(&c["floor"])];
        let rules: Vec<Rule> = c["rules"]
            .as_array()
            .map(|a| a.iter().map(grule).collect())
            .unwrap_or_default();
        let got = match f.check_mcp(&gmcp(&c["mcp"]), &rules, &s(&c["concern"])) {
            Ok(fs) => json!({"findings": findings_v(&fs)}),
            Err(e) if e.status == 500 => json!({"error": "ValueError"}),
            Err(e) => return Err(p(e)),
        };
        let want = if c.get("error").is_some() {
            json!({"error": c["error"]})
        } else {
            json!({"findings": c["findings"]})
        };
        if got != want {
            bad.push(format!("{c} -> {got}"));
        }
    }
    for c in cases(&g, "url_hostname") {
        n += 1;
        let got = match url_hostname(&s(&c["url"])) {
            Ok(h) => json!({"url": c["url"], "hostname": h}),
            Err(_) => json!({"url": c["url"], "error": "ValueError"}),
        };
        if &got != c {
            bad.push(format!("{c} -> {got}"));
        }
    }
    expect_all("check_mcp/url_hostname", &bad, n)
}

// ---- proptests with the golden table as the oracle --------------------------------------------------------------------

/// The per-rule golden decision of a check_rule case, re-addressed to (concern, rid).
fn readdress(f: &Value, concern: &str, rid: Option<&str>) -> Value {
    match f {
        Value::Array(a) if a.len() == 6 => json!([a[0], a[1], a[2], rid, concern, a[5]]),
        other => other.clone(),
    }
}

/// enforce() over random sets of golden rules equals what the per-rule Python decisions imply: errors dropped and
/// reported, warnings reported and kept, floor rules first, floor tool defaults for unnamed deny-by-default tools,
/// sorted unique allowed egress hosts.
#[test]
fn proptest_enforce_agrees_with_python_per_rule_decisions() -> R {
    if !in_golden_home("proptest_enforce_agrees_with_python_per_rule_decisions")? {
        return Ok(());
    }
    let g = golden()?;
    let fl = floors(&g);
    let by_floor: IndexMap<String, Vec<(Rule, Value)>> = fl
        .keys()
        .map(|name| {
            let rows = cases(&g, "check_rule")
                .iter()
                .filter(|c| c["floor"] == name.as_str())
                .map(|c| (grule(&c["rule"]), c["finding"].clone()))
                .collect();
            (name.clone(), rows)
        })
        .collect();
    let names: Vec<String> = fl.keys().cloned().collect();
    let mut runner = TestRunner::new(Config {
        cases: 400,
        ..Config::default()
    });
    let strategy = (
        0..names.len(),
        proptest::collection::vec(any::<prop::sample::Index>(), 0..12),
    );
    runner
        .run(&strategy, |(fi, picks)| {
            let name = &names[fi];
            let floor = &fl[name];
            let rows = &by_floor[name];
            let chosen: Vec<&(Rule, Value)> = picks.iter().map(|ix| &rows[ix.index(rows.len())]).collect();
            let content: Vec<(String, Rule)> = chosen
                .iter()
                .enumerate()
                .map(|(i, (r, _))| (format!("p{i}"), r.clone()))
                .collect();
            let res = floor
                .enforce(&content, None)
                .map_err(|e| TestCaseError::fail(e.detail))?;
            let mut want_findings = Vec::new();
            let mut want_kept = Vec::new();
            for (i, (r, f)) in chosen.iter().enumerate() {
                let rid = format!("p{i}");
                if !f.is_null() {
                    want_findings.push(readdress(f, "permissions", Some(&rid)));
                }
                if f.is_null() || f[1] != "error" {
                    want_kept.push((rid, r.clone()));
                }
            }
            prop_assert_eq!(findings_v(&res.findings), Value::Array(want_findings));
            prop_assert_eq!(
                &res.kept_ids,
                &want_kept.iter().map(|(i, _)| i.clone()).collect::<Vec<_>>()
            );
            prop_assert_eq!(&res.kept, &want_kept.iter().map(|(_, r)| r.clone()).collect::<Vec<_>>());
            let mut rules = floor.floor_rules();
            let named: BTreeSet<&str> = res
                .kept
                .iter()
                .filter(|r| r.kind == "tool")
                .map(|r| r.matches.as_str())
                .collect();
            for (t, pol) in &floor.p.tools {
                if !named.contains(t.as_str()) && pol.default == "deny" {
                    rules.push(Rule {
                        kind: "tool".into(),
                        matches: t.clone(),
                        decision: "deny".into(),
                        reason: Some("floor default".into()),
                    });
                }
            }
            rules.extend(res.kept.iter().cloned());
            prop_assert_eq!(&res.permissions.rules, &rules);
            let hosts: BTreeSet<String> = res
                .kept
                .iter()
                .filter(|r| r.kind == "egress_host" && r.decision == "allow")
                .map(|r| r.matches.clone())
                .collect();
            prop_assert_eq!(&res.allow_hosts, &hosts.into_iter().collect::<Vec<_>>());
            Ok(())
        })
        .map_err(|e| format!("{e}"))?;
    Ok(())
}

/// check_permission_set() over random live permission sets made of golden rules: one finding per golden decision
/// (re-addressed to the set's concern), then the default-tool and network-default verdicts.
#[test]
fn proptest_permission_set_agrees_with_python_per_rule_decisions() -> R {
    if !in_golden_home("proptest_permission_set_agrees_with_python_per_rule_decisions")? {
        return Ok(());
    }
    let g = golden()?;
    let fl = floors(&g);
    let rows: Vec<(Rule, Value)> = cases(&g, "check_rule")
        .iter()
        .filter(|c| c["floor"] == "real")
        .map(|c| (grule(&c["rule"]), c["finding"].clone()))
        .collect();
    let floor = &fl["real"];
    let mut runner = TestRunner::new(Config {
        cases: 400,
        ..Config::default()
    });
    let dec = prop::sample::select(vec!["allow", "ask", "deny"]);
    let strategy = (
        proptest::collection::vec(any::<prop::sample::Index>(), 0..10),
        dec.clone(),
        dec,
    );
    runner
        .run(&strategy, |(picks, dtd, net)| {
            let chosen: Vec<&(Rule, Value)> = picks.iter().map(|ix| &rows[ix.index(rows.len())]).collect();
            let ps = PermissionSet {
                rules: chosen.iter().map(|(r, _)| r.clone()).collect(),
                default_tool_decision: dtd.into(),
                network_default: net.into(),
            };
            let got = floor
                .check_permission_set(&ps, "live")
                .map_err(|e| TestCaseError::fail(e.detail))?;
            let n = chosen.iter().filter(|(_, f)| !f.is_null()).count();
            let want: Vec<Value> = chosen
                .iter()
                .filter(|(_, f)| !f.is_null())
                .map(|(_, f)| readdress(f, "live", None))
                .collect();
            prop_assert_eq!(findings_v(&got[..n.min(got.len())]), Value::Array(want));
            let order = |d: &str| ["deny", "ask", "allow"].iter().position(|x| *x == d).unwrap_or(2);
            let extra: Vec<&str> = got[n.min(got.len())..].iter().map(|f| f.code.as_str()).collect();
            let mut want_extra = Vec::new();
            if order(dtd) > order(&floor.p.default_tool_decision) {
                want_extra.push("floor_default_tool");
            }
            if net != floor.p.network_default {
                want_extra.push("floor_network_default");
            }
            prop_assert_eq!(extra, want_extra);
            Ok(())
        })
        .map_err(|e| format!("{e}"))?;
    Ok(())
}

proptest! {
    /// overlaps() is symmetric (both directions are tried) and reflexive for any glob over the floor's alphabet.
    #[test]
    fn proptest_overlaps_is_symmetric_and_reflexive(a in "[~/.*?a-c\\[\\]!-]{0,12}", b in "[~/.*?a-c\\[\\]!-]{0,12}") {
        prop_assert_eq!(overlaps(&a, &b), overlaps(&b, &a));
        let n = norm_path(&a);
        prop_assert!(overlaps(&n, &n) || n.is_empty() || n.contains('[') || n.contains('?') || n.contains('*'));
    }

    /// Whatever content rules come in, enforce() never outputs an allow that check_rule refuses as an error.
    #[test]
    fn proptest_enforce_output_never_keeps_an_error_rule(
        rules in proptest::collection::vec(
            (prop::sample::select(vec!["tool", "command", "path_read", "path_write", "egress_host", "mcp_server"]),
             "[~/.*?a-c ]{0,10}",
             prop::sample::select(vec!["allow", "ask", "deny"])),
            0..8)
    ) {
        let floor = real_floor().map_err(|e| TestCaseError::fail(e.to_string()))?;
        let content: Vec<(String, Rule)> = rules.iter().enumerate().map(|(i, (k, m, d))| (format!("r{i}"), rule(k, m, d))).collect();
        let res = floor.enforce(&content, None).map_err(|e| TestCaseError::fail(e.detail))?;
        for r in &res.kept {
            let f = floor.rule_finding(r, "permissions", None);
            prop_assert!(f.is_none_or(|f| f.severity != "error"), "{:?}", r);
        }
        prop_assert!(res.permissions.rules.starts_with(&floor.floor_rules()));
    }
}
