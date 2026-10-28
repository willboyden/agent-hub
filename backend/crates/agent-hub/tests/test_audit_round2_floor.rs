//! Port of the WS-F rows of backend/tests/core/test_audit_round2.py: N2 skill allowed-tools, N6 launcher probes and
//! egress wildcards (S-FLOOR), N4 sensitive targets and the protected app tree (S-TARGETS). The attestation and CLI
//! rows of that file belong to WS-B1/WS-C (named `test_audit_round2.rs` by them; this file carries a suffix so both
//! can exist).

mod support_wsf;

use std::path::PathBuf;

use agent_hub::adapters::base::Artifact;
use agent_hub::config::app_root;
use agent_hub::services::hub::{Hub, HubOptions};
use agent_hub::services::targets::TargetGuard;
use hub_core::value::PyValue;
use indexmap::IndexMap;
use serde_json::{Value, json};
use support_wsf::{FakeAdapter, R, p, real_floor, rule};

fn tools(raw: &str) -> PyValue {
    PyValue::Dict(vec![(PyValue::str("allowed-tools"), PyValue::str(raw))])
}

#[test]
fn test_allowed_tools_split_on_commas_and_whitespace() -> R {
    let floor = real_floor()?;
    let srv = ["srv".to_string()];
    for (raw, code) in [
        ("Read Bash", "skill_unrestricted_tools"),
        ("Read, Bash", "skill_unrestricted_tools"),
        ("Bash(*)", "skill_unrestricted_tools"),
        ("Bash(*:*)", "skill_unrestricted_tools"),
        ("Bash(:*)", "skill_unrestricted_tools"),
        ("Read Grep *", "skill_unrestricted_tools"),
        ("Grep mcp__*", "skill_unrestricted_tools"),
        ("Read mcp__ghost__run", "skill_unscanned_mcp_tool"),
        ("Read Bash(git status:*) Grep", "skill_preapproves_tools"),
        ("Read, Grep", "skill_preapproves_tools"),
        ("Read mcp__srv__search", "skill_preapproves_tools"),
    ] {
        let fs = floor.check_skill("s", &tools(raw), Some(&srv)).map_err(p)?;
        assert_eq!(
            fs.iter().map(|f| f.code.as_str()).collect::<Vec<_>>(),
            vec![code],
            "{raw}"
        );
    }
    Ok(())
}

#[test]
fn test_specific_mcp_tools_need_an_enabled_clean_server() -> R {
    let floor = real_floor()?;
    let first = |v: Vec<hub_core::models::Finding>| v.into_iter().next().ok_or("one finding");
    let srv = ["srv".to_string()];
    assert_eq!(
        first(floor.check_skill("s", &tools("mcp__srv__x"), Some(&srv)).map_err(p)?)?.severity,
        "warn"
    );
    assert_eq!(
        first(floor.check_skill("s", &tools("mcp__srv__x"), Some(&[])).map_err(p)?)?.code,
        "skill_unscanned_mcp_tool"
    );
    let list = PyValue::Dict(vec![(
        PyValue::str("allowed-tools"),
        PyValue::List(vec![PyValue::str("Read")]),
    )]);
    assert_eq!(
        first(floor.check_skill("s", &list, None).map_err(p)?)?.code,
        "skill_preapproves_tools"
    );
    Ok(())
}

#[test]
fn test_general_purpose_launchers_warn() -> R {
    let floor = real_floor()?;
    for m in [
        "git *",
        "node *",
        "npx *",
        "uv run *",
        "uvx *",
        "find *",
        "xargs *",
        "perl *",
        "awk *",
        "env *",
        "nohup *",
        "timeout *",
        "ssh *",
        "curl *",
        "wget *",
    ] {
        let f = floor
            .check_rule(&rule("command", m, "allow"), "permissions", None)
            .map_err(p)?;
        let f = f.ok_or(format!("{m} must warn"))?;
        assert!(f.code == "floor_command_pattern" && f.severity == "warn", "{m}");
        let res = floor
            .enforce(&[("r".into(), rule("command", m, "allow"))], None)
            .map_err(p)?;
        assert!(
            res.permissions.rules.iter().any(|r| r.matches == m),
            "a warning keeps the rule: {m}"
        );
    }
    Ok(())
}

#[test]
fn test_bare_wildcards_are_still_errors_and_specific_patterns_pass() -> R {
    let floor = real_floor()?;
    for m in ["*", "**", "* *"] {
        let f = floor
            .check_rule(&rule("command", m, "allow"), "permissions", None)
            .map_err(p)?;
        assert_eq!(f.map(|f| f.severity), Some("error".to_string()), "{m}");
    }
    for m in [
        "git status*",
        "uv run pytest tests/*",
        "curl https://api.example.com/health",
    ] {
        let f = floor
            .check_rule(&rule("command", m, "allow"), "permissions", None)
            .map_err(p)?;
        assert!(f.is_none_or(|f| f.severity == "warn"), "{m}");
    }
    let res = floor
        .enforce(&[("bare".into(), rule("command", "*", "allow"))], None)
        .map_err(p)?;
    assert!(!res.permissions.rules.iter().any(|r| r.matches == "*"));
    Ok(())
}

#[test]
fn test_tld_wildcard_egress_is_refused() -> R {
    let floor = real_floor()?;
    for host in ["*.io", "*.org", "*.dev", "*.com", "*.net", "*.*"] {
        let f = floor
            .check_rule(&rule("egress_host", host, "allow"), "permissions", None)
            .map_err(p)?;
        let f = f.ok_or(format!("{host} must be refused"))?;
        assert!(f.code == "floor_egress_wildcard" && f.severity == "error", "{host}");
    }
    Ok(())
}

/// The real Hub's guard (Hub::new is CORE; the guard needs no content repo).
#[test]
fn test_whole_app_tree_is_protected_except_out() -> R {
    // the checkout's parent must not contain HOME for "a broad root merely contains the app" to hold (audit F9)
    if !support_wsf::reexec(
        "test_whole_app_tree_is_protected_except_out",
        &[("HOME", support_wsf::FIXED_HOME)],
    )? {
        return Ok(()); // it ran, and passed, in a child process with HOME = FIXED_HOME
    }
    let d = tempfile::tempdir()?;
    let hub = Hub::new(support_wsf::cfg(d.path(), &[])?, HubOptions::default()).map_err(p)?;
    let g = &hub.guard;
    let app = app_root();
    assert!(g.root_problem(&app.join("docs")).is_some() && g.root_problem(&app.join("frontend")).is_some());
    assert_eq!(g.root_problem(&app.join("out").join("example")), None);
    let parent = app.parent().map(PathBuf::from).ok_or("the app root has a parent")?;
    assert_eq!(g.root_problem(&parent), None); // a broad project root merely CONTAINS the app
    assert!(
        g.target_problem(&app.join("docs").join("x.md"), "hub/docs/x.md", false)
            .is_some()
    );
    assert_eq!(
        g.target_problem(&app.join("out").join("example").join("x"), "x", false),
        None
    );
    Ok(())
}

/// (adapter, manage, kind, rel, ok)
type SensitiveCase = (
    &'static str,
    &'static [(&'static str, bool)],
    &'static str,
    &'static str,
    bool,
);

#[test]
fn test_sensitive_targets() {
    let cases: [SensitiveCase; 12] = [
        (
            "claude-code",
            &[("permissions", true)],
            "rule",
            ".claude/settings.json",
            true,
        ),
        (
            "claude-code",
            &[("permissions", false)],
            "rule",
            ".claude/settings.json",
            false,
        ),
        (
            "opencode",
            &[("permissions", true)],
            "rule",
            ".claude/settings.json",
            false,
        ),
        (
            "claude-code",
            &[("permissions", true)],
            "rule",
            ".claude/settings.local.json",
            false,
        ),
        (
            "claude-code",
            &[("instructions", true)],
            "instruction",
            "CLAUDE.md",
            true,
        ),
        (
            "claude-code",
            &[("instructions", true)],
            "instruction",
            "AGENTS.md",
            false,
        ),
        (
            "hermes-agent",
            &[("instructions", true)],
            "instruction",
            "AGENTS.md",
            false,
        ),
        ("opencode", &[("instructions", true)], "instruction", "AGENTS.md", true),
        (
            "opencode",
            &[("instructions", false)],
            "instruction",
            "AGENTS.md",
            false,
        ),
        (
            "generic",
            &[("instructions", true)],
            "instruction",
            "sub/AGENTS.md",
            true,
        ),
        ("generic", &[("skills", true)], "skill", "security/x.sh", false),
        ("claude-code", &[("skills", true)], "skill", ".git/hooks/x", false),
    ];
    for (adapter, manage, kind, rel, ok) in cases {
        let manage: IndexMap<String, bool> = manage.iter().map(|(k, v)| ((*k).to_string(), *v)).collect();
        assert_eq!(
            TargetGuard::sensitive_target(adapter, &manage, kind, rel).is_none(),
            ok,
            "{adapter} {kind} {rel}"
        );
    }
}

#[test]
fn test_sensitive_target_blocks_a_managed_plan_but_not_an_advisory_one() -> R {
    let sneaky = FakeAdapter::scripted("sneaky2", |_ctx| {
        [".claude/settings.json", "security/policy.json", "notes.md"]
            .iter()
            .map(|p| Artifact::new("home", p, b"{}".to_vec(), "rule"))
            .collect()
    });
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub_with(d.path(), &[], vec![sneaky])?;
    let home = support_wsf::client_home(d.path())?;
    let h = home.to_string_lossy().to_string();
    let managed =
        json!({"adapter": "sneaky2", "display_name": "S", "roots": {"home": h}, "manage": {"permissions": true}});
    hub.put_client("sn", &support_wsf::dict(&managed), true).map_err(p)?;
    let advisory = json!({"adapter": "sneaky2", "display_name": "S", "roots": {"home": h}});
    hub.put_client("sn2", &support_wsf::dict(&advisory), true).map_err(p)?;
    support_wsf::commit(&hub)?;
    let plan = support_wsf::plan(&hub, Some(&["sn", "sn2"]))?;
    let (m, a) = (&plan.clients[0], &plan.clients[1]);
    let unsafe_paths = m.diagnostics.iter().filter(|d| d["code"] == "unsafe_path").count();
    assert!(m.blocked && unsafe_paths == 2);
    assert!(!a.blocked);
    assert!(!home.join("security").exists() && !home.join(".claude").exists());
    Ok(())
}

// ---- golden table: sensitive_target against Python --------------------------------------------------------------------

#[test]
fn golden_sensitive_target_matches_python() -> R {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/floor_decisions.json");
    let g: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let cases = g["sensitive_target"].as_array().ok_or("cases")?;
    let mut bad = Vec::new();
    for c in cases {
        let manage: IndexMap<String, bool> = c["manage"]
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_bool().unwrap_or(false)))
                    .collect()
            })
            .unwrap_or_default();
        let s = |k: &str| c[k].as_str().unwrap_or_default().to_string();
        let got = TargetGuard::sensitive_target(&s("adapter"), &manage, &s("kind"), &s("rel"));
        if json!(got) != c["problem"] {
            bad.push(format!("{c} -> {got:?}"));
        }
    }
    assert!(
        bad.is_empty() && !cases.is_empty(),
        "{} differ: {:?}",
        bad.len(),
        bad.first()
    );
    Ok(())
}
