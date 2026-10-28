//! Port of the WS-F rows of backend/tests/core/test_mcp_gate.py (H2: the floor's MCP gate). The attestation and
//! slice-verify rows of that file belong to WS-B1/WS-B2 (their `test_mcp_gate.rs`; this file carries a suffix so both
//! can exist).

mod support_wsf;

use std::collections::BTreeSet;

use agent_hub::adapters::base::McpItem;
use agent_hub::adapters::base::Rule;
use indexmap::IndexMap;
use serde_json::json;
use support_wsf::{R, p, real_floor, rule};

/// The Python test takes the `hub` fixture only for its setup; the floor it checks is the app's real one.
#[test]
fn test_sandbox_and_egress_declarations_are_required() -> R {
    let floor = real_floor()?;
    let codes = |m: &McpItem| -> Result<BTreeSet<String>, support_wsf::E> {
        Ok(floor
            .check_mcp(m, &[], "mcp")
            .map_err(p)?
            .into_iter()
            .map(|f| f.code)
            .collect())
    };
    let stdio = McpItem {
        name: "a".into(),
        transport: "stdio".into(),
        command: Some("c".into()),
        scan_status: "clean".into(),
        ..McpItem::default()
    };
    assert_eq!(codes(&stdio)?, BTreeSet::from(["mcp_no_sandbox".to_string()]));
    let http = McpItem {
        name: "b".into(),
        transport: "http".into(),
        url: Some("https://x.example.com/mcp".into()),
        scan_status: "clean".into(),
        ..McpItem::default()
    };
    assert_eq!(codes(&http)?, BTreeSet::from(["mcp_no_egress_declared".to_string()]));
    let local = McpItem {
        url: Some("http://127.0.0.1:9/mcp".into()),
        ..http
    };
    assert!(codes(&local)?.is_empty()); // loopback needs none
    Ok(())
}

const BASE: &str =
    r#"{"transport": "stdio", "command": "srv", "args": ["--x"], "sandbox_profile": "srt", "scan_status": "clean"}"#;

#[test]
fn test_mcp_allow_rules_wildcards_always_error_named_only_for_enabled_clean_servers() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let home = support_wsf::client_home(d.path())?;
    support_wsf::add_client(&hub, &home, "fake1", &json!({"manage": {"mcp": true}}))?;
    let base: serde_json::Value = serde_json::from_str(BASE)?;
    support_wsf::put(&hub, "mcp", "srv", base.clone())?;
    support_wsf::put_profile(&hub, "fake1", json!({"enable": {"mcp": ["srv"]}}))?;
    support_wsf::commit(&hub)?;
    let mut shady = base;
    shady["command"] = json!("shady");
    shady["scan_status"] = json!("unscanned");
    support_wsf::put(&hub, "mcp", "shady", shady)?;
    let rules = [
        ("wild", "tool", "mcp__*"),
        ("toolmcp", "tool", "mcp"),
        ("glob", "mcp_server", "sh*"),
        ("shady-named", "mcp_server", "shady"),
        ("shady-tool", "tool", "mcp__shady__run"),
        ("absent", "tool", "mcp__notenabled"),
        ("okname", "mcp_server", "srv"),
        ("oktool", "tool", "mcp__srv__search"),
    ];
    for (rid, kind, m) in rules {
        support_wsf::put(
            &hub,
            "rule",
            rid,
            json!({"title": rid, "kind": kind, "match": m, "decision": "allow"}),
        )?;
    }
    let ids: Vec<&str> = rules.iter().map(|r| r.0).collect();
    support_wsf::put_profile(
        &hub,
        "fake1",
        json!({"enable": {"mcp": ["srv", "shady"], "rules": ids}}),
    )?;
    support_wsf::commit(&hub)?;
    let bad = |hub: &agent_hub::services::hub::Hub| -> Result<BTreeSet<String>, support_wsf::E> {
        let plan = support_wsf::plan(hub, None)?;
        Ok(plan.clients[0]
            .floor_violations
            .iter()
            .filter(|f| f.code == "floor_mcp_rule")
            .filter_map(|f| f.item.clone())
            .collect())
    };
    let want: BTreeSet<String> = ["wild", "toolmcp", "glob", "shady-named", "shady-tool", "absent"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(bad(&hub)?, want); // okname/oktool (clean + enabled) pass
    // a wildcard is an error even when EVERY enabled server is clean (the old behaviour is gone)
    support_wsf::put_profile(
        &hub,
        "fake1",
        json!({"enable": {"mcp": ["srv"], "rules": ["wild", "okname"]}}),
    )?;
    support_wsf::commit(&hub)?;
    assert_eq!(bad(&hub)?, BTreeSet::from(["wild".to_string()]));
    Ok(())
}

/// S-FLOOR (MCP allow rules), runnable now: the same eight rules through `enforce` with srv clean and shady not.
#[test]
fn mcp_allow_rules_through_enforce() -> R {
    let floor = real_floor()?;
    let rules: Vec<(String, Rule)> = [
        ("wild", "tool", "mcp__*"),
        ("toolmcp", "tool", "mcp"),
        ("glob", "mcp_server", "sh*"),
        ("shady-named", "mcp_server", "shady"),
        ("shady-tool", "tool", "mcp__shady__run"),
        ("absent", "tool", "mcp__notenabled"),
        ("okname", "mcp_server", "srv"),
        ("oktool", "tool", "mcp__srv__search"),
    ]
    .iter()
    .map(|(id, k, m)| ((*id).to_string(), rule(k, m, "allow")))
    .collect();
    let clean = IndexMap::from([("srv".to_string(), true), ("shady".to_string(), false)]);
    let res = floor.enforce(&rules, Some(&clean)).map_err(p)?;
    let bad: Vec<String> = res
        .findings
        .iter()
        .filter(|f| f.code == "floor_mcp_rule")
        .filter_map(|f| f.item.clone())
        .collect();
    assert_eq!(
        bad,
        vec!["wild", "toolmcp", "glob", "shady-named", "shady-tool", "absent"]
    );
    assert_eq!(res.kept_ids, vec!["okname", "oktool"]);
    // without the per-client map (None) MCP allow rules are not judged
    assert!(floor.enforce(&rules, None).map_err(p)?.findings.is_empty());
    Ok(())
}
