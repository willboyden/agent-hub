//! Port of backend/tests/core/test_toctou.py (M4): dir-fd relative writes and compare-and-swap. Attacks are simulated
//! between plan and commit through the applier's test hooks (Python monkeypatches `Applier._stage/_commit`). Needs
//! WS-B1 + WS-F for the Hub; the engine-level equivalents run now in src/services/applier_tests.rs.

mod b2_fakes;

use std::sync::{Arc, Mutex};

use agent_hub::services::applier::ApplyHooks;
use b2_fakes::*;
use serde_json::json;

#[test]
fn test_symlink_swapped_in_before_staging_is_refused() -> R {
    let f = hub()?.setup()?;
    let outside = f.root.join("outside");
    std::fs::create_dir(&outside)?;
    std::fs::create_dir(f.home.join("agents"))?;
    let plan = f.hub.plan(None, "committed")?; // planned while `agents` is a normal directory
    std::fs::remove_dir_all(f.home.join("agents"))?;
    std::os::unix::fs::symlink(&outside, f.home.join("agents"))?; // swapped for a symlink between plan and apply
    let e = f.hub.apply(&plan.id, true, &[], "t").err().ok_or("applied")?;
    assert_eq!(e.status, 409); // the pre-pass replan no longer matches the reviewed plan
    assert_eq!(std::fs::read_dir(&outside)?.count(), 0);
    Ok(())
}

#[test]
fn test_symlink_swap_after_staging_cannot_redirect_the_write() -> R {
    let f = hub()?.setup()?;
    let outside = f.root.join("outside");
    std::fs::create_dir(&outside)?;
    let plan = f.hub.plan(None, "committed")?;
    let (home, out2) = (f.home.clone(), outside.clone());
    let swapped = Arc::new(Mutex::new(false));
    f.hub.applier.set_test_hooks(Some(ApplyHooks {
        before_commit: Some(Box::new(move |_| {
            let mut s = swapped.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if !*s {
                *s = true;
                let d = home.join("agents");
                if d.exists() {
                    let _ = std::fs::rename(&d, home.join("agents.moved"));
                }
                let _ = std::os::unix::fs::symlink(&out2, &d); // attacker swaps the directory for a link mid-apply
            }
            Ok(())
        })),
        ..ApplyHooks::default()
    }));
    f.hub.apply(&plan.id, true, &[], "t")?;
    f.hub.applier.set_test_hooks(None);
    assert_eq!(std::fs::read_dir(&outside)?.count(), 0); // the held dirfd still points at the ORIGINAL directory
    Ok(())
}

#[test]
fn test_live_file_edited_between_plan_and_commit_aborts_the_whole_client() -> R {
    let f = hub()?.setup()?;
    f.run(None, &[])?;
    let before = snapshot(&f.home);
    f.put(
        "agent",
        "reviewer",
        json!({"description": "d", "capabilities": ["read"], "body": "v2\n"}),
    )?;
    f.put(
        "instruction",
        "style",
        json!({"title": "Style", "order": 10, "body": "v2\n"}),
    )?;
    f.hub.commit("v2", "t")?;
    let plan = f.hub.plan(None, "committed")?;
    let home = f.home.clone();
    let calls = Arc::new(Mutex::new(0));
    f.hub.applier.set_test_hooks(Some(ApplyHooks {
        before_commit: Some(Box::new(move |_| {
            let mut c = calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            *c += 1;
            if *c == 2 {
                // after one file was already replaced
                let _ = std::fs::write(
                    home.join("agents/reviewer.md"),
                    "edited by a human at the worst moment\n",
                );
            }
            Ok(())
        })),
        ..ApplyHooks::default()
    }));
    let res = f.hub.apply(&plan.id, true, &[], "t")?;
    f.hub.applier.set_test_hooks(None);
    let r = &res["results"][0];
    let err = r["error"].as_str().unwrap_or_default();
    assert!(r["status"] == "failed" && err.starts_with("conflict") && err.contains("nothing was changed"));
    let mut after = snapshot(&f.home);
    assert_eq!(
        after.remove("agents/reviewer.md"),
        Some(b"edited by a human at the worst moment\n".to_vec())
    );
    let mut before = before;
    before.remove("agents/reviewer.md");
    assert_eq!(after, before); // everything else rolled back
    assert_eq!(tmp_files(&f.home), 0);
    Ok(())
}

#[test]
fn test_removal_is_also_compare_and_swap() -> R {
    let f = hub()?.setup()?;
    f.run(None, &[])?;
    f.hub.organizer.toggle("fake1", "agent", "reviewer", false)?;
    f.hub.commit("off", "t")?;
    let plan = f.hub.plan(None, "committed")?;
    let home = f.home.clone();
    f.hub.applier.set_test_hooks(Some(ApplyHooks {
        before_commit: Some(Box::new(move |op| {
            if op.name == "reviewer.md" {
                let _ = std::fs::write(home.join("agents/reviewer.md"), "late edit");
            }
            Ok(())
        })),
        ..ApplyHooks::default()
    }));
    let res = f.hub.apply(&plan.id, true, &[], "t")?;
    f.hub.applier.set_test_hooks(None);
    assert_eq!(res["results"][0]["status"], "failed");
    assert_eq!(read(&f.home.join("agents/reviewer.md")), "late edit");
    Ok(())
}

#[test]
fn test_stage_time_race_and_dir_swap_detection() -> R {
    let f = hub()?.setup()?;
    let plan = f.hub.plan(None, "committed")?;
    let (home, tmp) = (f.home.clone(), f.root.clone());
    let n = Arc::new(Mutex::new(0));
    f.hub.applier.set_test_hooks(Some(ApplyHooks {
        before_stage: Some(Box::new(move |_| {
            let mut c = n.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            *c += 1;
            if *c == 1 {
                let _ = std::fs::create_dir_all(home.join("skills"));
                // a link where the skill directory will be created
                let _ = std::os::unix::fs::symlink(&tmp, home.join("skills/alpha"));
            }
            Ok(())
        })),
        ..ApplyHooks::default()
    }));
    let res = f.hub.apply(&plan.id, true, &[], "t")?;
    f.hub.applier.set_test_hooks(None);
    assert_eq!(res["results"][0]["status"], "failed");
    assert!(!f.root.join("SKILL.md").exists() && !f.root.join("ref").exists());
    Ok(())
}
