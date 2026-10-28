//! Port of the WS-B2 rows of backend/tests/core/test_ops_hardening.py (L5: discard needs confirm, only known content
//! paths are committed, literal pathspecs, the snapshot reads git objects). CORE's rows of the same Python file live in
//! tests/test_ops_hardening.rs. Needs WS-B1 (content store) and WS-F (contentguard).

mod b2_fakes;

use b2_fakes::*;
use serde_json::json;

#[test]
fn test_discard_all_needs_confirm_and_supports_dry_run() -> R {
    let f = hub()?;
    f.put("instruction", "a", json!({"title": "A", "body": "a\n"}))?;
    f.put("instruction", "b", json!({"title": "B", "body": "b\n"}))?;
    let both = json!(["instructions/a.md", "instructions/b.md"]);
    let sorted = |v: &serde_json::Value| {
        let mut x: Vec<String> = v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_str().map(String::from))
            .collect();
        x.sort();
        json!(x)
    };
    let e = f.hub.discard(None, "t", false, false).err().ok_or("discarded")?;
    assert_eq!(e.code, "confirm_required");
    assert_eq!(sorted(e.extra.get("would_discard").ok_or("no would_discard")?), both);
    assert_eq!(f.hub.changes()?["count"], 2);
    assert_eq!(sorted(&f.hub.discard(None, "t", false, true)?["would_discard"]), both);
    assert_eq!(f.hub.changes()?["count"], 2); // dry run deleted nothing
    // explicit paths need no confirm
    assert_eq!(
        f.hub.discard(Some(&["instructions/a.md".into()]), "t", false, false)?["discarded"],
        json!(["instructions/a.md"])
    );
    f.hub.discard(None, "t", true, false)?;
    assert_eq!(f.hub.changes()?["count"], 0);
    Ok(())
}

#[tokio::test]
async fn test_discard_over_http() -> R {
    let f = hub()?;
    let app = f.api();
    let k = f.admin_key()?;
    f.put("instruction", "a", json!({"title": "A", "body": "a\n"}))?;
    let url = format!("{API}/changes/discard");
    assert_eq!(req(&app, "POST", &url, Some(&k), Some(json!({}))).await.status, 422);
    let d = req(&app, "POST", &url, Some(&k), Some(json!({"dry_run": true})))
        .await
        .json();
    assert_eq!(d["would_discard"], json!(["instructions/a.md"]));
    assert_eq!(
        req(&app, "POST", &url, Some(&k), Some(json!({"confirm": true})))
            .await
            .status,
        200
    );
    Ok(())
}

#[test]
fn test_commit_stages_only_known_content_paths_and_reports_strays() -> R {
    let f = hub()?;
    f.put("instruction", "a", json!({"title": "A", "body": "a\n"}))?;
    std::fs::write(f.content("stray.txt"), "not content")?;
    std::fs::create_dir(f.content("notes"))?;
    std::fs::write(f.content("notes/todo.md"), "x")?;
    std::fs::create_dir_all(f.content("inbox/c1"))?;
    std::fs::write(f.content("inbox/c1/x.md"), "proposal")?;
    let out = f.hub.commit("only content", "t")?;
    let mut strays: Vec<String> = out["strays"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(String::from))
        .collect();
    strays.sort();
    assert_eq!(strays, vec!["inbox/c1/x.md", "notes/todo.md", "stray.txt"]);
    let tracked = f.hub.repo.text(&["ls-files"])?;
    let tracked: Vec<&str> = hub_core::py::text::split_ws(&tracked);
    assert!(tracked.contains(&"instructions/a.md"));
    assert!(
        !["stray.txt", "notes/todo.md", "inbox/c1/x.md"]
            .iter()
            .any(|p| tracked.contains(p))
    );
    let e = f.hub.commit("only strays left", "t").err().ok_or("committed")?; // strays alone never commit
    assert_eq!(e.code, "nothing_to_commit");
    Ok(())
}

#[test]
fn test_deletions_under_known_paths_are_committed() -> R {
    let f = hub()?;
    f.put("instruction", "a", json!({"title": "A", "body": "a\n"}))?;
    f.hub.commit("add", "t")?;
    f.hub.work.delete("instruction", "a")?;
    f.hub.commit("remove", "t")?;
    assert!(!f.hub.repo.text(&["ls-files"])?.contains("instructions/a.md"));
    Ok(())
}

#[test]
fn test_pathspecs_are_literal() -> R {
    let f = hub()?;
    f.put("instruction", "a", json!({"title": "A", "body": "a\n"}))?;
    f.hub.commit("add", "t")?;
    assert!(
        f.hub
            .repo
            .run(&["ls-files", "--", "instructions/*.md"], true, None)?
            .out
            .is_empty()
    ); // not expanded
    let out = f
        .hub
        .repo
        .run(&["ls-files", "--", "instructions/a.md"], true, None)?
        .out;
    assert!(String::from_utf8_lossy(&out).contains("instructions/a.md"));
    Ok(())
}

#[test]
fn test_snapshot_reads_objects_not_attributes() -> R {
    let f = hub()?;
    f.put("instruction", "a", json!({"title": "A", "body": "hello\n"}))?;
    f.hub.commit("add", "t")?;
    let (_, store) = f.hub.approved()?;
    assert_eq!(store.load("instruction", "a")?.body, "hello\n");
    f.put("instruction", "a", json!({"title": "A", "body": "changed\n"}))?;
    assert!(
        f.hub.changes()?["items"][0]["diff"]
            .as_str()
            .unwrap_or_default()
            .contains("+changed")
    );
    Ok(())
}
