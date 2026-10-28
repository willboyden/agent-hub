//! Port of backend/tests/core/test_rollback.py: rollback of an apply (restore backups, delete created files, protect
//! later edits), all in a temp dir, through the Hub. Needs WS-B1 (content store, resolver, organizer) and WS-F
//! (contentguard, targets, floor); the CLI half of the last test needs WS-C. The engine-level equivalents run now in
//! src/services/applier_tests.rs.

mod b2_fakes;

use std::sync::{Arc, Mutex};

use agent_hub::services::applier::ApplyHooks;
use b2_fakes::*;
use serde_json::{Value, json};

fn do_apply(f: &Fx, adopt: &[&str]) -> Result<Value, E> {
    let (_, res) = f.run(None, adopt)?;
    Ok(res["results"][0].clone())
}

fn rollback(
    f: &Fx,
    apply_id: Option<&str>,
    confirm: bool,
    dry_run: bool,
    force: &[&str],
) -> hub_core::HubResult<Value> {
    let force: Vec<String> = force.iter().map(|s| (*s).to_string()).collect();
    f.hub.rollback("fake1", apply_id, confirm, dry_run, &force, "t")
}

#[test]
fn test_applies_are_recorded_and_listed() -> R {
    let f = hub()?.setup()?;
    assert_eq!(f.hub.list_applies("fake1")?["items"], json!([]));
    let r = do_apply(&f, &[])?;
    let lst = f.hub.list_applies("fake1")?["items"].clone();
    assert_eq!(lst[0]["apply_id"], r["apply_id"]);
    assert!(lst[0]["plan_id"].as_str().is_some_and(|p| p.starts_with("plan_")));
    assert!(lst[0]["counts"]["add"].as_i64().is_some_and(|n| n >= 4));
    assert!(lst[0]["rolled_back_at"].is_null());
    let (id, plan) = (
        lst[0]["apply_id"].as_str().unwrap_or_default(),
        lst[0]["plan_id"].as_str().unwrap_or("?"),
    );
    assert!(id.ends_with(plan));
    assert!(do_apply(&f, &[])?["apply_id"].is_null()); // a no-op apply records nothing
    assert_eq!(f.hub.list_applies("ghost").err().map(|p| p.status), Some(404));
    Ok(())
}

#[test]
fn test_rollback_restores_bytes_and_removes_added_files() -> R {
    let f = hub()?.setup()?;
    std::fs::create_dir(f.home.join("agents"))?;
    std::fs::write(f.home.join("agents/reviewer.md"), "original hand-written agent\n")?;
    std::fs::write(f.home.join("CLAUDE.md"), "mine\n")?;
    let before = snapshot(&f.home);
    let first = do_apply(&f, &["home:agents/reviewer.md"])?; // changes one file, adds the rest
    assert_ne!(snapshot(&f.home), before);
    let out = rollback(&f, None, true, false, &[])?;
    assert_eq!(out["status"], "rolled_back");
    assert_eq!(out["apply_id"], first["apply_id"]);
    assert_eq!(out["conflicts"], json!([]));
    assert_eq!(snapshot(&f.home), before); // byte-identical, added files gone
    assert!(!f.home.join("skills").exists()); // and the directories they created are pruned
    assert_ne!(out["verify"]["ok"], json!(false));
    assert!(!f.hub.list_applies("fake1")?["items"][0]["rolled_back_at"].is_null());
    assert!(f.hub.locks.load("fake1").ok_or("no lock")?.files.is_empty()); // back to the pre-apply state
    Ok(())
}

#[test]
fn test_later_hand_edit_is_protected_and_force_overrides() -> R {
    let f = hub()?.setup()?;
    do_apply(&f, &[])?;
    std::fs::write(f.home.join("agents/reviewer.md"), "edited after the apply\n")?;
    let dry = rollback(&f, None, false, true, &[])?;
    assert_eq!(dry["status"], "dry_run");
    assert_eq!(
        dry["conflicts"]
            .as_array()
            .map(|a| a.iter().map(|c| c["path"].clone()).collect::<Vec<_>>()),
        Some(vec![json!("agents/reviewer.md")])
    );
    assert!(
        dry["changed"]
            .as_array()
            .ok_or("changed")?
            .iter()
            .all(|c| c.get("diff").is_some())
    );
    assert!(f.home.join("skills/alpha/SKILL.md").exists()); // dry run wrote nothing
    let out = rollback(&f, None, true, false, &[])?;
    assert_eq!(out["status"], "partially_rolled_back");
    assert_eq!(out["conflicts"][0]["path"], "agents/reviewer.md");
    assert_eq!(read(&f.home.join("agents/reviewer.md")), "edited after the apply\n"); // never clobbered
    assert!(!f.home.join("skills").exists()); // the untouched files were rolled back
    let again = rollback(&f, None, true, false, &[])?; // the same conflict, nothing else
    assert_eq!(
        (again["status"].clone(), again["changed"].clone()),
        (json!("conflicts_only"), json!([]))
    );
    let fin = rollback(&f, None, true, false, &["home:agents/reviewer.md"])?; // explicit override finishes the job
    assert_eq!(fin["status"], "rolled_back");
    assert!(!f.home.join("agents").exists());
    Ok(())
}

#[test]
fn test_force_paths_restores_an_edited_file() -> R {
    let f = hub()?.setup()?;
    std::fs::create_dir(f.home.join("agents"))?;
    std::fs::write(f.home.join("agents/reviewer.md"), "v0\n")?;
    do_apply(&f, &["home:agents/reviewer.md"])?;
    std::fs::write(f.home.join("agents/reviewer.md"), "v-edited\n")?;
    let out = rollback(&f, None, true, false, &["home:agents/reviewer.md"])?;
    assert_eq!(out["conflicts"], json!([]));
    assert_eq!(read(&f.home.join("agents/reviewer.md")), "v0\n");
    Ok(())
}

#[test]
fn test_second_rollback_is_a_clear_noop() -> R {
    let f = hub()?.setup()?;
    do_apply(&f, &[])?;
    rollback(&f, None, true, false, &[])?;
    let out = rollback(&f, None, true, false, &[])?;
    assert_eq!(
        (out["status"].clone(), out["changed"].clone()),
        (json!("already_rolled_back"), json!([]))
    );
    assert!(out["message"].as_str().is_some_and(|m| m.contains("nothing to do")));
    Ok(())
}

#[test]
fn test_rollback_requires_confirm_and_a_recorded_apply() -> R {
    let f = hub()?.setup()?;
    let e = rollback(&f, None, false, false, &[]).err().ok_or("rolled back")?;
    assert_eq!((e.status, e.code.as_str()), (422, "confirm_required"));
    assert_eq!(rollback(&f, None, true, false, &[]).err().map(|e| e.status), Some(404));
    // a hostile apply id is a 400
    assert_eq!(
        rollback(&f, Some("../../etc/passwd"), true, false, &[])
            .err()
            .map(|e| e.status),
        Some(400)
    );
    Ok(())
}

#[test]
fn test_older_apply_can_only_be_rolled_back_if_every_file_still_matches() -> R {
    let f = hub()?.setup()?;
    let first = do_apply(&f, &[])?["apply_id"].as_str().ok_or("id")?.to_string();
    std::thread::sleep(std::time::Duration::from_millis(1100)); // apply ids carry a second-resolution stamp
    f.put(
        "agent",
        "reviewer",
        json!({"description": "d", "capabilities": ["read"], "body": "v2\n"}),
    )?;
    f.hub.commit("v2", "t")?;
    let second = do_apply(&f, &[])?["apply_id"].as_str().ok_or("id")?.to_string();
    assert_ne!(first, second);
    let e = rollback(&f, Some(&first), true, false, &[])
        .err()
        .ok_or("rolled back")?;
    assert_eq!(e.code, "rollback_not_latest");
    assert!(f.home.join("skills/alpha/SKILL.md").exists());
    rollback(&f, Some(&second), true, false, &[])?; // newest first
    assert!(read(&f.home.join("agents/reviewer.md")).contains("Review carefully"));
    rollback(&f, Some(&first), true, false, &[])?;
    assert!(!f.home.join("agents").exists() && !f.home.join("skills").exists());
    Ok(())
}

#[test]
fn test_rollback_restores_removed_files_and_uses_safe_writes() -> R {
    let f = hub()?.setup()?;
    do_apply(&f, &[])?;
    f.hub.organizer.toggle("fake1", "agent", "reviewer", false)?;
    f.hub.commit("off", "t")?;
    let r = do_apply(&f, &[])?;
    assert!(!f.home.join("agents/reviewer.md").exists());
    let out = rollback(&f, r["apply_id"].as_str(), true, false, &[])?;
    assert_eq!(out["status"], "rolled_back");
    assert_eq!(
        read(&f.home.join("agents/reviewer.md")),
        "# reviewer\nReview carefully.\n"
    );
    let lock = f.hub.locks.load("fake1").ok_or("no lock")?;
    assert!(serde_json::to_string(&lock)?.contains("reviewer"));
    Ok(())
}

#[test]
fn test_rollback_aborts_atomically_when_a_file_changes_mid_way() -> R {
    let f = hub()?.setup()?;
    do_apply(&f, &[])?;
    let after = snapshot(&f.home);
    let home = f.home.clone();
    let n = Arc::new(Mutex::new(0));
    f.hub.applier.set_test_hooks(Some(ApplyHooks {
        before_commit: Some(Box::new(move |_| {
            let mut c = n.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            *c += 1;
            if *c == 2 {
                let _ = std::fs::write(home.join("agents/reviewer.md"), "late edit\n");
            }
            Ok(())
        })),
        ..ApplyHooks::default()
    }));
    let e = rollback(&f, None, true, false, &[]).err().ok_or("rolled back")?;
    f.hub.applier.set_test_hooks(None);
    assert_eq!(e.code, "rollback_failed");
    let mut got = snapshot(&f.home);
    assert_eq!(got.remove("agents/reviewer.md"), Some(b"late edit\n".to_vec()));
    let mut after = after;
    after.remove("agents/reviewer.md");
    assert_eq!(got, after); // everything else restored to post-apply
    assert!(f.hub.list_applies("fake1")?["items"][0]["rolled_back_at"].is_null());
    Ok(())
}

#[tokio::test]
async fn test_rollback_http_and_cli() -> R {
    let f = hub()?;
    let app = f.api();
    let (admin, viewer) = (f.admin_key()?, f.viewer_key()?);
    let f = f.setup()?;
    let r = do_apply(&f, &[])?;
    let lst = req(&app, "GET", &format!("{API}/clients/fake1/applies"), Some(&admin), None)
        .await
        .json();
    assert_eq!(lst["items"][0]["apply_id"], r["apply_id"]);
    let url = format!("{API}/clients/fake1/rollback");
    assert_eq!(
        req(&app, "POST", &url, Some(&viewer), Some(json!({"confirm": true})))
            .await
            .status,
        403
    );
    assert_eq!(req(&app, "POST", &url, Some(&admin), Some(json!({}))).await.status, 422);
    let d = req(&app, "POST", &url, Some(&admin), Some(json!({"dry_run": true})))
        .await
        .json();
    assert_eq!(d["status"], "dry_run");
    assert!(f.home.join("CLAUDE.md").exists());
    let done = req(&app, "POST", &url, Some(&admin), Some(json!({"confirm": true})))
        .await
        .json();
    assert_eq!(done["status"], "rolled_back");
    assert!(!f.home.join("CLAUDE.md").exists());
    let audit = req(&app, "GET", &format!("{API}/audit"), Some(&admin), None)
        .await
        .json();
    assert!(audit.to_string().contains("rollback"));
    // CLI (Python: HUB_API_KEY=admin, `run([...], hub=hub)`: errors print "error: <code>: <detail>" and exit 1)
    do_apply(&f, &[])?;
    let cli_key = f.hub.auth.create_key("cli", "admin", None, &[])?["secret"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let cli_out = |args: &[&str]| -> (i32, String, String) {
        let mut env = agent_hub::cli::CliEnv {
            environ: std::collections::HashMap::from([("HUB_API_KEY".to_string(), cli_key.clone())]),
            env_file: f.hub.cfg().data_dir.join("no-env-file"),
            stdin_tty: false,
            prompt: Box::new(|_| None),
        };
        let argv: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = agent_hub::cli::run_with(&argv, Some(f.hub.clone()), &mut env, &mut out, &mut err);
        (
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    };
    let (code, _, err) = cli_out(&["rollback", "--client", "fake1"]); // needs --yes
    assert_eq!(code, 1);
    assert!(err.starts_with("error: confirm_required: "), "{err}");
    assert_eq!(cli_out(&["rollback", "--client", "fake1", "--dry-run"]).0, 0);
    assert!(f.home.join("CLAUDE.md").exists());
    let (code, out, _) = cli_out(&["--json", "rollback", "--client", "fake1", "--yes"]);
    assert_eq!(code, 0);
    assert_eq!(serde_json::from_str::<Value>(&out)?["status"], "rolled_back");
    assert!(!f.home.join("CLAUDE.md").exists());
    let (code, out, _) = cli_out(&["applies", "--client", "fake1"]);
    assert!(code == 0 && out.contains("rolled back"), "{out}");
    Ok(())
}
