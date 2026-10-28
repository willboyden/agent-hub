//! WS-C part of backend/tests/core/test_cli_registry.py (+ the attest-mcp CLI test of test_audit_round2.py): `hubctl`
//! subcommands, exit codes and the admin-key requirement. The registry/env-file rows of that file are CORE's.
//! Tests that need the content store, git repo or planner are `pending WS-B1/WS-B2` until those land.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod wsc_support;

use agent_hub::cli::run_with;
use serde_json::Value;
use wsc_support::{R, cli, cli_env, hub, put, put_client, put_profile, seed_content};

#[test]
fn test_parser_requires_a_command() {
    let t = hub();
    let mut env = agent_hub::cli::CliEnv {
        environ: std::collections::HashMap::new(),
        env_file: t.no_env_file(),
        stdin_tty: false,
        prompt: Box::new(|_| None),
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(run_with(&[], Some(t.hub.clone()), &mut env, &mut out, &mut err), 2);
    assert!(!err.is_empty());
    // argparse's choices: an unknown tier is a usage error (exit 2)
    let argv: Vec<String> = ["evaluate-skill", "alpha", "--tiers", "tier3"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(run_with(&argv, Some(t.hub.clone()), &mut env, &mut out, &mut err), 2);
    // --select and --subject are mutually exclusive
    let argv: Vec<String> = ["evaluate", "--select", "all", "--subject", "hub:a"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(run_with(&argv, Some(t.hub), &mut env, &mut out, &mut err), 2);
}

/// Python asserts with `validate`/`plan`; the authentication layer runs before every command but init/serve/
/// retire-bootstrap-key, so the success cases use `adapters` (a CORE-only command) until WS-B2 lands `validate`.
#[test]
fn test_cli_requires_an_admin_key() -> R {
    let t = hub();
    let r = cli_env(&t, &["validate"], &[], None);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("admin key"), "{}", r.err);
    assert!(r.err.starts_with("error: unauthorized: "));
    let viewer = t.key("v", "viewer");
    let r = cli_env(&t, &["plan"], &[("HUB_API_KEY", &viewer)], None);
    assert_eq!(r.code, 1, "a viewer key is not enough");
    assert!(r.err.contains("admin key"));
    let r = cli_env(&t, &["plan"], &[("HUB_API_KEY", "hc_bogus")], None);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("admin key"));
    // the 0600 bootstrap file is the fallback
    std::fs::write(t.data().join("bootstrap-admin.key"), t.key("b", "admin"))?;
    let r = cli_env(&t, &["adapters"], &[], None);
    assert_eq!(r.code, 0, "{}", r.err);
    // a key from a hidden prompt when stdin is a terminal (and nothing else is set)
    std::fs::remove_file(t.data().join("bootstrap-admin.key"))?;
    let key = t.key("p", "admin");
    let mut env = agent_hub::cli::CliEnv {
        environ: std::collections::HashMap::new(),
        env_file: t.no_env_file(),
        stdin_tty: true,
        prompt: Box::new(move |p| {
            assert_eq!(p, "Hub admin key (input hidden): ");
            Some(format!("  {key}\n"))
        }),
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_with(&["adapters".to_string()], Some(t.hub), &mut env, &mut out, &mut err);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    Ok(())
}

#[test]
fn test_hub_api_key_is_read_from_the_env_file_and_real_env_wins() -> R {
    let t = hub();
    let good = t.key("f", "admin");
    let envf = t.root.join("envfile");
    std::fs::write(&envf, format!("HUB_API_KEY={good}\n"))?;
    let r = cli_env(&t, &["adapters"], &[], Some(&envf));
    assert_eq!(r.code, 0, "{}", r.err);
    let r = cli_env(&t, &["adapters"], &[("HUB_API_KEY", "hc_wrong")], Some(&envf));
    assert_eq!(r.code, 1, "the real environment wins over the file");
    let all = format!("{}{}", r.out, r.err);
    assert!(!all.contains(&good) && !all.contains("hc_wrong"));
    // `validate` takes the same path (python asserts it exits 1 with the wrong key)
    let r = cli_env(&t, &["validate"], &[("HUB_API_KEY", "hc_wrong")], Some(&envf));
    assert_eq!(r.code, 1);
    Ok(())
}

#[test]
fn test_cli_errors_exit_1() {
    let t = hub();
    let r = cli(&t, &["drift", "--client", "ghost"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("error:"));
    let r = cli(&t, &["import", "--client", "fake1"]);
    assert_eq!(r.code, 1, "needs --all or --item");
    assert_eq!(r.err, "error: pass --all or --item kind:name\n");
}

#[test]
fn test_cli_adapters_lists_and_json() -> R {
    let t = hub();
    let r = cli(&t, &["--json", "adapters"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let v: Value = serde_json::from_str(&r.out)?;
    assert!(v["adapters"].is_array() && v["load_errors"].is_object());
    let r = cli(&t, &["adapters"]);
    assert_eq!(r.code, 0);
    assert!(!r.out.is_empty());
    Ok(())
}

#[test]
fn test_attest_mcp_cli_refuses_without_confirmation() {
    let t = hub();
    let r = cli(&t, &["attest-mcp", "srv"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("does NOT run it") && r.err.contains("scanner") && r.err.contains("'srv'"));
    assert!(r.out.is_empty());
}

#[test]
fn test_cli_init_validate_plan_apply_exit_codes() -> R {
    let t = hub();
    let home = t.root.join("client-home");
    std::fs::create_dir_all(&home)?;
    t.hub.init_content()?;
    put_client(
        &t,
        "fake1",
        &serde_json::json!({"adapter": "claude-code", "display_name": "Fake 1", "roots": {"project": home.to_string_lossy()}}),
    )?;
    seed_content(&t, "fake1")?;
    t.hub.commit("c", "t")?;
    assert_eq!(cli(&t, &["validate"]).code, 0);
    let r = cli(&t, &["--json", "plan"]);
    assert_eq!(r.code, 0);
    let plan: Value = serde_json::from_str(&r.out)?;
    assert_eq!(plan["clients"][0]["client"], "fake1");
    assert_eq!(cli(&t, &["apply"]).code, 1, "never applies without --yes");
    assert!(!home.join("CLAUDE.md").exists());
    let r = cli(&t, &["--json", "apply", "--yes"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let out: Value = serde_json::from_str(&r.out)?;
    assert_eq!(out["results"][0]["status"], "applied");
    assert!(home.join("CLAUDE.md").exists(), "the instruction block was written");
    assert_eq!(cli(&t, &["--json", "verify"]).code, 0);
    assert_eq!(cli(&t, &["--json", "drift"]).code, 0);
    let r = cli(&t, &["plan", "--client", "fake1"]);
    assert_eq!(r.code, 0);
    assert!(r.out.contains("fake1:"));
    Ok(())
}

#[test]
fn test_cli_plan_exit_2_on_violations() -> R {
    let t = hub();
    let home = t.root.join("client-home");
    std::fs::create_dir_all(&home)?;
    t.hub.init_content()?;
    put_client(
        &t,
        "fake1",
        &serde_json::json!({"adapter": "claude-code", "display_name": "Fake 1", "roots": {"project": home.to_string_lossy()},
                            "manage": {"permissions": true}}),
    )?;
    put(
        &t,
        "rule",
        "bad",
        &serde_json::json!({"title": "b", "kind": "path_read", "match": "**", "decision": "allow"}),
    )?;
    put_profile(&t, "fake1", &serde_json::json!({"enable": {"rules": ["bad"]}}))?;
    t.hub.commit("c", "t")?;
    assert_eq!(cli(&t, &["plan"]).code, 2);
    assert_eq!(cli(&t, &["apply", "--yes"]).code, 2);
    assert!(!home.join(".claude/settings.json").exists());
    Ok(())
}

#[test]
fn test_cli_validate_fails_on_bad_content() -> R {
    let t = hub();
    t.hub.init_content()?;
    std::fs::write(t.content().join("agents").join("bad.md"), "---\nx: 1\n---\n")?;
    assert_eq!(cli(&t, &["validate"]).code, 1);
    Ok(())
}

#[test]
fn test_cli_import_adapters_init() -> R {
    let t = hub();
    let home = t.root.join("client-home");
    std::fs::create_dir_all(home.join(".claude/agents"))?;
    std::fs::write(
        home.join(".claude/agents/helper.md"),
        "---\nname: helper\ndescription: h\n---\nbody",
    )?;
    t.hub.init_content()?;
    put_client(
        &t,
        "fake1",
        &serde_json::json!({"adapter": "claude-code", "display_name": "Fake 1", "roots": {"project": home.to_string_lossy()}}),
    )?;
    let r = cli(&t, &["--json", "import", "--client", "fake1", "--all"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let res: Value = serde_json::from_str(&r.out)?;
    assert!(
        res["results"]
            .as_array()
            .is_some_and(|a| a.iter().any(|r| r["kind"] == "agent" && r["status"] == "created"))
    );
    let r = cli(
        &t,
        &[
            "import",
            "--client",
            "fake1",
            "--item",
            "agent:helper",
            "--on-conflict",
            "replace",
        ],
    );
    assert_eq!(r.code, 0);
    let r = cli(&t, &["--json", "adapters"]);
    assert_eq!(r.code, 0);
    t.hub.repo.discard(None, false)?;
    assert_eq!(cli(&t, &["init"]).code, 0, "idempotent");
    Ok(())
}

#[test]
fn test_cli_audit_exit_code() -> R {
    let t = hub();
    let home = t.root.join("client-home");
    std::fs::create_dir_all(home.join(".claude"))?;
    t.hub.init_content()?;
    put_client(
        &t,
        "fake1",
        &serde_json::json!({"adapter": "claude-code", "display_name": "Fake 1", "roots": {"project": home.to_string_lossy()},
                            "manage": {"permissions": true}}),
    )?;
    t.hub.commit("c", "t")?;
    std::fs::write(
        home.join(".claude/settings.json"),
        serde_json::json!({"permissions": {"allow": ["WebFetch"]}}).to_string(),
    )?;
    assert_eq!(cli(&t, &["audit"]).code, 2);
    Ok(())
}

#[test]
fn test_cli_status_and_commit() -> R {
    let t = hub();
    t.hub.init_content()?;
    put(
        &t,
        "instruction",
        "one",
        &serde_json::json!({"title": "One", "body": "x\n"}),
    )?;
    let r = cli(&t, &["status"]);
    assert_eq!(r.code, 0);
    assert!(r.out.contains("instructions/one.md"));
    assert_eq!(cli(&t, &["commit", "-m", "approve one"]).code, 0);
    let r = cli(&t, &["status"]);
    assert_eq!(r.code, 0);
    assert!(r.out.contains("no pending"));
    assert_eq!(cli(&t, &["commit", "-m", "again"]).code, 1, "nothing to commit");
    let h = t.hub.history(50)?;
    assert_eq!(h[0]["message"], "approve one");
    assert_eq!(h[0]["author"], "Agent Hub");
    Ok(())
}

#[test]
fn test_attest_mcp_cli_recomputes_only_after_confirmation() -> R {
    let t = hub();
    t.hub.init_content()?;
    let base = serde_json::json!({"transport": "stdio", "command": "npx", "args": ["-y", "srv@1.0.0"], "scan_status": "unscanned"});
    put(&t, "mcp", "srv", &base)?;
    let r = cli(&t, &["attest-mcp", "srv"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("does NOT run it") && r.err.contains("scanner"));
    let r = cli(&t, &["attest-mcp", "srv", "--yes"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("attested srv"));
    let key = std::fs::read_to_string(t.data().join("attest.key"))?;
    assert!(!r.out.contains(key.trim()));
    assert_eq!(cli(&t, &["attest-mcp", "ghost", "--yes"]).code, 1);
    Ok(())
}
