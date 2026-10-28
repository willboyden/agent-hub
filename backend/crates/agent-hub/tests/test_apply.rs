//! Port of backend/tests/core/test_apply.py: planner + applier, the safety properties of ARCHITECTURE section 4, on real
//! files in temp dirs (fake adapter), through the Hub. These need the content store / resolver / organizer (WS-B1) and
//! the content guard / target guard / floor checks (WS-F); the same properties are proven now on the engine by
//! src/services/applier_tests.rs.

mod b2_fakes;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_hub::adapters::base::{AdapterError, Artifact, RenderResult};
use agent_hub::services::applier::{ApplyHooks, OpError};
use b2_fakes::*;
use serde_json::{Value, json};

fn sha(p: &std::path::Path) -> String {
    hub_core::common::sha256(&std::fs::read(p).unwrap_or_default())
}

// ---- plan is pure ----------------------------------------------------------------------------------------------------

#[test]
fn test_plan_writes_nothing_and_is_bound_to_content_hash() -> R {
    let f = hub()?.setup()?;
    let plan = f.hub.plan(None, "committed")?;
    assert!(snapshot(&f.home).is_empty() && plan.content_hash == f.hub.repo.head_tree()?);
    let a = acts(&plan, "fake1");
    assert_eq!(a["home:skills/alpha/SKILL.md"], "add");
    assert_eq!(a["home:skills/alpha/ref/notes.md"], "add");
    assert_eq!(
        (a["home:agents/reviewer.md"].as_str(), a["home:CLAUDE.md"].as_str()),
        ("add", "add")
    );
    // mcp and permissions are advisory by default: shown, never written
    assert_eq!(
        (a["home:.mcp.json"].as_str(), a["home:settings.json"].as_str()),
        ("advisory", "advisory")
    );
    assert_eq!(f.hub.get_plan(&plan.id)?.id, plan.id);
    Ok(())
}

#[test]
fn test_uncommitted_edits_are_not_in_the_plan() -> R {
    let f = hub()?.setup()?;
    f.put(
        "instruction",
        "style",
        json!({"title": "Style", "order": 10, "body": "CHANGED\n"}),
    )?;
    let plan = f.hub.plan(None, "committed")?;
    assert_eq!(plan.pending_changes, 1);
    assert!(!plan.clients[0].files.iter().any(|x| x.diff.contains("CHANGED")));
    Ok(())
}

#[test]
fn test_apply_delivers_files_lock_and_verifies() -> R {
    let f = hub()?.setup()?;
    let (_, res) = f.run(None, &[])?;
    let r = result(&res, "fake1");
    assert_eq!(
        (r["status"].clone(), r["verify_ok"].clone()),
        (json!("applied"), json!(true))
    );
    assert!(f.runner.n_calls() > 0);
    assert!(read(&f.home.join("skills/alpha/SKILL.md")).starts_with("---\nname: alpha"));
    assert_eq!(
        read(&f.home.join("agents/reviewer.md")),
        "# reviewer\nReview carefully.\n"
    );
    let claude = read(&f.home.join("CLAUDE.md"));
    assert!(claude.starts_with(BEGIN) && claude.contains("Be concise.") && claude.trim_end().ends_with(END));
    assert!(!f.home.join(".mcp.json").exists() && !f.home.join("settings.json").exists());
    let lock_file = f.data("locks/fake1.json");
    assert_eq!(mode_of(&lock_file), 0o600);
    let lock: Value = serde_json::from_str(&read(&lock_file))?;
    let ent = &lock["files"]["home:skills/alpha/SKILL.md"];
    assert_eq!(ent["sha256"], json!(sha(&f.home.join("skills/alpha/SKILL.md"))));
    assert_eq!(ent["created"], json!(true));
    assert_eq!(lock["content_hash"], json!(f.hub.repo.head_tree()?));
    assert_eq!(tmp_files(&f.home), 0);
    Ok(())
}

#[test]
fn test_apply_requires_confirm_and_a_known_plan() -> R {
    let f = hub()?.setup()?;
    let plan = f.hub.plan(None, "committed")?;
    assert_eq!(
        f.hub.apply(&plan.id, false, &[], "t").err().map(|p| p.status),
        Some(422)
    );
    assert_eq!(
        f.hub.apply("plan_nope", true, &[], "t").err().map(|p| p.status),
        Some(404)
    );
    assert_eq!(f.hub.get_plan("../../etc/passwd").err().map(|p| p.status), Some(404));
    Ok(())
}

#[test]
fn test_apply_is_idempotent() -> R {
    let f = hub()?.setup()?;
    f.run(None, &[])?;
    let before = snapshot(&f.home);
    let mt = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mtimes: Vec<_> = all_paths(&f.home)
        .iter()
        .filter(|p| p.is_file())
        .map(|p| (p.clone(), mt(p)))
        .collect();
    let (plan, res) = f.run(None, &[])?;
    assert!(
        acts(&plan, "fake1")
            .values()
            .all(|a| a == "unchanged" || a == "advisory")
    );
    assert_eq!(result(&res, "fake1")["status"], "nothing");
    assert_eq!(snapshot(&f.home), before);
    for (p, m) in mtimes {
        assert_eq!(mt(&p), m, "{}", p.display()); // untouched, not rewritten
    }
    Ok(())
}

#[test]
fn test_edit_shows_change_and_backs_up_previous() -> R {
    let f = hub()?.setup()?;
    f.run(None, &[])?;
    let old = std::fs::read(f.home.join("skills/alpha/SKILL.md"))?;
    f.put(
        "skill",
        "alpha",
        json!({"description": "Alpha v2", "body": "Do alpha v2.\n", "files": {}}),
    )?;
    f.commit()?;
    let plan = f.hub.plan(None, "committed")?;
    assert_eq!(acts(&plan, "fake1")["home:skills/alpha/SKILL.md"], "change");
    let diff = &plan.clients[0]
        .files
        .iter()
        .find(|x| x.path == "skills/alpha/SKILL.md")
        .ok_or("no file")?
        .diff;
    assert!(diff.contains("-description: Alpha skill") && diff.contains("+description: Alpha v2"));
    let res = f.hub.apply(&plan.id, true, &[], "t")?;
    let r = result(&res, "fake1");
    assert!(read(&f.home.join("skills/alpha/SKILL.md")).contains("Alpha v2"));
    let bdir = PathBuf::from(r["backup_dir"].as_str().ok_or("no backup dir")?);
    assert_eq!(std::fs::read(bdir.join("home/skills/alpha/SKILL.md"))?, old);
    // extra file no longer produced -> removed (hub created it, unchanged since)
    assert!(!f.home.join("skills/alpha/ref/notes.md").exists());
    Ok(())
}

#[test]
fn test_plan_diffs_are_redacted() -> R {
    let f = hub()?.setup()?;
    f.put(
        "instruction",
        "leak",
        json!({"title": "Leak", "order": 99, "body": "api_key = sk-abcdefghijklmnopqrstuv\n"}),
    )?;
    f.hub
        .organizer
        .enable_for_client("fake1", "instruction", &["leak".into()])?;
    f.commit()?;
    let plan = f.hub.plan(None, "committed")?;
    let text: String = plan
        .clients
        .iter()
        .flat_map(|c| c.files.iter().map(|x| x.diff.clone()))
        .collect();
    assert!(!text.contains("sk-abcdefghijklmnopqrstuv") && text.contains("[redacted]"));
    Ok(())
}

// ---- stale plans -----------------------------------------------------------------------------------------------------

#[test]
fn test_stale_plan_after_new_commit_is_409() -> R {
    let f = hub()?.setup()?;
    let plan = f.hub.plan(None, "committed")?;
    f.put("instruction", "extra", json!({"title": "E", "body": "e\n"}))?;
    f.commit()?;
    let e = f.hub.apply(&plan.id, true, &[], "t").err().ok_or("applied")?;
    assert_eq!((e.code.as_str(), e.status), ("plan_stale", 409));
    Ok(())
}

#[test]
fn test_plan_goes_stale_when_a_live_file_changes() -> R {
    let f = hub()?.setup()?;
    f.run(None, &[])?;
    f.put(
        "instruction",
        "style",
        json!({"title": "Style", "order": 10, "body": "v2\n"}),
    )?;
    f.commit()?;
    let plan = f.hub.plan(None, "committed")?;
    let claude = read(&f.home.join("CLAUDE.md"));
    std::fs::write(f.home.join("CLAUDE.md"), format!("{claude}\nappended by someone\n"))?;
    std::fs::write(f.home.join("agents/reviewer.md"), "edited by a human\n")?;
    let e = f.hub.apply(&plan.id, true, &[], "t").err().ok_or("applied")?;
    assert_eq!(e.code, "plan_stale");
    Ok(())
}

// ---- all-or-nothing --------------------------------------------------------------------------------------------------

#[test]
fn test_failure_while_committing_rolls_everything_back() -> R {
    let f = hub()?.setup()?;
    f.run(None, &[])?;
    f.put("skill", "alpha", json!({"description": "Alpha v2", "body": "v2\n"}))?;
    f.put(
        "agent",
        "reviewer",
        json!({"description": "Reviews code", "capabilities": ["read"], "body": "v2\n"}),
    )?;
    f.put(
        "instruction",
        "style",
        json!({"title": "Style", "order": 10, "body": "v2\n"}),
    )?;
    f.commit()?;
    let before = snapshot(&f.home);
    let plan = f.hub.plan(None, "committed")?;
    // Python patches os.replace to fail on its third call; here the third commit step fails ("disk full")
    let n = Arc::new(Mutex::new(0));
    f.hub.applier.set_test_hooks(Some(ApplyHooks {
        before_commit: Some(Box::new(move |_| {
            let mut c = n.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            *c += 1;
            if *c == 3 {
                return Err(OpError::new("OSError", "disk full"));
            }
            Ok(())
        })),
        ..ApplyHooks::default()
    }));
    let res = f.hub.apply(&plan.id, true, &[], "t")?;
    f.hub.applier.set_test_hooks(None);
    let r = result(&res, "fake1");
    assert_eq!(r["status"], "failed");
    assert!(r["error"].as_str().unwrap_or_default().contains("nothing was changed"));
    assert_eq!(snapshot(&f.home), before);
    assert_eq!(tmp_files(&f.home), 0);
    // the lock still describes what is on disk: a fresh plan is a normal 'change' plan, not conflicts
    assert!(
        !acts(&f.hub.plan(None, "committed")?, "fake1")
            .values()
            .any(|a| a == "conflict")
    );
    Ok(())
}

#[test]
fn test_failure_while_staging_touches_nothing_and_removes_new_dirs() -> R {
    let f = hub()?.setup()?;
    let plan = f.hub.plan(None, "committed")?;
    let n = Arc::new(Mutex::new(0));
    f.hub.applier.set_test_hooks(Some(ApplyHooks {
        before_stage: Some(Box::new(move |_| {
            let mut c = n.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            *c += 1;
            if *c == 4 {
                return Err(OpError::new("OSError", "boom"));
            }
            Ok(())
        })),
        ..ApplyHooks::default()
    }));
    let res = f.hub.apply(&plan.id, true, &[], "t")?;
    f.hub.applier.set_test_hooks(None);
    assert_eq!(result(&res, "fake1")["status"], "failed");
    assert!(all_paths(&f.home).is_empty(), "not even empty directories");
    assert!(f.hub.locks.load("fake1").is_none());
    Ok(())
}

#[test]
fn test_one_broken_client_does_not_affect_another() -> R {
    let f = hub()?.setup()?;
    let other = f.root.join("other-home");
    std::fs::create_dir(&other)?;
    f.add_client(&other, "fake2", json!({"strict": true}))?;
    f.hub
        .work
        .put_profile("fake2", &py(json!({"enable": {"agents": ["reviewer"]}})))?;
    f.put(
        "agent",
        "browsy",
        json!({"description": "d", "capabilities": ["browser"], "body": "b"}),
    )?;
    f.hub
        .organizer
        .enable_for_client("fake2", "agent", &["browsy".into()])?;
    f.commit()?;
    let (_, res) = f.run(None, &[])?;
    assert_eq!(result(&res, "fake2")["status"], "blocked");
    assert_eq!(result(&res, "fake1")["status"], "applied");
    assert!(snapshot(&other).is_empty());
    Ok(())
}

// ---- conflicts and adoption ------------------------------------------------------------------------------------------

#[test]
fn test_edited_live_file_is_a_conflict_until_adopted() -> R {
    let f = hub()?.setup()?;
    f.run(None, &[])?;
    std::fs::write(f.home.join("agents/reviewer.md"), "hand edited\n")?;
    f.put(
        "agent",
        "reviewer",
        json!({"description": "Reviews code", "capabilities": ["read"], "body": "Review v2.\n"}),
    )?;
    f.commit()?;
    let (plan, res) = f.run(None, &[])?;
    assert_eq!(acts(&plan, "fake1")["home:agents/reviewer.md"], "conflict");
    let r = result(&res, "fake1");
    assert_eq!(r["skipped_conflicts"][0]["path"], "agents/reviewer.md");
    assert_eq!(r["skipped_conflicts"].as_array().map(Vec::len), Some(1));
    assert_eq!(read(&f.home.join("agents/reviewer.md")), "hand edited\n"); // skipped, not clobbered
    let (_, res2) = f.run(None, &["home:agents/reviewer.md"])?;
    assert_eq!(read(&f.home.join("agents/reviewer.md")), "# reviewer\nReview v2.\n");
    let bdir = PathBuf::from(result(&res2, "fake1")["backup_dir"].as_str().ok_or("no backup")?);
    assert_eq!(read(&bdir.join("home/agents/reviewer.md")), "hand edited\n");
    assert!(
        acts(&f.hub.plan(None, "committed")?, "fake1")
            .values()
            .all(|a| a == "unchanged" || a == "advisory")
    );
    Ok(())
}

#[test]
fn test_adopt_can_be_scoped_to_one_client() -> R {
    let f = hub()?.setup()?;
    f.run(None, &[])?;
    std::fs::write(f.home.join("agents/reviewer.md"), "edited\n")?;
    f.put(
        "agent",
        "reviewer",
        json!({"description": "d", "capabilities": ["read"], "body": "v2\n"}),
    )?;
    f.commit()?;
    let (_, res) = f.run(None, &["someone-else:home:agents/reviewer.md"])?;
    assert!(
        !result(&res, "fake1")["skipped_conflicts"]
            .as_array()
            .is_none_or(Vec::is_empty)
    );
    let (_, res) = f.run(None, &["fake1:home:agents/reviewer.md"])?;
    assert!(
        result(&res, "fake1")["skipped_conflicts"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    Ok(())
}

#[test]
fn test_preexisting_foreign_file_is_a_conflict_but_identical_is_adopted_silently() -> R {
    let f = hub()?.setup()?;
    std::fs::create_dir(f.home.join("agents"))?;
    std::fs::write(f.home.join("agents/reviewer.md"), "mine\n")?;
    std::fs::create_dir_all(f.home.join("skills/alpha"))?;
    let (_, store) = f.hub.approved()?;
    let skill = store.load("skill", "alpha")?;
    std::fs::write(
        f.home.join("skills/alpha/SKILL.md"),
        skill.files.get("SKILL.md").ok_or("no SKILL.md")?,
    )?;
    let plan = f.hub.plan(None, "committed")?;
    let a = acts(&plan, "fake1");
    assert_eq!(
        (
            a["home:agents/reviewer.md"].as_str(),
            a["home:skills/alpha/SKILL.md"].as_str()
        ),
        ("conflict", "unchanged")
    );
    let res = f.hub.apply(&plan.id, true, &[], "t")?;
    assert_eq!(read(&f.home.join("agents/reviewer.md")), "mine\n");
    let lock = f.hub.locks.load("fake1").ok_or("no lock")?;
    assert!(
        lock.files.contains_key("home:skills/alpha/SKILL.md") && !lock.files.contains_key("home:agents/reviewer.md")
    );
    assert_eq!(
        lock.files.get("home:skills/alpha/SKILL.md").map(|e| e.created),
        Some(false)
    );
    assert_eq!(result(&res, "fake1")["status"], "applied");
    Ok(())
}

#[test]
fn test_unreadable_live_target_cannot_be_adopted() -> R {
    let f = hub()?.setup()?;
    std::fs::create_dir_all(f.home.join("agents/reviewer.md"))?; // a directory where a file should go
    let plan = f.hub.plan(None, "committed")?;
    assert_eq!(acts(&plan, "fake1")["home:agents/reviewer.md"], "conflict");
    let res = f.hub.apply(&plan.id, true, &["home:agents/reviewer.md".into()], "t")?;
    assert!(f.home.join("agents/reviewer.md").is_dir());
    assert!(
        !result(&res, "fake1")["skipped_conflicts"]
            .as_array()
            .is_none_or(Vec::is_empty)
    );
    Ok(())
}

// ---- merge modes preserve unmanaged content --------------------------------------------------------------------------

#[test]
fn test_json_and_block_merges_preserve_foreign_content() -> R {
    let f = hub()?.full()?;
    std::fs::write(f.home.join("CLAUDE.md"), "# My own notes\n\nkeep me\n")?;
    std::fs::write(
        f.home.join("settings.json"),
        json!({"theme": "dark", "permissions": {"defaultMode": "plan"}}).to_string(),
    )?;
    std::fs::write(
        f.home.join(".mcp.json"),
        json!({"other": {"x": 1}, "mcpServers": {}}).to_string(),
    )?;
    let (_, res) = f.run(None, &[])?;
    assert_eq!(result(&res, "fake1")["status"], "applied");
    let claude = read(&f.home.join("CLAUDE.md"));
    assert!(claude.starts_with(&format!("# My own notes\n\nkeep me\n\n{BEGIN}")) && claude.contains("Be concise."));
    let st: Value = serde_json::from_str(&read(&f.home.join("settings.json")))?;
    assert_eq!(
        (st["theme"].clone(), st["permissions"]["defaultMode"].clone()),
        (json!("dark"), json!("plan"))
    );
    assert!(
        st["permissions"]["deny"]
            .as_array()
            .ok_or("deny")?
            .iter()
            .any(|x| x.as_str().unwrap_or_default().contains(".aws"))
    );
    let mcp: Value = serde_json::from_str(&read(&f.home.join(".mcp.json")))?;
    assert_eq!(mcp["other"], json!({"x": 1}));
    Ok(())
}

#[test]
fn test_block_edit_replaces_only_the_region() -> R {
    let f = hub()?.full()?;
    std::fs::write(f.home.join("CLAUDE.md"), "top\n")?;
    f.run(None, &[])?;
    let text = read(&f.home.join("CLAUDE.md"));
    std::fs::write(
        f.home.join("CLAUDE.md"),
        format!("{text}\nappended by the user after the block\n"),
    )?;
    f.put(
        "instruction",
        "style",
        json!({"title": "Style", "order": 10, "body": "Be VERY concise.\n"}),
    )?;
    f.commit()?;
    f.run(None, &[])?;
    let out = read(&f.home.join("CLAUDE.md"));
    assert!(
        out.starts_with("top\n")
            && out.ends_with("appended by the user after the block\n")
            && out.contains("VERY concise")
    );
    assert_eq!(out.matches(BEGIN).count(), 1);
    Ok(())
}

#[test]
fn test_editing_inside_the_managed_block_is_a_conflict() -> R {
    let f = hub()?.full()?;
    f.run(None, &[])?;
    std::fs::write(
        f.home.join("CLAUDE.md"),
        read(&f.home.join("CLAUDE.md")).replace("Be concise.", "Be chatty."),
    )?;
    f.put(
        "instruction",
        "style",
        json!({"title": "Style", "order": 10, "body": "v2\n"}),
    )?;
    f.commit()?;
    assert_eq!(
        acts(&f.hub.plan(None, "committed")?, "fake1")["home:CLAUDE.md"],
        "conflict"
    );
    Ok(())
}

#[test]
fn test_unparseable_json_is_a_conflict_and_adopting_replaces_it() -> R {
    let f = hub()?.full()?;
    std::fs::write(f.home.join(".mcp.json"), "{ // jsonc comment\n}")?;
    let plan = f.hub.plan(None, "committed")?;
    assert_eq!(acts(&plan, "fake1")["home:.mcp.json"], "conflict");
    let reason = &plan.clients[0]
        .files
        .iter()
        .find(|x| x.path == ".mcp.json")
        .ok_or("no file")?
        .reason;
    assert!(reason.contains("not a mergeable json"));
    f.run(None, &["home:.mcp.json"])?;
    assert_eq!(
        serde_json::from_str::<Value>(&read(&f.home.join(".mcp.json")))?,
        json!({"mcpServers": {}})
    );
    Ok(())
}

#[test]
fn test_broken_block_markers_are_a_conflict() -> R {
    let f = hub()?.full()?;
    std::fs::write(f.home.join("CLAUDE.md"), format!("x\n{BEGIN}\nno end marker\n"))?;
    assert_eq!(
        acts(&f.hub.plan(None, "committed")?, "fake1")["home:CLAUDE.md"],
        "conflict"
    );
    Ok(())
}

#[test]
fn test_mode_is_applied_and_existing_mode_kept_for_merges() -> R {
    use std::os::unix::fs::PermissionsExt;
    let f = hub()?.full()?;
    std::fs::write(f.home.join("settings.json"), "{}")?;
    std::fs::set_permissions(f.home.join("settings.json"), std::fs::Permissions::from_mode(0o600))?;
    f.run(None, &[])?;
    assert_eq!(mode_of(&f.home.join("settings.json")), 0o600);
    assert_eq!(mode_of(&f.home.join("skills/alpha/SKILL.md")), 0o644);
    Ok(())
}

// ---- removals only for lock-owned files ------------------------------------------------------------------------------

#[test]
fn test_disable_removes_only_hub_created_files_and_prunes_dirs() -> R {
    let f = hub()?.setup()?;
    std::fs::create_dir(f.home.join("skills"))?;
    std::fs::write(f.home.join("skills/foreign.txt"), "not mine")?;
    std::fs::create_dir(f.home.join("agents"))?;
    std::fs::write(f.home.join("agents/mine.md"), "hand written agent")?;
    f.run(None, &[])?;
    f.hub.organizer.toggle("fake1", "skill", "alpha", false)?;
    f.hub.organizer.toggle("fake1", "agent", "reviewer", false)?;
    f.commit()?;
    let (plan, res) = f.run(None, &[])?;
    assert_eq!(acts(&plan, "fake1")["home:skills/alpha/SKILL.md"], "remove");
    assert!(!f.home.join("skills/alpha").exists() && !f.home.join("agents/reviewer.md").exists());
    assert_eq!(read(&f.home.join("skills/foreign.txt")), "not mine");
    assert!(f.home.join("agents/mine.md").exists() && f.home.join("skills").is_dir());
    assert!(
        !f.hub
            .locks
            .load("fake1")
            .ok_or("no lock")?
            .files
            .contains_key("home:skills/alpha/SKILL.md")
    );
    let bdir = PathBuf::from(result(&res, "fake1")["backup_dir"].as_str().ok_or("no backup")?);
    assert!(bdir.join("home/agents/reviewer.md").exists());
    Ok(())
}

#[test]
fn test_removal_of_an_edited_hub_file_is_a_conflict() -> R {
    let f = hub()?.setup()?;
    f.run(None, &[])?;
    std::fs::write(f.home.join("agents/reviewer.md"), "edited")?;
    f.hub.organizer.toggle("fake1", "agent", "reviewer", false)?;
    f.commit()?;
    let (plan, _) = f.run(None, &[])?;
    assert_eq!(acts(&plan, "fake1")["home:agents/reviewer.md"], "conflict");
    assert!(f.home.join("agents/reviewer.md").exists());
    f.run(None, &["home:agents/reviewer.md"])?;
    assert!(!f.home.join("agents/reviewer.md").exists());
    Ok(())
}

#[test]
fn test_adopted_preexisting_file_is_released_not_deleted() -> R {
    let f = hub()?.setup()?;
    std::fs::create_dir(f.home.join("agents"))?;
    std::fs::write(f.home.join("agents/reviewer.md"), "# reviewer\nReview carefully.\n")?;
    f.run(None, &[])?;
    f.hub.organizer.toggle("fake1", "agent", "reviewer", false)?;
    f.commit()?;
    let (plan, _) = f.run(None, &[])?;
    assert_eq!(acts(&plan, "fake1")["home:agents/reviewer.md"], "unchanged");
    assert!(f.home.join("agents/reviewer.md").exists());
    assert!(
        !f.hub
            .locks
            .load("fake1")
            .ok_or("no lock")?
            .files
            .contains_key("home:agents/reviewer.md")
    );
    Ok(())
}

#[test]
fn test_managed_slice_removal_preserves_the_rest() -> R {
    let f = hub()?.full()?;
    std::fs::write(f.home.join("CLAUDE.md"), "mine\n")?;
    std::fs::write(f.home.join("settings.json"), json!({"theme": "dark"}).to_string())?;
    f.run(None, &[])?;
    assert!(read(&f.home.join("CLAUDE.md")).contains(BEGIN));
    f.hub.organizer.toggle("fake1", "instruction", "style", false)?;
    f.hub.organizer.toggle("fake1", "rule", "no-web", false)?;
    f.commit()?;
    f.run(None, &[])?;
    assert_eq!(read(&f.home.join("CLAUDE.md")), "mine\n");
    let st: Value = serde_json::from_str(&read(&f.home.join("settings.json")))?;
    assert_eq!(st["theme"], "dark");
    Ok(())
}

#[test]
fn test_hub_created_merge_file_is_deleted_when_empty() -> R {
    let f = hub()?.full()?;
    f.run(None, &[])?;
    assert!(f.home.join("CLAUDE.md").exists());
    f.hub.organizer.toggle("fake1", "instruction", "style", false)?;
    f.hub.organizer.toggle("fake1", "skill", "alpha", false)?; // via collection: goes to the disable list
    f.commit()?;
    f.run(None, &[])?;
    assert!(!f.home.join("CLAUDE.md").exists());
    Ok(())
}

#[test]
fn test_going_advisory_never_deletes() -> R {
    let f = hub()?.full()?;
    f.run(None, &[])?;
    assert!(f.home.join(".mcp.json").exists());
    f.hub.work.put_client(
        "fake1",
        &py(
            json!({"adapter": "fake", "display_name": "F", "roots": {"home": s(&f.home)},
                    "manage": {"mcp": false, "permissions": false}}),
        ),
    )?;
    f.commit()?;
    let (plan, _) = f.run(None, &[])?;
    let a = &acts(&plan, "fake1")["home:.mcp.json"];
    assert!(a == "advisory" || a == "unchanged");
    assert!(f.home.join(".mcp.json").exists());
    Ok(())
}

/// Parametrised in Python over hub_created = True / False; both cases run here.
#[test]
fn test_going_advisory_releases_the_lock_entry() -> R {
    for hub_created in [true, false] {
        let f = hub()?.full()?;
        if !hub_created {
            std::fs::write(f.home.join("CLAUDE.md"), "mine\n")?; // pre-existing: adopted, not created by us
        }
        f.run(None, &[])?;
        assert!(
            f.hub
                .locks
                .load("fake1")
                .ok_or("no lock")?
                .files
                .contains_key("home:CLAUDE.md")
        );
        f.hub.work.put_client(
            "fake1",
            &py(
                json!({"adapter": "fake", "display_name": "F", "roots": {"home": s(&f.home)},
                        "manage": {"mcp": true, "permissions": true, "instructions": false}}),
            ),
        )?;
        f.commit()?;
        std::fs::write(
            f.home.join("CLAUDE.md"),
            "edited by hand after the concern went advisory\n",
        )?;
        let plan = f.hub.plan(None, "committed")?;
        let cp = plan.clients.iter().find(|c| c.client == "fake1").ok_or("no client")?;
        let file = cp.files.iter().find(|x| x.path == "CLAUDE.md").ok_or("no file")?;
        assert_eq!(file.action, "advisory");
        assert!(file.reason.contains("released from the lock"));
        f.hub.apply(&plan.id, true, &[], "t")?;
        assert!(
            !f.hub
                .locks
                .load("fake1")
                .ok_or("no lock")?
                .files
                .contains_key("home:CLAUDE.md")
        );
        assert_eq!(
            read(&f.home.join("CLAUDE.md")),
            "edited by hand after the concern went advisory\n"
        );
        assert_eq!(f.hub.verify("fake1")?["ok"], json!(true));
    }
    Ok(())
}

/// test_apply.py `MixedAdapter`: one json file holding two concerns' slices (like opencode.json: mcp + permissions).
fn mixed() -> FakeAdapter {
    FakeAdapter::subclass("mixed", |_| {
        let mut a = Artifact::new(
            "home",
            "tool.json",
            json!({"mcpServers": {}}).to_string().into_bytes(),
            "mcp",
        );
        a.merge = "json_keys".into();
        a.managed_keys = vec!["mcpServers".into()];
        a.source_ids = vec!["none".into()];
        let mut b = Artifact::new(
            "home",
            "tool.json",
            json!({"permissions": {"deny": ["x"]}}).to_string().into_bytes(),
            "rule",
        );
        b.merge = "json_keys".into();
        b.managed_keys = vec!["permissions".into()];
        b.source_ids = vec!["floor".into()];
        Ok(RenderResult {
            artifacts: vec![a, b],
            diagnostics: vec![],
        })
    })
}

#[test]
fn test_a_file_with_one_concern_still_managed_keeps_its_lock_entry() -> R {
    let f = hub_with(vec![mixed()], true)?;
    let spec = json!({"adapter": "mixed", "display_name": "M", "roots": {"home": s(&f.home)}});
    let with_manage = |m: Value| {
        let mut v = spec.clone();
        v["manage"] = m;
        obj(v)
    };
    f.hub
        .put_client("mix1", &with_manage(json!({"mcp": true, "permissions": true})), true)?;
    f.commit()?;
    f.run(Some(&["mix1"]), &[])?;
    assert!(
        f.hub
            .locks
            .load("mix1")
            .ok_or("no lock")?
            .files
            .contains_key("home:tool.json")
    );
    f.hub
        .put_client("mix1", &with_manage(json!({"mcp": false, "permissions": true})), false)?;
    f.commit()?;
    let (plan, _) = f.run(Some(&["mix1"]), &[])?;
    let file = plan.clients[0]
        .files
        .iter()
        .find(|x| x.path == "tool.json")
        .ok_or("no file")?;
    assert!(!file.reason.contains("released"));
    assert!(
        f.hub
            .locks
            .load("mix1")
            .ok_or("no lock")?
            .files
            .contains_key("home:tool.json")
    );
    Ok(())
}

#[test]
fn test_a_missing_root_blocks_the_plan_with_a_clear_reason() -> R {
    let f = hub()?;
    let gone = f.root.join("not-created");
    f.add_client(&gone, "fake1", json!({"strict": false}))?; // not strict: root_missing must block anyway
    f.seed_content("fake1")?;
    f.commit()?;
    let plan = f.hub.plan(Some(&["fake1".into()]), "committed")?;
    let cp = &plan.clients[0];
    assert!(cp.blocked);
    assert_eq!(cp.diagnostics.iter().filter(|d| d["code"] == "root_missing").count(), 1); // once per root
    assert!(
        cp.blocked_reasons
            .iter()
            .any(|r| r.contains("does not exist") && r.contains("never creates a client root"))
    );
    assert_eq!(
        result(&f.hub.apply(&plan.id, true, &[], "t")?, "fake1")["status"],
        "blocked"
    );
    assert!(!gone.exists());
    std::fs::create_dir(&gone)?;
    assert!(!f.hub.plan(Some(&["fake1".into()]), "committed")?.clients[0].blocked);
    Ok(())
}

#[test]
fn test_a_root_removed_between_plan_and_apply_is_caught() -> R {
    let f = hub()?;
    let root = f.root.join("root");
    std::fs::create_dir(&root)?;
    f.add_client(&root, "fake1", json!({}))?;
    f.seed_content("fake1")?;
    f.commit()?;
    let plan = f.hub.plan(Some(&["fake1".into()]), "committed")?;
    assert!(!plan.clients[0].blocked);
    std::fs::remove_dir(&root)?;
    let e = f.hub.apply(&plan.id, true, &[], "t").err().ok_or("applied")?;
    assert!(e.status == 409 && e.detail.contains("plan again")); // the plan-digest re-check catches it
    assert!(!root.exists());
    let again = f.hub.plan(Some(&["fake1".into()]), "committed")?;
    assert!(again.clients[0].diagnostics.iter().any(|d| d["code"] == "root_missing"));
    Ok(())
}

// ---- path safety in the pipeline -------------------------------------------------------------------------------------

/// test_apply.py `EvilAdapter`.
fn evil(paths: Vec<String>, root: &'static str) -> FakeAdapter {
    FakeAdapter::subclass("evil", move |_| {
        Ok(RenderResult {
            artifacts: paths
                .iter()
                .map(|p| Artifact::new(root, p, b"pwned".to_vec(), "skill"))
                .collect(),
            diagnostics: vec![],
        })
    })
}

/// Parametrised in Python over seven hostile paths; every one runs here.
#[test]
fn test_unsafe_artifact_paths_block_the_plan() -> R {
    for bad in [
        "../escape.txt",
        "/etc/cron.d/x",
        "a/../../b",
        "a\\b",
        "a\0b",
        "sub/../../x",
        "",
    ] {
        let f = hub_with(vec![evil(vec![bad.into()], "home")], true)?;
        f.hub.put_client(
            "evil1",
            &obj(json!({"adapter": "evil", "display_name": "E", "roots": {"home": s(&f.home)}})),
            true,
        )?;
        f.commit()?;
        let plan = f.hub.plan(Some(&["evil1".into()]), "committed")?;
        let cp = &plan.clients[0];
        assert!(
            cp.blocked && cp.diagnostics.iter().any(|d| d["code"] == "unsafe_path"),
            "{bad:?}"
        );
        let res = f.hub.apply(&plan.id, true, &[], "t")?;
        assert_eq!(result(&res, "evil1")["status"], "blocked");
        assert!(!f.root.join("escape.txt").exists() && snapshot(&f.home).is_empty());
    }
    Ok(())
}

#[test]
fn test_unknown_root_blocks() -> R {
    let f = hub_with(vec![evil(vec!["ok.txt".into()], "elsewhere")], true)?;
    f.hub.put_client(
        "evil1",
        &obj(json!({"adapter": "evil", "display_name": "E", "roots": {"home": s(&f.home)}})),
        true,
    )?;
    f.commit()?;
    let plan = f.hub.plan(Some(&["evil1".into()]), "committed")?;
    assert!(plan.clients[0].diagnostics.iter().any(|d| d["code"] == "unknown_root"));
    Ok(())
}

#[test]
fn test_symlink_escape_is_refused_at_plan_and_apply() -> R {
    let f = hub()?;
    let outside = f.root.join("outside");
    std::fs::create_dir(&outside)?;
    std::os::unix::fs::symlink(&outside, f.home.join("skills"))?; // ~/.claude/skills -> somewhere else
    f.add_client(&f.home, "fake1", json!({}))?;
    f.seed_content("fake1")?;
    f.commit()?;
    let plan = f.hub.plan(None, "committed")?;
    let cp = &plan.clients[0];
    assert!(cp.blocked && cp.diagnostics.iter().any(|d| d["code"] == "unsafe_path"));
    f.hub.apply(&plan.id, true, &[], "t")?;
    assert_eq!(std::fs::read_dir(&outside)?.count(), 0); // nothing leaked through the link
    Ok(())
}

#[test]
fn test_symlinked_leaf_inside_root_is_not_written_through() -> R {
    let f = hub()?;
    let target = f.root.join("victim.txt");
    std::fs::write(&target, "victim")?;
    std::fs::create_dir(f.home.join("agents"))?;
    std::os::unix::fs::symlink(&target, f.home.join("agents/reviewer.md"))?;
    f.add_client(&f.home, "fake1", json!({}))?;
    f.seed_content("fake1")?;
    f.commit()?;
    let plan = f.hub.plan(None, "committed")?;
    assert!(plan.clients[0].blocked && plan.clients[0].diagnostics.iter().any(|d| d["code"] == "unsafe_path"));
    f.hub.apply(&plan.id, true, &[], "t")?;
    assert_eq!(read(&target), "victim");
    assert!(f.home.join("agents/reviewer.md").is_symlink());
    Ok(())
}

#[test]
fn test_setuid_modes_are_refused() -> R {
    let modey = FakeAdapter::subclass("modey", |_| {
        let mut a = Artifact::new("home", "x.sh", b"#!/bin/sh".to_vec(), "skill");
        a.mode = 0o4755;
        Ok(RenderResult {
            artifacts: vec![a],
            diagnostics: vec![],
        })
    });
    let f = hub_with(vec![modey], true)?;
    f.hub.put_client(
        "m1",
        &obj(json!({"adapter": "modey", "display_name": "M", "roots": {"home": s(&f.home)}})),
        true,
    )?;
    f.commit()?;
    assert!(f.hub.plan(Some(&["m1".into()]), "committed")?.clients[0].blocked);
    Ok(())
}

// ---- diagnostics, floor, mcp -----------------------------------------------------------------------------------------

#[test]
fn test_strict_client_error_diag_blocks_nonstrict_does_not() -> R {
    let f = hub()?;
    f.put(
        "agent",
        "browsy",
        json!({"description": "d", "capabilities": ["browser"], "body": "b"}),
    )?;
    let other = f.root.join("h2");
    std::fs::create_dir(&other)?;
    f.add_client(&f.home, "strictc", json!({"strict": true}))?;
    f.add_client(&other, "loosec", json!({"strict": false}))?;
    for c in ["strictc", "loosec"] {
        f.hub
            .work
            .put_profile(c, &py(json!({"enable": {"agents": ["browsy"]}})))?;
    }
    f.commit()?;
    let plan = f.hub.plan(None, "committed")?;
    let by = |c: &str| plan.clients.iter().find(|x| x.client == c).map(|x| x.blocked);
    assert_eq!((by("strictc"), by("loosec")), (Some(true), Some(false)));
    f.hub.apply(&plan.id, true, &[], "t")?;
    assert!(!f.home.join("agents/browsy.md").exists() && other.join("agents/browsy.md").exists());
    Ok(())
}

#[test]
fn test_floor_violation_blocks_apply() -> R {
    let f = hub()?;
    f.add_client(&f.home, "fake1", json!({"manage": {"permissions": true}}))?;
    f.put(
        "rule",
        "read-everything",
        json!({"title": "bad", "kind": "path_read", "match": "~/**", "decision": "allow"}),
    )?;
    f.hub.work.put_profile(
        "fake1",
        &py(json!({"enable": {"rules": ["read-everything"], "instructions": []}})),
    )?;
    f.commit()?;
    let plan = f.hub.plan(None, "committed")?;
    let cp = &plan.clients[0];
    assert!(cp.blocked);
    assert_eq!(cp.floor_violations[0].code, "floor_path_read");
    assert_eq!(cp.floor_violations[0].item.as_deref(), Some("read-everything"));
    let res = f.hub.apply(&plan.id, true, &[], "t")?;
    assert_eq!(result(&res, "fake1")["status"], "blocked");
    assert!(snapshot(&f.home).is_empty());
    let st = cp
        .files
        .iter()
        .find(|x| x.path == "settings.json")
        .ok_or("no settings.json")?;
    assert!(!st.diff.contains("read-everything")); // the bad allow is never rendered
    Ok(())
}

#[test]
fn test_floor_violation_on_advisory_concern_is_only_a_warning() -> R {
    let f = hub()?;
    f.add_client(&f.home, "fake1", json!({}))?; // permissions advisory (default)
    f.put(
        "rule",
        "read-everything",
        json!({"title": "bad", "kind": "path_read", "match": "~/**", "decision": "allow"}),
    )?;
    f.hub
        .work
        .put_profile("fake1", &py(json!({"enable": {"rules": ["read-everything"]}})))?;
    f.commit()?;
    let cp = f.hub.plan(None, "committed")?.clients.remove(0);
    assert!(!cp.blocked && cp.floor_violations[0].severity == "warn");
    Ok(())
}

#[test]
fn test_mcp_enable_requires_clean_scan_and_egress_rule() -> R {
    let f = hub()?;
    f.add_client(&f.home, "fake1", json!({"manage": {"mcp": true}}))?;
    let srv = |scan: &str| {
        json!({"transport": "stdio", "command": "srv", "sandbox_profile": "srt", "egress_hosts": ["api.example.com"],
               "scan_status": scan})
    };
    f.put("mcp", "search", srv("unscanned"))?;
    f.hub
        .work
        .put_profile("fake1", &py(json!({"enable": {"mcp": ["search"]}})))?;
    f.commit()?;
    let codes = |f: &Fx| -> Result<std::collections::BTreeSet<String>, E> {
        Ok(f.hub.plan(None, "committed")?.clients[0]
            .floor_violations
            .iter()
            .map(|x| x.code.clone())
            .collect())
    };
    let cp = f.hub.plan(None, "committed")?.clients.remove(0);
    assert!(cp.blocked);
    assert_eq!(
        codes(&f)?,
        ["mcp_egress_not_allowed", "mcp_not_clean"].map(String::from).into()
    );
    f.put("mcp", "search", srv("clean"))?;
    f.commit()?;
    assert_eq!(codes(&f)?, ["mcp_egress_not_allowed"].map(String::from).into());
    f.put(
        "rule",
        "allow-api",
        json!({"title": "api", "kind": "egress_host", "match": "api.example.com", "decision": "allow"}),
    )?;
    f.hub.work.put_profile(
        "fake1",
        &py(json!({"enable": {"mcp": ["search"], "rules": ["allow-api"]}})),
    )?;
    f.commit()?;
    let (plan, _) = f.run(None, &[])?;
    assert!(!plan.clients[0].blocked);
    let mcp: Value = serde_json::from_str(&read(&f.home.join(".mcp.json")))?;
    assert_eq!(mcp["mcpServers"]["search"]["command"], "srv");
    let eff = f.hub.effective("fake1", "committed")?;
    assert!(
        eff["egress"]["allow_hosts"]
            .as_array()
            .ok_or("hosts")?
            .contains(&json!("api.example.com"))
    );
    Ok(())
}

// ---- drift, verify, revert -------------------------------------------------------------------------------------------

#[test]
fn test_drift_states() -> R {
    let f = hub()?.setup()?;
    assert_eq!(f.hub.drift("fake1")?["status"], "never_applied");
    f.run(None, &[])?;
    let d = f.hub.drift("fake1")?;
    assert_eq!(d["status"], "in_sync");
    assert_eq!(d["drift"], json!({"edited_outside": [], "pending_changes": 0}));
    f.put(
        "instruction",
        "style",
        json!({"title": "Style", "order": 10, "body": "v2\n"}),
    )?;
    f.commit()?;
    let d = f.hub.drift("fake1")?;
    assert_eq!(
        (d["status"].clone(), d["drift"]["pending_changes"].clone()),
        (json!("pending_changes"), json!(1))
    );
    assert_eq!(d["drift"]["edited_outside"], json!([]));
    f.run(None, &[])?;
    std::fs::write(f.home.join("agents/reviewer.md"), "tampered")?;
    let d = f.hub.drift("fake1")?;
    assert_eq!(d["status"], "edited_outside");
    assert_eq!(
        d["drift"]["edited_outside"],
        json!([{"root": "home", "path": "agents/reviewer.md", "reason": "changed"}])
    );
    let row = d["files"]
        .as_array()
        .ok_or("files")?
        .iter()
        .find(|r| r["path"] == "agents/reviewer.md")
        .ok_or("row")?;
    assert_eq!(
        (
            row["expected"].as_str().map(str::len),
            row["actual"].as_str().map(str::len)
        ),
        (Some(12), Some(12))
    );
    assert_ne!(row["expected"], row["actual"]);
    assert!(!d.to_string().contains("tampered")); // hashes only, never contents
    std::fs::remove_file(f.home.join("agents/reviewer.md"))?;
    assert_eq!(f.hub.drift("fake1")?["drift"]["edited_outside"][0]["reason"], "missing");
    assert_eq!(f.hub.client_status("fake1")["status"], "edited_outside");
    Ok(())
}

#[test]
fn test_verify_reports_failures() -> R {
    let f = hub()?.setup()?;
    assert_eq!(f.hub.verify("fake1").err().map(|p| p.status), Some(409)); // never applied
    f.run(None, &[])?;
    assert_eq!(f.hub.verify("fake1")?["ok"], json!(true));
    std::fs::write(f.home.join("agents/reviewer.md"), "tampered")?;
    let v = f.hub.verify("fake1")?;
    assert_eq!(v["ok"], json!(false));
    assert!(
        v["checks"]
            .as_array()
            .ok_or("checks")?
            .iter()
            .any(|c| c["ok"] == json!(false))
    );
    f.runner.set_rc(1);
    let v = f.hub.verify("fake1")?;
    let c = v["checks"]
        .as_array()
        .ok_or("checks")?
        .iter()
        .find(|c| c["name"] == "fake-client --list")
        .ok_or("no check")?;
    assert_eq!(c["ok"], json!(false));
    Ok(())
}

#[test]
fn test_rollback_by_reverting_content_then_apply() -> R {
    let f = hub()?.setup()?;
    f.run(None, &[])?;
    let v1 = snapshot(&f.home);
    f.put("skill", "alpha", json!({"description": "Alpha v2", "body": "v2\n"}))?;
    f.put(
        "instruction",
        "style",
        json!({"title": "Style", "order": 10, "body": "v2\n"}),
    )?;
    let bad = f.hub.commit("v2", "t")?["commit"]
        .as_str()
        .ok_or("no commit")?
        .to_string();
    f.run(None, &[])?;
    assert_ne!(snapshot(&f.home), v1);
    f.hub.revert(&bad, "t")?;
    let (plan, _) = f.run(None, &[])?;
    assert_eq!(snapshot(&f.home), v1);
    assert!(acts(&plan, "fake1").values().any(|a| a == "change"));
    Ok(())
}

#[test]
fn test_effective_view() -> R {
    let f = hub()?.setup()?;
    let eff = f.hub.effective("fake1", "committed")?;
    let skills = eff["items"]["skills"].as_array().ok_or("skills")?;
    assert_eq!(
        skills.iter().map(|i| i["name"].clone()).collect::<Vec<_>>(),
        vec![json!("alpha")]
    );
    assert_eq!(skills[0]["via"], json!(["collection:core"]));
    assert_eq!(eff["items"]["agents"][0]["via"], json!(["direct"]));
    assert_eq!(
        (eff["network_default"].clone(), eff["egress"]["allow_hosts"].clone()),
        (json!("deny"), json!([]))
    );
    assert!(
        eff["rules"]
            .as_array()
            .ok_or("rules")?
            .iter()
            .any(|r| r["match"] == "~/.aws/**" && r["decision"] == "deny")
    );
    assert!(
        eff["warnings"]
            .as_array()
            .ok_or("warnings")?
            .iter()
            .all(Value::is_string)
    );
    f.put("instruction", "later", json!({"title": "L", "body": "l\n"}))?;
    f.hub
        .organizer
        .enable_for_client("fake1", "instruction", &["later".into()])?;
    let names = |v: &Value| -> Vec<Value> {
        v["items"]["instructions"]
            .as_array()
            .map(|a| a.iter().map(|i| i["name"].clone()).collect())
            .unwrap_or_default()
    };
    assert!(!names(&f.hub.effective("fake1", "committed")?).contains(&json!("later"))); // committed view
    assert!(names(&f.hub.effective("fake1", "working")?).contains(&json!("later")));
    Ok(())
}

#[test]
fn test_audit_checks_live_config_even_for_advisory_concerns() -> R {
    let f = hub()?.setup()?;
    std::fs::write(
        f.home.join("settings.json"),
        json!({"permissions": {"allow": ["path_read:~/.ssh/**", "tool:web_fetch"], "deny": []}}).to_string(),
    )?;
    std::fs::write(
        f.home.join(".mcp.json"),
        json!({"mcpServers": {"rogue": {"command": "x"}}}).to_string(),
    )?;
    let a = f.hub.audit_client("fake1")?;
    let codes: std::collections::BTreeSet<String> = a["findings"]
        .as_array()
        .ok_or("findings")?
        .iter()
        .filter_map(|x| x["code"].as_str().map(String::from))
        .collect();
    assert_eq!(a["ok"], json!(false));
    for c in ["floor_path_read", "floor_tool", "floor_deny_missing", "mcp_not_in_hub"] {
        assert!(codes.contains(c), "{c} in {codes:?}");
    }
    Ok(())
}

#[test]
fn test_missing_adapter_blocks_cleanly() -> R {
    let f = hub()?;
    std::fs::write(
        f.content("clients/ghost.yaml"),
        "id: ghost\nadapter: nothere\ndisplay_name: G\nroots:\n  home: /tmp/x\n",
    )?;
    f.commit()?;
    let cp = f.hub.plan(Some(&["ghost".into()]), "committed")?.clients.remove(0);
    assert!(cp.blocked && cp.diagnostics[0]["code"] == "adapter_missing");
    Ok(())
}

#[test]
fn test_conflicting_duplicate_artifacts_block() -> R {
    let dup = FakeAdapter::subclass("dup", |_| {
        let a = Artifact::new("home", "x.txt", b"1".to_vec(), "skill");
        let mut a2 = a.clone();
        a2.content = b"2".to_vec();
        let mut m = Artifact::new("home", "m.json", b"{}".to_vec(), "mcp");
        m.merge = "json_keys".into();
        m.managed_keys = vec!["a".into()];
        let o = Artifact::new("home", "m.json", b"x".to_vec(), "mcp");
        Ok(RenderResult {
            artifacts: vec![a, a2, m, o],
            diagnostics: vec![],
        })
    });
    let f = hub_with(vec![dup], true)?;
    f.hub.put_client(
        "d1",
        &obj(json!({"adapter": "dup", "display_name": "D", "roots": {"home": s(&f.home)}})),
        true,
    )?;
    f.commit()?;
    let plan = f.hub.plan(Some(&["d1".into()]), "committed")?;
    let codes: Vec<&str> = plan.clients[0]
        .diagnostics
        .iter()
        .filter_map(|d| d["code"].as_str())
        .collect();
    assert!(codes.contains(&"duplicate_artifact") && codes.contains(&"merge_mode_mismatch"));
    Ok(())
}

#[test]
fn test_adapter_exception_is_contained() -> R {
    let boom = FakeAdapter::subclass("boom", |_| {
        Err(AdapterError::new("RuntimeError", "secret detail sk-should-not-leak"))
    });
    let f = hub_with(vec![boom], true)?;
    f.hub.put_client(
        "b1",
        &obj(json!({"adapter": "boom", "display_name": "B", "roots": {"home": s(&f.home)}})),
        true,
    )?;
    f.commit()?;
    let cp = f.hub.plan(Some(&["b1".into()]), "committed")?.clients.remove(0);
    assert!(cp.blocked && !Value::Array(cp.diagnostics).to_string().contains("should-not-leak"));
    Ok(())
}

#[test]
fn test_foreign_value_in_a_managed_key_is_a_conflict() -> R {
    let f = hub()?.full()?;
    std::fs::write(
        f.home.join("settings.json"),
        json!({"permissions": {"deny": ["old"]}}).to_string(),
    )?;
    let (plan, _) = f.run(None, &[])?;
    assert_eq!(acts(&plan, "fake1")["home:settings.json"], "conflict");
    assert_eq!(
        serde_json::from_str::<Value>(&read(&f.home.join("settings.json")))?,
        json!({"permissions": {"deny": ["old"]}})
    );
    f.run(None, &["home:settings.json"])?;
    let st: Value = serde_json::from_str(&read(&f.home.join("settings.json")))?;
    assert!(
        !st["permissions"]["deny"]
            .as_array()
            .ok_or("deny")?
            .contains(&json!("old"))
    );
    Ok(())
}

#[test]
fn test_legacy_lock_from_an_import_reads_as_adopted_not_applied() -> R {
    let f = hub()?.setup()?;
    let legacy =
        json!({"version": 1, "client": "fake1", "applied_at": 1_700_000_000.0, "content_hash": "", "files": {}});
    std::fs::create_dir_all(f.data("locks"))?;
    std::fs::write(f.data("locks/fake1.json"), legacy.to_string())?;
    let lk = f.hub.locks.load("fake1").ok_or("no lock")?;
    assert_eq!((lk.adopted_at, lk.applied_at), (1_700_000_000.0, 0.0));
    let info = f.hub.client_view("fake1")?;
    assert_eq!(
        (info["adopted_at"].clone(), info["last_applied_at"].clone()),
        (json!(1_700_000_000.0), Value::Null)
    );
    f.run(None, &[])?; // a real apply sets applied_at, plan id and content hash
    let info = f.hub.client_view("fake1")?;
    assert!(info["last_applied_at"].is_f64() && info["last_applied_plan"].is_string());
    assert!(!f.hub.locks.load("fake1").ok_or("no lock")?.content_hash.is_empty());
    Ok(())
}
