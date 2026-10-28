//! Port of the WS-B2 row of backend/tests/core/test_mcp_gate.py (I1: merged files verify on the hub-owned slice).
//! The other rows of that Python file belong to WS-B1. Needs WS-B1 (content store, resolver) and WS-F (floor MCP checks).

mod b2_fakes;

use b2_fakes::*;
use serde_json::{Value, json};

const BASE: &str =
    r#"{"transport": "stdio", "command": "srv", "args": ["--x"], "sandbox_profile": "srt", "scan_status": "clean"}"#;

#[test]
fn test_merged_files_verify_on_the_managed_slice() -> R {
    let f = hub()?;
    f.add_client(&f.home, "fake1", json!({"manage": {"mcp": true, "permissions": true}}))?;
    f.put("mcp", "srv", serde_json::from_str(BASE)?)?;
    f.put(
        "instruction",
        "style",
        json!({"title": "S", "order": 1, "body": "Be concise.\n"}),
    )?;
    f.hub.work.put_profile(
        "fake1",
        &py(json!({"enable": {"mcp": ["srv"], "instructions": ["style"]}})),
    )?;
    std::fs::write(f.home.join(".mcp.json"), json!({"other": 1}).to_string())?;
    std::fs::write(f.home.join("CLAUDE.md"), "mine\n")?;
    f.commit()?;
    let plan = f.hub.plan(None, "committed")?;
    let res = f.hub.apply(&plan.id, true, &[], "t")?["results"][0].clone();
    assert!(
        res["status"] == "applied" && res["verify_ok"] == json!(true),
        "{}",
        res["checks"]
    );
    assert_eq!(f.hub.verify("fake1")?["ok"], json!(true));
    let mut doc: Value = serde_json::from_str(&read(&f.home.join(".mcp.json")))?;
    doc["mcpServers"]["srv"]["command"] = json!("tampered");
    std::fs::write(f.home.join(".mcp.json"), doc.to_string())?;
    assert_eq!(f.hub.verify("fake1")?["ok"], json!(false));
    doc["mcpServers"]["srv"]["command"] = json!("srv");
    doc["other"] = json!(2); // an UNMANAGED edit does not fail verify
    std::fs::write(f.home.join(".mcp.json"), doc.to_string())?;
    let claude = read(&f.home.join("CLAUDE.md"));
    let rest = claude.split_once("mine\n").map(|x| x.1).unwrap_or_default();
    std::fs::write(f.home.join("CLAUDE.md"), format!("mine, edited\n{rest}"))?;
    assert_eq!(f.hub.verify("fake1")?["ok"], json!(true));
    Ok(())
}
