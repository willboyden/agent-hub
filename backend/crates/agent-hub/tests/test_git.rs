//! Port of backend/tests/core/test_git.py: the content repo through the Hub (pending changes with diffs, the fixed
//! identity, refused hooks, discard, history/revert, the empty repo, invalid content, the HEAD snapshot). Needs WS-B1
//! (content store) and WS-F (contentguard: every git call re-verifies the repo). The same git behaviour is proven now by
//! the unit tests in src/services/gitrepo.rs (real git, contentguard bypassed).

mod b2_fakes;

use agent_hub::services::gitrepo::{EMPTY_TREE, GitRepo};
use b2_fakes::*;
use serde_json::json;

#[test]
fn test_pending_changes_have_diffs() -> R {
    let f = hub()?;
    f.put("instruction", "one", json!({"title": "One", "body": "first\n"}))?;
    let ch = f.hub.changes()?;
    assert_eq!(ch["count"], 1);
    assert_eq!(
        (ch["items"][0]["path"].clone(), ch["items"][0]["status"].clone()),
        (json!("instructions/one.md"), json!("added"))
    );
    assert!(ch["items"][0]["diff"].as_str().unwrap_or_default().contains("+first"));
    f.hub.commit("add one", "t")?;
    f.put("instruction", "one", json!({"title": "One", "body": "second\n"}))?;
    let ch = f.hub.changes()?;
    let diff = ch["items"][0]["diff"].as_str().unwrap_or_default();
    assert!(ch["items"][0]["status"] == "modified" && diff.contains("+second") && diff.contains("-first"));
    Ok(())
}

/// Python points GIT_CONFIG_GLOBAL at a hostile config; Rust cannot change the process environment safely in a test,
/// so the hostile config is planted where git would otherwise find it (`$HOME/.gitconfig`, and HOME is the content
/// dir): either way the fixed identity and `commit.gpgsign=false` win.
#[test]
fn test_commit_uses_fixed_identity_not_global() -> R {
    let f = hub()?;
    std::fs::write(
        f.content(".gitconfig"),
        "[user]\n\tname = Somebody Else\n\temail = else@example.com\n[commit]\n\tgpgsign = true\n",
    )?;
    f.put("instruction", "one", json!({"title": "One", "body": "x\n"}))?;
    let sha = f.hub.commit("msg", "t")?["commit"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let out = std::process::Command::new("git")
        .args(["log", "-1", "--format=%an <%ae> | %cn"])
        .current_dir(f.content(""))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", s(&f.root))
        .output()?;
    let who = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(who, "Agent Hub <agent-hub@localhost> | Agent Hub");
    assert!(!sha.is_empty());
    Ok(())
}

#[test]
fn test_hooks_in_the_content_repo_are_refused() -> R {
    use std::os::unix::fs::PermissionsExt;
    let f = hub()?;
    let ran = f.root.join("hub-hook-ran");
    let hook = f.content(".git/hooks/pre-commit");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\nexit 0\n", s(&ran)))?;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    f.put("instruction", "one", json!({"title": "One", "body": "x\n"}))?;
    let e = f.hub.commit("msg", "t").err().ok_or("committed")?;
    assert!(
        e.code == "content_dir_unsafe" && e.detail.contains("hooks"),
        "{}",
        e.detail
    );
    assert!(!ran.exists());
    Ok(())
}

#[test]
fn test_discard_all_and_paths() -> R {
    let f = hub()?;
    f.put("instruction", "keep", json!({"title": "K", "body": "k\n"}))?;
    f.hub.commit("keep", "t")?;
    f.put("instruction", "keep", json!({"title": "K", "body": "changed\n"}))?;
    f.put("instruction", "new1", json!({"title": "N", "body": "n\n"}))?;
    f.put("instruction", "new2", json!({"title": "N", "body": "n\n"}))?;
    f.hub
        .discard(Some(&["instructions/new1.md".into()]), "t", false, false)?;
    let names = f.hub.work.names("instruction")?;
    assert!(!names.contains(&"new1".to_string()) && names.contains(&"new2".to_string()));
    f.hub.discard(None, "t", true, false)?;
    assert!(f.hub.repo.is_clean()?);
    assert_eq!(f.hub.work.load("instruction", "keep")?.body, "k\n");
    Ok(())
}

#[test]
fn test_discard_rejects_hostile_paths() -> R {
    let f = hub()?;
    for bad in ["../x", "/etc", ".git/config", "--all"] {
        let e = f
            .hub
            .discard(Some(&[bad.into()]), "t", false, false)
            .err()
            .ok_or("discarded")?;
        assert_eq!(e.status, 400, "{bad}");
    }
    Ok(())
}

#[test]
fn test_history_and_revert() -> R {
    let f = hub()?;
    f.put("instruction", "one", json!({"title": "One", "body": "v1\n"}))?;
    let c1 = f.hub.commit("v1", "t")?["commit"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    f.put("instruction", "one", json!({"title": "One", "body": "v2\n"}))?;
    let c2 = f.hub.commit("v2", "t")?["commit"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let h = f.hub.history(50)?;
    assert_eq!(
        (h[0]["message"].clone(), h[1]["message"].clone()),
        (json!("v2"), json!("v1"))
    );
    f.put("instruction", "dirty", json!({"title": "D", "body": "d\n"}))?;
    assert_eq!(f.hub.revert(&c2, "t").err().map(|p| p.status), Some(409)); // dirty tree refuses
    f.hub.discard(None, "t", true, false)?;
    f.hub.revert(&c2, "t")?;
    assert_eq!(f.hub.work.load("instruction", "one")?.body, "v1\n");
    assert_eq!(f.hub.revert("not-a-hash!", "t").err().map(|p| p.status), Some(400));
    assert_eq!(f.hub.revert("deadbeefdeadbeef", "t").err().map(|p| p.status), Some(400));
    assert_ne!(c1, c2);
    Ok(())
}

#[test]
fn test_empty_repo_hash_and_nothing_to_commit() -> R {
    let d = tempfile::tempdir()?;
    let repo = GitRepo::new(&d.path().join("content"), "git", "n", "e@x", 30.0);
    repo.init()?;
    assert_eq!(repo.head_tree()?, EMPTY_TREE);
    assert!(repo.history(50)?.is_empty());
    assert_eq!(repo.commit("   ").err().map(|p| p.status), Some(400));
    assert_eq!(repo.commit("empty").err().map(|p| p.status), Some(409));
    Ok(())
}

#[test]
fn test_commit_refuses_invalid_content() -> R {
    let f = hub()?;
    std::fs::write(f.content("agents/broken.md"), "---\nbogus: 1\n---\n")?;
    assert_eq!(f.hub.commit("bad", "t").err().map(|p| p.status), Some(422));
    Ok(())
}

#[test]
fn test_snapshot_is_head_not_working_tree() -> R {
    let f = hub()?;
    f.put("instruction", "one", json!({"title": "One", "body": "v1\n"}))?;
    f.hub.commit("v1", "t")?;
    f.put("instruction", "one", json!({"title": "One", "body": "edited\n"}))?;
    let (tree, store) = f.hub.approved()?;
    assert_eq!(store.load("instruction", "one")?.body, "v1\n");
    assert_eq!(tree, f.hub.repo.head_tree()?);
    Ok(())
}
