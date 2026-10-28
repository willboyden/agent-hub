//! Port of backend/tests/core/test_content.py (WS-B1): the content store over a seeded content tree.

mod b1_support;

use b1_support::{R, add_client, hub, pv, seed_content};
use hub_core::value::PyValue;
use serde_json::json;

fn status(r: hub_core::HubResult<impl Sized>) -> u16 {
    r.err().map_or(200, |p| p.status)
}

#[test]
fn test_seeded_repo_layout() -> R {
    let t = hub()?;
    let root = t.content();
    assert!(root.join(".git").is_dir() && root.join("hub.yaml").is_file() && root.join("endpoints.yaml").is_file());
    assert!(t.hub.repo.head().map_err(|p| p.detail)?.is_some());
    assert!(t.hub.repo.is_clean().map_err(|p| p.detail)?);
    assert!(t.hub.work.endpoints().map_err(|p| p.detail)?["router"].starts_with("http://127.0.0.1"));
    Ok(())
}

/// The content half of test_seeded_repo_layout (runs without git).
#[test]
fn seeded_content_layout() -> R {
    let t = hub()?;
    let root = t.content();
    assert!(root.join("hub.yaml").is_file() && root.join("endpoints.yaml").is_file());
    assert_eq!(
        std::fs::read_to_string(root.join("hub.yaml"))?,
        "version: 1\nname: agent-hub-content\n"
    );
    assert!(!root.join("inbox/.gitkeep").exists() && root.join("skills/.gitkeep").is_file());
    assert!(t.hub.work.endpoints().map_err(|p| p.detail)?["router"].starts_with("http://127.0.0.1"));
    Ok(())
}

#[test]
fn test_skill_crud_with_extra_files_and_sidecar() -> R {
    let t = hub()?;
    let w = &t.hub.work;
    let it = w
        .put(
            "skill",
            "alpha",
            &pv(
                json!({"description": "A", "body": "hello\n", "files": {"ref/a.md": "one", "b.txt": "two"},
                       "tags": ["x"], "notes": "n", "source": "manual"}),
            ),
            None,
        )
        .map_err(|p| p.detail)?;
    let mut keys: Vec<&String> = it.files.keys().collect();
    keys.sort();
    assert!(it.valid() && keys == ["SKILL.md", "b.txt", "ref/a.md"]);
    let side = it.sidecar.clone().unwrap_or_default();
    assert_eq!(it.tags(), ["x"]);
    assert_eq!(side.get("notes"), Some(&PyValue::str("n")));
    // SKILL.md stays standard-clean: bookkeeping lives in hub.yaml only
    let md = String::from_utf8_lossy(&it.files["SKILL.md"]).into_owned();
    assert!(!md.contains("tags") && !md.contains("notes"));
    // files=None leaves extras alone; files={...} replaces the set
    w.put(
        "skill",
        "alpha",
        &pv(json!({"description": "A2", "body": "hello\n"})),
        None,
    )
    .map_err(|p| p.detail)?;
    assert_eq!(w.load("skill", "alpha").map_err(|p| p.detail)?.files.len(), 3);
    let it = w
        .put(
            "skill",
            "alpha",
            &pv(json!({"description": "A2", "body": "hello\n", "files": {"only.txt": "x"}})),
            None,
        )
        .map_err(|p| p.detail)?;
    let mut keys: Vec<&String> = it.files.keys().collect();
    keys.sort();
    assert!(keys == ["SKILL.md", "only.txt"] && it.tags() == ["x"]);
    w.delete("skill", "alpha").map_err(|p| p.detail)?;
    assert_eq!(status(w.load("skill", "alpha")), 404);
    Ok(())
}

#[test]
fn test_skill_extra_file_traversal_and_reserved_rejected() -> R {
    let t = hub()?;
    for bad in [
        "../evil",
        "/abs",
        "a/../../b",
        "SKILL.md",
        "hub.yaml",
        ".git/config",
        "a\\b",
    ] {
        let r = t.hub.work.put(
            "skill",
            "bad",
            &pv(json!({"description": "d", "body": "", "files": {bad: "x"}})),
            None,
        );
        assert_eq!(status(r), 422, "{bad}");
    }
    assert!(!t.content().parent().map(|p| p.join("evil").exists()).unwrap_or(true));
    Ok(())
}

#[test]
fn test_skill_duplicate() -> R {
    let t = hub()?;
    let w = &t.hub.work;
    w.put(
        "skill",
        "alpha",
        &pv(json!({"description": "A", "body": "b\n", "files": {"x.txt": "1"}})),
        None,
    )
    .map_err(|p| p.detail)?;
    let dup = w.duplicate_skill("alpha", "alpha-copy").map_err(|p| p.detail)?;
    assert!(dup.valid() && dup.meta_str("name") == "alpha-copy" && dup.files["x.txt"] == b"1");
    assert_eq!(status(w.duplicate_skill("alpha", "alpha-copy")), 409);
    Ok(())
}

#[test]
fn test_every_kind_roundtrips() -> R {
    let cases = [
        (
            "agent",
            json!({"description": "d", "capabilities": ["read", "shell"], "model_tier": "fast", "body": "b"}),
        ),
        (
            "instruction",
            json!({"title": "T", "order": 5, "applies_to": ["fake1"], "body": "b"}),
        ),
        (
            "mcp",
            json!({"transport": "stdio", "command": "srv", "args": ["--x"], "env_names": ["API_TOKEN"],
                   "egress_hosts": ["api.example.com"]}),
        ),
        (
            "rule",
            json!({"title": "R", "kind": "path_read", "match": "/data/**", "decision": "ask", "reason": "why"}),
        ),
        ("memory", json!({"type": "user", "title": "M", "body": "text"})),
    ];
    for (kind, payload) in cases {
        let t = hub()?;
        let w = &t.hub.work;
        let it = w.put(kind, "thing", &pv(payload), None).map_err(|p| p.detail)?;
        assert!(it.valid(), "{kind}: {:?}", it.issues);
        let again = w.load(kind, "thing").map_err(|p| p.detail)?;
        assert!(again.meta == it.meta && again.body == it.body);
        assert!(w.names(kind).map_err(|p| p.detail)?.contains(&"thing".to_string()));
        w.delete(kind, "thing").map_err(|p| p.detail)?;
        assert!(!w.names(kind).map_err(|p| p.detail)?.contains(&"thing".to_string()));
    }
    Ok(())
}

#[test]
fn test_unknown_keys_and_bad_values_are_errors() -> R {
    let cases = [
        ("agent", json!({"description": "d", "capabilities": ["teleport"]})),
        ("agent", json!({"description": "d", "surprise": 1})),
        ("instruction", json!({"title": "T", "order": "x"})),
        ("mcp", json!({"transport": "stdio"})),
        ("mcp", json!({"transport": "http", "url": "ftp://x"})),
        (
            "mcp",
            json!({"transport": "stdio", "command": "c", "env_names": ["TOKEN=hunter2"]}),
        ),
        (
            "mcp",
            json!({"transport": "stdio", "command": "c", "egress_hosts": ["bad host!"]}),
        ),
        (
            "rule",
            json!({"title": "R", "kind": "nonsense", "match": "x", "decision": "allow"}),
        ),
        (
            "rule",
            json!({"title": "R", "kind": "tool", "match": "x", "decision": "maybe"}),
        ),
        ("memory", json!({"type": "secret", "title": "M"})),
    ];
    let t = hub()?;
    for (kind, payload) in cases {
        let r = t.hub.work.put(kind, "thing", &pv(payload.clone()), None);
        let p = r.err().ok_or("accepted")?;
        assert_eq!(
            (p.status, p.code.as_str()),
            (422, "validation_error"),
            "{kind} {payload}"
        );
        assert!(
            !t.hub
                .work
                .names(kind)
                .map_err(|p| p.detail)?
                .contains(&"thing".to_string())
        );
    }
    Ok(())
}

#[test]
fn test_hostile_ids_rejected() -> R {
    let t = hub()?;
    let long = "x".repeat(65);
    for name in ["../x", "A", "a b", "", long.as_str(), ".x", "a/b", "a\n"] {
        let r = t.hub.work.put(
            "agent",
            name,
            &pv(json!({"description": "d", "capabilities": []})),
            None,
        );
        assert!(matches!(status(r), 400 | 404), "{name:?}");
        assert!(matches!(status(t.hub.work.load("agent", name)), 400 | 404), "{name:?}");
    }
    Ok(())
}

#[test]
fn test_body_id_must_match_url() -> R {
    let t = hub()?;
    let p = t
        .hub
        .work
        .put("instruction", "one", &pv(json!({"id": "two", "title": "T"})), None)
        .err()
        .ok_or("accepted")?;
    assert_eq!(
        (p.status, p.detail.as_str()),
        (422, "`id` in the body must match the URL (one)")
    );
    Ok(())
}

#[test]
fn test_invalid_file_on_disk_is_reported_not_fatal() -> R {
    let t = hub()?;
    let a = t.content().join("agents");
    std::fs::write(a.join("broken.md"), "---\nname: broken\nbogus: 1\n---\nbody")?;
    std::fs::write(a.join("mismatch.md"), "---\nname: other\ndescription: d\n---\nbody")?;
    std::fs::write(a.join("nofm.md"), "no frontmatter")?;
    let cat = t.hub.work.catalog().map_err(|p| p.detail)?;
    let paths: std::collections::BTreeSet<String> = cat
        .problems
        .iter()
        .map(|p| p.path.split(':').next().unwrap_or_default().to_string())
        .collect();
    for want in ["agents/broken", "agents/mismatch", "agents/nofm"] {
        assert!(paths.contains(want), "{want}: {paths:?}");
    }
    assert!(!cat.items[&("agent".to_string(), "broken".to_string())].valid());
    Ok(())
}

#[test]
fn test_yaml_bomb_and_symlink_and_huge_file_in_content() -> R {
    let t = hub()?;
    let rules = t.content().join("rules");
    std::fs::write(
        rules.join("bomb.yaml"),
        "id: bomb\ntitle: &a t\nkind: tool\nmatch: *a\ndecision: deny\n",
    )?;
    std::fs::write(rules.join("big.yaml"), format!("id: big\nx: {}", "a".repeat(400_000)))?;
    let secret = t.root.join("secret.yaml");
    std::fs::write(&secret, "id: link\ntitle: t\nkind: tool\nmatch: x\ndecision: deny\n")?;
    std::os::unix::fs::symlink(&secret, rules.join("link.yaml"))?;
    let cat = t.hub.work.catalog().map_err(|p| p.detail)?;
    let key = |n: &str| ("rule".to_string(), n.to_string());
    assert!(!cat.items[&key("bomb")].valid() && !cat.items[&key("big")].valid());
    assert!(!cat.items.contains_key(&key("link"))); // symlinked content files are ignored, not followed
    Ok(())
}

#[test]
fn test_delete_drops_references() -> R {
    let t = hub()?;
    add_client(&t.hub, &t.home()).map_err(|p| p.detail)?;
    seed_content(&t.hub, "fake1").map_err(|p| p.detail)?;
    t.hub.work.delete("skill", "alpha").map_err(|p| p.detail)?;
    assert!(
        t.hub
            .work
            .get_collection("core")
            .map_err(|p| p.detail)?
            .members
            .iter()
            .all(|m| m.name != "alpha")
    );
    t.hub.work.delete("agent", "reviewer").map_err(|p| p.detail)?;
    assert!(
        !t.hub
            .work
            .get_profile("fake1")
            .map_err(|p| p.detail)?
            .enable
            .agents
            .contains(&"reviewer".to_string())
    );
    Ok(())
}

#[test]
fn test_client_and_profile_validation() -> R {
    let t = hub()?;
    let w = &t.hub.work;
    assert_eq!(
        status(w.put_client("bad", &pv(json!({"adapter": "generic", "display_name": "x"})))),
        422
    );
    assert_eq!(
        status(w.put_client(
            "bad",
            &pv(json!({"adapter": "fake", "display_name": "x", "manage": {"nonsense": true}}))
        )),
        422
    );
    assert_eq!(
        status(w.put_profile("fake1", &pv(json!({"enable": {"skills": ["../x"]}})))),
        422
    );
    assert_eq!(status(w.put_profile("fake1", &pv(json!({"mystery": []})))), 422);
    Ok(())
}

#[test]
fn test_endpoints_validation() -> R {
    let t = hub()?;
    let w = &t.hub.work;
    assert_eq!(
        status(w.put_endpoints(&pv(json!({"router": "file:///etc/passwd"})))),
        422
    );
    assert_eq!(status(w.put_endpoints(&pv(json!({"Bad Key": "http://x"})))), 422);
    let ok = w
        .put_endpoints(&pv(json!({"router": "http://127.0.0.1:4000/v1"})))
        .map_err(|p| p.detail)?;
    assert_eq!(ok.len(), 1);
    assert_eq!(ok["router"], "http://127.0.0.1:4000/v1");
    Ok(())
}
