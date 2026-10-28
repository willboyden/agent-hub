//! Port of WS-B1's rows of backend/tests/core/test_audit_fixes.py: observed imports validate before they delete, obey
//! the content caps, restore a skill whose replacement failed, refuse secret-looking text unless overridden (and audit
//! the override, even when the write fails), and reserved client ids. Security control S-IMPORT.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod wsc_support;

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use agent_hub::adapters::base::Files;
use agent_hub::services::importer::ObservedSource;
use hub_core::value::PyValue;
use serde_json::{Map, Value, json};
use wsc_support::*;

/// The `hub` + `obs` fixtures: a hub whose content repo exists and a client `obs1` on the fake observed adapter.
fn obs_setup(r: Arc<FakeEvalRunner>) -> (T, Arc<ObsAdapter>) {
    let (t, a) = obs_hub(r);
    t.hub.init_content().unwrap();
    let home = t.root.join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    put_client(
        &t,
        "obs1",
        &json!({"adapter": "fakeobs", "display_name": "Obs", "roots": {"home": home.to_string_lossy()}}),
    )
    .unwrap();
    (t, a)
}

/// test_skilleval.py `skill(hub, name, body)`: the skill as `hub.work.put` writes it.
fn put_skill(t: &T, name: &str, body: &str) {
    t.hub
        .work
        .put(
            "skill",
            name,
            &PyValue::from_json(
                &json!({"description": "Alpha skill", "body": body, "files": {"ref/notes.md": "notes"},
                                        "tags": ["core"]}),
            ),
            None,
        )
        .unwrap();
}

fn observe_one(t: &T, a: &ObsAdapter, sk: agent_hub::adapters::base::ObservedSkill) -> (String, String) {
    a.set(vec![sk], true);
    t.hub.observe("obs1", "hub").unwrap();
    let item = t.hub.observed_view("obs1", None, None, None, 100, None).unwrap()["items"][0].clone();
    (
        item["key"].as_str().unwrap().to_string(),
        item["content_hash"].as_str().unwrap().to_string(),
    )
}

#[allow(clippy::needless_pass_by_value)] // called with json!(...) literals
fn import(t: &T, items: Value, on_conflict: &str, reason: Option<&str>, actor: &str) -> Vec<Value> {
    let items: Vec<Value> = items.as_array().cloned().unwrap_or_default();
    t.hub
        .import_observed("obs1", &items, on_conflict, false, reason, actor)
        .unwrap()["results"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn gate_off(t: &T) {
    let mut p = Map::new();
    p.insert("eval_gate_observed".into(), json!("off"));
    t.hub.update_settings(&p).unwrap();
}

fn override_events(t: &T) -> Vec<Value> {
    t.hub.audit_svc.list_page(50, None).unwrap()["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["path"] == "skill.import.override")
        .cloned()
        .collect()
}

#[test]
fn test_replace_validates_before_deleting() {
    for bad in ["hub.yaml", ".git/hooks/x"] {
        let (t, obs) = obs_setup(FakeEvalRunner::new());
        gate_off(&t);
        put_skill(&t, "foo", "Hub version.\n");
        let before = std::fs::read(t.content().join("skills/foo/SKILL.md")).unwrap();
        obs.set(vec![oskill("foo", None, "agent", "New.\n", &[(bad, b"x")])], true);
        t.hub.observe("obs1", "hub").unwrap();
        let key = t.hub.observed_view("obs1", None, None, None, 100, None).unwrap()["items"][0]["key"].clone();
        let row = import(&t, json!([{"key": key}]), "replace", None, "hub")[0].clone();
        assert!(
            row["status"] == "error" && row["code"] == "validation_error",
            "{bad}: {row}"
        );
        assert_eq!(std::fs::read(t.content().join("skills/foo/SKILL.md")).unwrap(), before);
        assert!(t.content().join("skills/foo/hub.yaml").is_file()); // the old sidecar survived too
    }
}

#[test]
fn test_content_caps_apply_to_observed_import() {
    let (t, obs) = obs_setup(FakeEvalRunner::new());
    gate_off(&t);
    let many: Vec<(String, Vec<u8>)> = (0..210).map(|i| (format!("f{i}.md"), b"x".to_vec())).collect();
    let many_ref: Vec<(&str, &[u8])> = many.iter().map(|(k, v)| (k.as_str(), v.as_slice())).collect();
    let max = usize::try_from(t.hub.cfg().max_skill_bytes).unwrap();
    let big = vec![b'x'; max + 1];
    obs.set(
        vec![
            oskill("many", None, "agent", "Do it.\n", &many_ref),
            oskill("fat", None, "agent", "Do it.\n", &[("big.bin", &big)]),
        ],
        true,
    );
    t.hub.observe("obs1", "hub").unwrap();
    let keys: Vec<Value> = t.hub.observed_view("obs1", None, None, None, 100, None).unwrap()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| json!({"key": i["key"]}))
        .collect();
    let rows = import(&t, Value::Array(keys), "skip", None, "hub");
    assert_eq!(
        rows.iter().map(|r| r["status"].clone()).collect::<Vec<_>>(),
        [json!("error"), json!("error")]
    );
    assert!(rows.iter().all(|r| r["code"] == "skill_too_large"));
    assert!(t.hub.work.names("skill").unwrap().is_empty());
}

/// Python monkeypatches `write_skill_files` to write SKILL.md and then raise OSError; here the replacement carries a
/// file name longer than NAME_MAX, so the real write fails after SKILL.md went out (ENAMETOOLONG, an OSError).
#[test]
fn test_a_failed_replace_restores_the_old_skill() {
    let t = hub();
    std::fs::create_dir_all(t.content()).unwrap();
    t.hub.work.seed().unwrap();
    t.hub
        .work
        .put_client(
            "obs1",
            &PyValue::from_json(&json!({"id": "obs1", "adapter": "fakeobs", "display_name": "Obs"})),
        )
        .unwrap();
    put_skill(&t, "foo", "Hub version.\n");
    let long = format!("{}.md", "n".repeat(300));
    let src = FakeSource::one(
        "k1",
        "foo",
        &[("SKILL.md", md("foo", "New.\n")), (long.as_str(), b"1".to_vec())],
    );
    let out = t
        .hub
        .importer
        .import_observed_from("obs1", &[json!({"key": "k1"})], "replace", false, None, &src)
        .unwrap();
    let row = &out["results"][0];
    assert!(row["status"] == "error" && row["detail"] == "OSError", "{row}");
    assert!(
        std::fs::read_to_string(t.content().join("skills/foo/SKILL.md"))
            .unwrap()
            .contains("Hub version.")
    );
    let mut names: Vec<String> = std::fs::read_dir(t.content().join("skills"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, [".gitkeep", "foo"]);
}

#[test]
fn test_secret_looking_text_is_refused_unless_overridden_and_audited() {
    let (t, obs) = obs_setup(FakeEvalRunner::new());
    gate_off(&t);
    let leak = format!("sk-{}", "A1b2C3d4E5f6G7h8I9j0");
    let env = format!("api_key={leak}\n");
    let (key, h) = observe_one(
        &t,
        &obs,
        oskill("leaky", None, "agent", "Do it.\n", &[("ref/env.md", env.as_bytes())]),
    );
    let row = import(&t, json!([{"key": key}]), "skip", None, "hub")[0].clone();
    assert!(row["status"] == "refused" && row["code"] == "secret_detected");
    assert!(row["detail"].as_str().unwrap().contains("ref/env.md") && !row.to_string().contains(&leak));
    assert!(t.hub.work.names("skill").unwrap().is_empty());
    let ok = import(
        &t,
        json!([{"key": key}]),
        "skip",
        Some("reviewed: the value is a documented dummy"),
        "alice",
    )[0]
    .clone();
    assert_eq!(ok["status"], "created");
    let ev = override_events(&t);
    assert_eq!(ev.len(), 1);
    let p = &ev[0]["params"];
    assert!(ev[0]["actor"] == "alice" && p["key"] == key.as_str() && p["content_hash"] == h[..12]);
    assert!(p["overrides"] == json!(["secret_detected"]) && p["outcome"] == "written" && p["skill"] == "leaky");
    assert!(!Value::Array(ev).to_string().contains(&leak));
}

/// Python monkeypatches `write_skill_files` to raise OSError; here the skills directory is made read-only, so the
/// real write fails (PermissionError) after the gate let the blocked skill through on an override.
#[test]
fn test_override_is_audited_even_when_the_write_fails() {
    let (t, obs) = obs_setup(FakeEvalRunner::new());
    let (key, h) = observe_one(&t, &obs, oskill("devops/foo", None, "agent", "Do it.\n", &[]));
    use_(
        &t.hub.skilleval,
        FakeEvalRunner::tier1(canned(&[("Code_Risk_Analysis", "failed", &[("high", "bad")])])),
        LAUNCHERS,
        None,
        None,
    );
    t.hub.skilleval.evaluate(&format!("obs1:{key}"), "static", "t").unwrap();
    let skills = t.content().join("skills");
    std::fs::set_permissions(&skills, std::fs::Permissions::from_mode(0o555)).unwrap();
    let rows = import(
        &t,
        json!([{"key": key}]),
        "skip",
        Some("known false positive, reviewed by hand"),
        "bob",
    );
    std::fs::set_permissions(&skills, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(rows[0]["status"], "error");
    let ev = override_events(&t);
    assert_eq!(ev.len(), 1);
    let p = &ev[0]["params"];
    assert!(
        p["outcome"] == "failed: PermissionError" && p["overrides"] == json!(["eval_blocked"]),
        "{p}"
    );
    assert!(p["key"] == key.as_str() && p["content_hash"] == h[..12]);
}

#[test]
fn test_reserved_client_ids() {
    let t = hub();
    let home = t.root.join("home");
    for cid in ["hub", "a:b"] {
        let doc: indexmap::IndexMap<String, PyValue> = [
            ("adapter".to_string(), PyValue::str("fake")),
            ("display_name".to_string(), PyValue::str("x")),
            (
                "roots".to_string(),
                PyValue::from_json(&json!({"home": home.to_string_lossy()})),
            ),
        ]
        .into_iter()
        .collect();
        let e = t.hub.put_client(cid, &doc, true).expect_err("accepted");
        assert_eq!((e.status, e.code.as_str()), (422, "reserved_client_id"), "{cid}");
    }
}

/// An observed source with one entry (the importer's view of `ObserveService`).
struct FakeSource {
    entry: Value,
    files: Mutex<Files>,
}

impl FakeSource {
    fn one(key: &str, name: &str, files: &[(&str, Vec<u8>)]) -> FakeSource {
        FakeSource {
            entry: json!({"key": key, "name": name, "description": "A skill", "path": name, "origin": "agent"}),
            files: Mutex::new(files.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect()),
        }
    }
}

impl ObservedSource for FakeSource {
    fn index(&self, _client: &str) -> Option<Value> {
        Some(json!({"observed_at": 1.5, "skills": [self.entry]}))
    }
    fn entry(&self, _client: &str, key: &str) -> Option<Value> {
        (self.entry["key"] == key).then(|| self.entry.clone())
    }
    fn files(&self, _client: &str, _key: &str) -> hub_core::HubResult<Files> {
        Ok(self.files.lock().unwrap().clone())
    }
}
