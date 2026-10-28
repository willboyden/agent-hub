//! Port of backend/tests/core/test_observed_eval.py (Addendum F): observed inventory (store, comparison), evaluation
//! subjects, reuse by content hash, the FIFO queue, the gate, the import gate, enum settings, roles and subject path
//! safety. Fakes only. The observation store and the evaluation service are exercised directly (standalone, sharing one
//! state dir) where Python goes through `hub.observe`; the hub-level flows that need the content store (client
//! registry, catalog, import) wait for WS-B1/WS-B2 (`pending` ignores).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod wsc_support;

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use agent_hub::adapters::base::{Observation, ObservedSkill};
use agent_hub::services::observe::{HubView, ObserveService, make_key};
use agent_hub::services::skilleval::{SAFE_CHECKS, normalize_subject, parse_subject, valid_key};
use serde_json::{Value, json};
use wsc_support::*;

fn mode(p: &std::path::Path) -> u32 {
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0)
}

/// test_observed_eval.py `evaluated(hub, runner, name, tiers)`: content hash of a freshly evaluated hub skill.
fn evaluated(s: &S, r: Arc<FakeEvalRunner>, name: &str, tiers: &str) -> String {
    use_(&s.svc, r, LAUNCHERS, None, None);
    s.skill(name);
    let rec = s.svc.evaluate(name, tiers, "t").unwrap();
    rec["content_hash"].as_str().unwrap().to_string()
}

fn store(s: &S, skills: Vec<ObservedSkill>) -> Value {
    s.obs.store("obs1", &observation(skills, true)).unwrap()
}

fn key_of(idx: &Value, path: &str) -> String {
    idx["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["path"] == path)
        .unwrap()["key"]
        .as_str()
        .unwrap()
        .to_string()
}

// ---- subjects -------------------------------------------------------------------------------------------------------
#[test]
fn test_subject_fullmatch_rejects_traversal_and_junk() {
    let long_hub = format!("hub:{}", "x".repeat(65));
    let long_key = format!("client:{}", "k".repeat(82));
    for bad in [
        "hub:../x", "a:b/c", "hub:A", "hub:", ":x", "hub:x:y", "..:x", "hub:x\n", "a:b c", &long_hub, &long_key,
    ] {
        assert_eq!(parse_subject(bad).err().map(|p| p.status), Some(400), "{bad:?}");
        assert_eq!(normalize_subject(bad).err().map(|p| p.status), Some(400), "{bad:?}");
    }
}

#[test]
fn test_subjects_parse_and_bare_names_mean_hub() {
    assert_eq!(parse_subject("hub:alpha").unwrap(), ("hub".into(), "alpha".into()));
    assert_eq!(
        parse_subject("obs1:foo-0a1b2c3d").unwrap(),
        ("obs1".into(), "foo-0a1b2c3d".into())
    );
    assert_eq!(normalize_subject("alpha").unwrap(), "hub:alpha");
    assert_eq!(normalize_subject("obs1:foo-0a1b2c3d").unwrap(), "obs1:foo-0a1b2c3d");
}

#[tokio::test]
async fn test_subject_routes_refuse_traversal() {
    let r = FakeEvalRunner::new();
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            eval_runner: Some(r.clone()),
            ..Default::default()
        },
        |_| {},
    );
    use_(&t.hub.skilleval, r.clone(), LAUNCHERS, None, None);
    alpha(&t);
    let a = app(&t);
    let h = bearer(&t.key("ev-admin", "admin"));
    let hdr = [("authorization", h.as_str())];
    for bad in ["hub:..%2Fx", "a:b%2Fc", "hub:A", "..%2F..%2Fetc"] {
        let st = api(&a, "GET", &format!("/api/v1/skill-evaluations/{bad}"), &hdr, None)
            .await
            .status;
        assert!(st == 400 || st == 404, "{bad}: {st}");
        let st = api(
            &a,
            "GET",
            &format!("/api/v1/skill-evaluations/{bad}/runs/20261006T201500123Z-0a1b2c3d/report"),
            &hdr,
            None,
        )
        .await
        .status;
        assert!(st == 400 || st == 404, "{bad}: {st}");
    }
    let resp = api(
        &a,
        "POST",
        "/api/v1/skill-evaluations/run",
        &hdr,
        Some(json!({"subjects": ["hub:../x"]})),
    )
    .await;
    assert_eq!(resp.status, 400, "{}", resp.text());
    assert!(r.tool_calls().is_empty());
}

// ---- the observation store ------------------------------------------------------------------------------------------
#[test]
fn test_key_format_and_stability() {
    let k = make_key("devops/My Skill!");
    assert!(
        valid_key(&k)
            && k.starts_with("my-skill-")
            && k == make_key("devops/My Skill!")
            && k != make_key("other/My Skill!")
    );
    assert!(valid_key(&make_key("!!!")) && valid_key(&make_key(&"x".repeat(400))));
}

/// Python goes through `hub.observe("obs1")` (WS-B1 client registry); the store, the annotation and the counts are
/// what it returns, exercised here directly (see `..._via_hub` for the hub path).
#[test]
fn test_observe_stores_index_and_private_files() {
    let s = standalone();
    let idx = store(
        &s,
        vec![
            oskill("devops/foo", None, "agent", "Do it.\n", &[("ref/a.md", b"A")]),
            oskill("bar", None, "shipped", "Do it.\n", &[]),
        ],
    );
    let ann = s.obs.annotated("obs1").unwrap();
    let items = ann["items"].as_array().unwrap().clone();
    let counts = ObserveService::count(&items);
    assert_eq!(items.len(), 2);
    assert_eq!(counts["by_origin"], json!({"agent": 1, "shipped": 1}));
    assert_eq!(idx["complete"], true);
    assert_eq!(
        counts["by_hub_status"],
        json!({"in_hub_same": 0, "in_hub_differs": 0, "not_in_hub": 2})
    );
    assert_eq!(ann["notes"], json!(["note one"]));
    let root = s.data().join("observed/obs1");
    let file: Value = serde_json::from_str(&std::fs::read_to_string(root.join("index.json")).unwrap()).unwrap();
    assert_eq!(
        (file["client"].as_str(), file["source"].as_str()),
        (Some("obs1"), Some("fake container"))
    );
    assert_eq!(file["skills"].as_array().unwrap().len(), 2);
    let foo = file["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["path"] == "devops/foo")
        .unwrap()
        .clone();
    let key = foo["key"].as_str().unwrap();
    assert!(valid_key(key));
    assert_eq!(foo["files"], 2);
    assert_eq!(foo["bytes"], json!(md("foo", "Do it.\n").len() + 1));
    assert_eq!(
        std::fs::read(root.join("skills").join(key).join("ref/a.md")).unwrap(),
        b"A"
    );
    for p in [
        root.clone(),
        root.join("skills"),
        root.join("skills").join(key).join("ref"),
    ] {
        assert_eq!(mode(&p), 0o700, "{}", p.display());
    }
    for p in [
        root.join("index.json"),
        root.join("skills").join(key).join("SKILL.md"),
        root.join("skills").join(key).join("ref/a.md"),
    ] {
        assert_eq!(mode(&p), 0o600, "{}", p.display());
    }
    assert_eq!(
        s.svc.current_hash(&format!("obs1:{key}")),
        foo["content_hash"].as_str().map(str::to_string)
    );
}

#[test]
fn test_observe_stores_index_and_private_files_via_hub() {
    let (t, a) = obs_hub(FakeEvalRunner::new());
    t.hub.init_content().unwrap();
    let home = t.root.join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    put_client(
        &t,
        "obs1",
        &json!({"adapter": "fakeobs", "display_name": "Obs", "roots": {"home": home.to_string_lossy()}}),
    )
    .unwrap();
    a.set(
        vec![
            oskill("devops/foo", None, "agent", "Do it.\n", &[("ref/a.md", b"A")]),
            oskill("bar", None, "shipped", "Do it.\n", &[]),
        ],
        true,
    );
    let out = t.hub.observe("obs1", "hub").unwrap();
    assert_eq!(out["total"], 2);
    assert_eq!(out["by_origin"], json!({"agent": 1, "shipped": 1}));
    assert_eq!(out["complete"], true);
    assert_eq!(
        out["by_hub_status"],
        json!({"in_hub_same": 0, "in_hub_differs": 0, "not_in_hub": 2})
    );
    assert_eq!(out["notes"], json!(["note one"]));
}

#[test]
fn test_a_new_observation_replaces_the_old_atomically() {
    let s = standalone();
    store(&s, vec![os1("one"), os1("two")]);
    let root = s.data().join("observed");
    store(&s, vec![os1("three")]);
    let idx: Value = serde_json::from_str(&std::fs::read_to_string(root.join("obs1/index.json")).unwrap()).unwrap();
    assert_eq!(
        idx["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["three"]
    );
    let names = |d: &std::path::Path| {
        let mut v: Vec<String> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        names(&root.join("obs1/skills")),
        [idx["skills"][0]["key"].as_str().unwrap()]
    );
    assert_eq!(names(&root), ["obs1"], "no temp or .old directories left");
    // a failure while building leaves the previous observation untouched and leaves no temp directory
    s.obs.set_write_failpoint(Some("/.tmp-".into()));
    let e = s
        .obs
        .store("obs1", &observation(vec![os1("four")], true))
        .err()
        .unwrap();
    assert_eq!(e.code, "observe_store_failed");
    assert!(e.detail.contains("could not store"));
    s.obs.set_write_failpoint(None);
    let idx: Value = serde_json::from_str(&std::fs::read_to_string(root.join("obs1/index.json")).unwrap()).unwrap();
    assert_eq!(idx["skills"][0]["path"], "three");
    assert_eq!(names(&root), ["obs1"]);
}

#[test]
fn test_caps_unsafe_paths_and_secrets() {
    let s = standalone();
    let svc = ObserveService::new(&s.data().join("observed-caps"), std::sync::Weak::new(), None).with_limits(2, 100);
    let leak = format!("sk-{}", "A1b2C3d4E5f6G7h8I9j0");
    let mut c = os1("c");
    c.description = format!("key {leak}");
    c.files = [("SKILL.md".to_string(), vec![b'z'; 200])].into_iter().collect();
    let sk = vec![
        oskill("a", None, "agent", &"x".repeat(10), &[]),
        os1("../escape"),
        oskill("b", None, "agent", "Do it.\n", &[("../../evil", b"x"), ("ok.md", b"y")]),
        c,
        os1("d"),
        os1("e"),
    ];
    let obs = Observation {
        skills: sk,
        notes: vec![format!("token: {leak}")],
        ..Observation::default()
    };
    let idx = svc.store("obs1", &obs).unwrap();
    assert_eq!(
        idx["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    let notes: Vec<String> = idx["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap().to_string())
        .collect();
    assert_eq!(idx["complete"], false);
    assert!(notes.iter().any(|n| n.contains("unsafe")) && notes.iter().any(|n| n.contains("2 skills")));
    assert!(!idx.to_string().contains(&leak));
    let b = idx["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["path"] == "b")
        .unwrap();
    assert_eq!(b["files"], 2, "../../evil dropped, ok.md kept");
    let svc2 =
        ObserveService::new(&s.data().join("observed-caps2"), std::sync::Weak::new(), None).with_limits(5000, 100);
    let mut big = os1("big");
    big.files = [("SKILL.md".to_string(), vec![b'z'; 500])].into_iter().collect();
    let out = svc2
        .store(
            "obs1",
            &Observation {
                skills: vec![os1("small"), big],
                ..Observation::default()
            },
        )
        .unwrap();
    assert_eq!(
        out["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["small"]
    );
    assert_eq!(out["complete"], false);
    assert_eq!(
        svc.store("../x", &Observation::default()).err().map(|p| p.status),
        Some(400)
    );
    assert!(svc.entry("obs1", "../x").is_none() && svc.index("A").is_none());
}

#[test]
fn test_incomplete_adapter_flag_is_kept() {
    let (t, a) = obs_hub(FakeEvalRunner::new());
    t.hub.init_content().unwrap();
    put_client(
        &t,
        "obs1",
        &json!({"adapter": "fakeobs", "display_name": "Obs", "roots": {"home": wsc_support::client_home(&t.root).to_string_lossy()}}),
    )
    .unwrap();
    a.set(vec![os1("one")], false);
    assert_eq!(t.hub.observe("obs1", "hub").unwrap()["complete"], false);
    assert_eq!(
        t.hub.observed_view("obs1", None, None, None, 100, None).unwrap()["complete"],
        false
    );
}

#[test]
fn test_default_observe_wraps_discover() {
    let (t, _) = obs_hub(FakeEvalRunner::new());
    t.hub.init_content().unwrap();
    let home = t.root.join("client-home");
    std::fs::create_dir_all(home.join("skills/found")).unwrap();
    std::fs::write(home.join("skills/found/SKILL.md"), md("found", "Do it.\n")).unwrap();
    put_client(
        &t,
        "fake1",
        &json!({"adapter": "fake", "display_name": "Fake 1", "roots": {"home": home.to_string_lossy()}}),
    )
    .unwrap();
    let out = t.hub.observe("fake1", "hub").unwrap();
    assert_eq!(out["total"], 1);
    assert_eq!(out["by_origin"], json!({"client": 1}));
    assert_eq!(
        t.hub.observed_view("fake1", None, None, None, 100, None).unwrap()["items"][0]["path"],
        "found"
    );
}

// ---- comparison with the hub ----------------------------------------------------------------------------------------
/// The comparison (`annotated`) against a hub view built like `Hub._hub_view` from the standalone service's hashes
/// (Python builds it from the content catalog, WS-B1: see `..._via_hub`).
#[test]
fn test_hub_status_same_differs_not_in_hub_and_enabled() {
    let s = standalone();
    s.skill("alpha");
    s.skill("beta");
    let same_files = s.svc.snapshot("hub:alpha").unwrap().files;
    let content = s.content();
    let svc_root = s.eval_root();
    let enabled: BTreeSet<String> = ["alpha".to_string()].into_iter().collect();
    let view_svc = agent_hub::services::skilleval::SkillEvalService::new(
        &svc_root,
        &content,
        std::sync::Weak::new(),
        None,
        200,
        4 << 20,
        &s.data().join("observed"),
    );
    let vs = Arc::new(view_svc);
    let content2 = content;
    s.obs.set_hub_view(Some(Arc::new(move |_client: &str| {
        let mut v = HubView::default();
        let mut names: Vec<String> = std::fs::read_dir(content2.join("skills"))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        for n in names {
            let h = vs.current_hash(&format!("hub:{n}"));
            v.hash_of.insert(n.clone(), h.clone());
            if let Some(h) = h {
                v.by_hash.entry(h).or_default().push(n.clone());
            }
            if enabled.contains(&n) {
                v.enabled.insert(n);
            }
        }
        v
    })));
    let mut same = os1("x/alpha");
    same.name = "alpha".into();
    same.files = same_files;
    store(
        &s,
        vec![
            same,
            oskill("beta", None, "agent", "Different body.\n", &[]),
            os1("gamma"),
        ],
    );
    let ann = s.obs.annotated("obs1").unwrap();
    let items = ann["items"].as_array().unwrap();
    assert_eq!(
        ObserveService::count(items)["by_hub_status"],
        json!({"in_hub_same": 1, "in_hub_differs": 1, "not_in_hub": 1})
    );
    let by: std::collections::HashMap<String, Value> = items
        .iter()
        .map(|i| (i["name"].as_str().unwrap().to_string(), i.clone()))
        .collect();
    assert_eq!(
        (by["alpha"]["hub_status"].as_str(), by["alpha"]["hub_name"].as_str()),
        (Some("in_hub_same"), Some("alpha"))
    );
    assert_eq!(by["alpha"]["enabled_for_client"], true);
    assert_eq!(
        by["alpha"]["content_hash"].as_str().map(str::to_string),
        s.svc.current_hash("hub:alpha")
    );
    assert_eq!(
        (by["beta"]["hub_status"].as_str(), by["beta"]["hub_name"].as_str()),
        (Some("in_hub_differs"), Some("beta"))
    );
    assert_eq!(by["beta"]["enabled_for_client"], false);
    assert_eq!(by["gamma"]["hub_status"], "not_in_hub");
    assert!(by["gamma"]["hub_name"].is_null());
    // computed on read: a new hub skill changes the answer without a new observation
    s.skill_with("gamma", "Do it.\n", &[]);
    let ann = s.obs.annotated("obs1").unwrap();
    let g = ann["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "gamma")
        .unwrap()["hub_status"]
        .clone();
    assert!(g == "in_hub_differs" || g == "in_hub_same");
}

#[test]
fn test_hub_status_same_differs_not_in_hub_and_enabled_via_hub() {
    let (t, a) = obs_hub(FakeEvalRunner::new());
    t.hub.init_content().unwrap();
    put_client(
        &t,
        "obs1",
        &json!({"adapter": "fakeobs", "display_name": "Obs", "roots": {"home": wsc_support::client_home(&t.root).to_string_lossy()}}),
    )
    .unwrap();
    put(
        &t,
        "skill",
        "alpha",
        &json!({"description": "Alpha skill", "body": "Do alpha.\n", "files": {"ref/notes.md": "notes"}}),
    )
    .unwrap();
    put(
        &t,
        "skill",
        "beta",
        &json!({"description": "Alpha skill", "body": "Do alpha.\n", "files": {"ref/notes.md": "notes"}}),
    )
    .unwrap();
    let same_files = t.hub.skilleval.snapshot("hub:alpha").unwrap().files;
    let mut same = os1("x/alpha");
    same.name = "alpha".into();
    same.files = same_files;
    a.set(
        vec![
            same,
            oskill("beta", None, "agent", "Different body.\n", &[]),
            os1("gamma"),
        ],
        true,
    );
    put_profile(&t, "obs1", &json!({"enable": {"skills": ["alpha"]}})).unwrap();
    let out = t.hub.observe("obs1", "hub").unwrap();
    assert_eq!(
        out["by_hub_status"],
        json!({"in_hub_same": 1, "in_hub_differs": 1, "not_in_hub": 1})
    );
    let view = t.hub.observed_view("obs1", None, None, None, 100, None).unwrap();
    let alpha = view["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "alpha")
        .unwrap()
        .clone();
    assert_eq!(alpha["enabled_for_client"], true);
    assert_eq!(
        t.hub
            .observed_view("obs1", None, Some("in_hub_same"), None, 100, None)
            .unwrap()["items"][0]["name"],
        "alpha"
    );
    let q = t
        .hub
        .observed_view("obs1", Some("agent"), None, Some("ALP"), 100, None)
        .unwrap();
    assert_eq!(
        q["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["alpha"]
    );
    assert_eq!(
        t.hub
            .observed_view("obs1", Some("nope"), None, None, 100, None)
            .err()
            .map(|p| p.status),
        Some(400)
    );
}

#[tokio::test]
async fn test_observed_api_clients_and_drift_carry_the_summary() {
    let (t, a) = obs_hub(FakeEvalRunner::new());
    t.hub.init_content().unwrap();
    put_client(
        &t,
        "obs1",
        &json!({"adapter": "fakeobs", "display_name": "Obs", "roots": {"home": wsc_support::client_home(&t.root).to_string_lossy()}}),
    )
    .unwrap();
    let app_ = app(&t);
    let h = bearer(&t.key("ev-admin", "admin"));
    let hdr = [("authorization", h.as_str())];
    assert_eq!(
        api(&app_, "GET", "/api/v1/clients/obs1/observed", &hdr, None)
            .await
            .code(),
        "not_observed"
    );
    assert!(api(&app_, "GET", "/api/v1/clients/obs1", &hdr, None).await.json()["observed_summary"].is_null());
    a.set(vec![os1("one"), oskill("two", None, "shipped", "Do it.\n", &[])], true);
    let posted = api(&app_, "POST", "/api/v1/clients/obs1/observe", &hdr, Some(json!({})))
        .await
        .json();
    assert_eq!(
        (posted["total"].as_i64(), posted["client"].as_str()),
        (Some(2), Some("obs1"))
    );
    let body = api(&app_, "GET", "/api/v1/clients/obs1/observed?origin=shipped", &hdr, None)
        .await
        .json();
    assert_eq!(body["summary"]["total"], 2);
    assert_eq!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["two"]
    );
    assert!(body["next_cursor"].is_null() && body["observed_at"].is_number() && body["source"] == "fake container");
    let summ = api(&app_, "GET", "/api/v1/clients/obs1", &hdr, None).await.json()["observed_summary"].clone();
    assert_eq!(
        (
            summ["total"].as_i64(),
            summ["not_in_hub"].as_i64(),
            summ["in_hub_differs"].as_i64()
        ),
        (Some(2), Some(2), Some(0))
    );
    assert_eq!(summ["by_origin"], json!({"agent": 1, "shipped": 1}));
    put_profile(&t, "obs1", &json!({})).unwrap();
    t.hub.commit("c", "t").unwrap();
    assert_eq!(
        api(&app_, "GET", "/api/v1/clients/obs1/drift", &hdr, None).await.json()["observed"]["total"],
        2
    );
    assert_eq!(
        api(&app_, "GET", "/api/v1/clients/nope/observed", &hdr, None)
            .await
            .status,
        404
    );
    assert_eq!(
        api(&app_, "GET", "/api/v1/clients/obs1/observed?hub_status=bad", &hdr, None)
            .await
            .status,
        400
    );
}

// ---- reuse by content hash ------------------------------------------------------------------------------------------
#[test]
fn test_identical_content_is_reused_without_a_process_and_force_reruns() {
    let s = standalone();
    let r = use_default(&s.svc);
    s.skill("alpha");
    let mut o = os1("alpha");
    o.files = s.svc.snapshot("hub:alpha").unwrap().files;
    let idx = store(&s, vec![o]);
    let key = key_of(&idx, "alpha");
    let first = s.svc.evaluate("alpha", "tier1", "t").unwrap();
    let n = r.tool_calls().len();
    assert!(first["reused_from"].is_null() && first["evaluator_version"] == "0.5.0" && first["subject"] == "hub:alpha");
    let out = s
        .svc
        .enqueue(&[format!("obs1:{key}"), "hub:alpha".into()], "static", false, "t")
        .unwrap();
    assert_eq!(out["queued"], json!([format!("obs1:{key}"), "hub:alpha"]));
    s.svc.join(Some(10.0));
    assert_eq!(r.tool_calls().len(), n, "both reused: no process started");
    let rec = s.svc.records(&format!("obs1:{key}"), 20)[0].clone();
    assert_eq!(
        rec["reused_from"],
        json!({"subject": "hub:alpha", "run_id": first["run_id"]})
    );
    assert_eq!(rec["tiers"], "tier1");
    assert_eq!(
        (
            rec["subject"].clone(),
            rec["source"].as_str(),
            rec["path"].as_str(),
            rec["origin"].as_str()
        ),
        (json!(format!("obs1:{key}")), Some("obs1"), Some("alpha"), Some("agent"))
    );
    assert_eq!(rec["verdict"], first["verdict"]);
    assert_eq!(rec["validators"], first["validators"]);
    assert_eq!(
        s.svc.records("hub:alpha", 20)[0]["reused_from"]["run_id"],
        first["run_id"]
    );
    let rep = s
        .svc
        .report(&format!("obs1:{key}"), rec["run_id"].as_str().unwrap())
        .unwrap();
    assert!(
        rep["markdown"].as_str().is_some_and(|m| !m.is_empty()),
        "a reused run shows the original report"
    );
    assert_eq!(rep["subject"], json!(format!("obs1:{key}")));
    let runs = s.eval_root().join("@observed/obs1").join(&key);
    let names: Vec<String> = std::fs::read_dir(&runs)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, [rec["run_id"].as_str().unwrap()]);
    s.svc.enqueue(&[format!("obs1:{key}")], "static", true, "t").unwrap(); // force: a real run
    s.svc.join(Some(10.0));
    assert_eq!(r.tool_calls().len(), n + 1);
    assert!(s.svc.records(&format!("obs1:{key}"), 20)[0]["reused_from"].is_null());
}

#[test]
fn test_higher_tiers_or_another_version_or_a_changed_skill_are_not_reused() {
    let s = standalone();
    let r = use_default(&s.svc);
    let d = s.skill("alpha");
    s.svc.evaluate("alpha", "static", "t").unwrap();
    let n = r.tool_calls().len();
    s.svc.evaluate_force("alpha", "static", "t", false).unwrap();
    assert_eq!(r.tool_calls().len(), n, "same tiers, same version: reused");
    s.svc.evaluate_force("alpha", "tier1", "t", false).unwrap();
    assert_eq!(r.tool_calls().len(), n + 2, "tiers requested are higher: real run");
    s.svc.reset_version_cache();
    r.state().version_out = "skillevaluator, version 0.6.0".into();
    s.svc.evaluate_force("alpha", "static", "t", false).unwrap();
    assert_eq!(r.tool_calls().len(), n + 3, "another evaluator version: real run");
    let mut b = std::fs::read(d.join("SKILL.md")).unwrap();
    b.extend_from_slice(b"more\n");
    std::fs::write(d.join("SKILL.md"), b).unwrap();
    s.svc.evaluate_force("alpha", "static", "t", false).unwrap();
    assert_eq!(r.tool_calls().len(), n + 4, "other content: real run");
}

#[test]
fn test_error_runs_are_never_reused() {
    let s = standalone();
    let r = use_(
        &s.svc,
        FakeEvalRunner::tier1(Canned::none(2, "")),
        LAUNCHERS,
        None,
        None,
    );
    s.skill("alpha");
    assert_eq!(s.svc.evaluate("alpha", "static", "t").unwrap()["verdict"], "error");
    assert!(!s.eval_root().join("@by-hash").exists());
    s.svc.evaluate_force("alpha", "static", "t", false).unwrap();
    assert_eq!(r.tool_calls().len(), 2);
}

// ---- the queue ------------------------------------------------------------------------------------------------------
#[test]
fn test_queue_is_fifo_dedupes_and_cancels() {
    let s = standalone();
    let gate = Event::default();
    let r = use_(
        &s.svc,
        FakeEvalRunner::with(None, None, None, Some(gate.clone())),
        LAUNCHERS,
        None,
        None,
    );
    for n in ["a1", "b1", "c1", "d1"] {
        s.skill(n);
    }
    let subj = |v: &[&str]| v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
    let out = s
        .svc
        .enqueue(&subj(&["hub:a1", "hub:b1", "hub:c1", "hub:b1"]), "static", false, "t")
        .unwrap();
    assert_eq!(out["queued"], json!(["hub:a1", "hub:b1", "hub:c1"]));
    assert_eq!(out["skipped"], json!([{"subject": "hub:b1", "reason": "queued"}]));
    assert!(r.started.wait(5.0));
    let again = s
        .svc
        .enqueue(&subj(&["hub:a1", "hub:c1", "hub:d1", "hub:nope"]), "static", false, "t")
        .unwrap();
    assert_eq!(again["queued"], json!(["hub:d1"]));
    let skipped: BTreeSet<(String, String)> = again["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["subject"].as_str().unwrap().into(),
                x["reason"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        skipped,
        [("hub:a1", "running"), ("hub:c1", "queued"), ("hub:nope", "not_found")]
            .iter()
            .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
            .collect()
    );
    let q = s.svc.queue().unwrap();
    assert_eq!(q["running"]["subject"], "hub:a1");
    assert_eq!(
        q["queued"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["subject"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["hub:b1", "hub:c1", "hub:d1"]
    );
    assert_eq!(
        (q["queued_total"].as_i64(), q["finished_since_start"].as_i64()),
        (Some(3), Some(0))
    );
    let cancelled = s.svc.cancel().unwrap();
    assert_eq!(cancelled, json!({"cancelled_queued": 3, "killed_running": true}));
    assert_eq!(r.state().cancelled, 1);
    gate.set();
    s.svc.join(Some(10.0));
    assert_eq!(r.names_in_order(), ["a1"]);
    let q = s.svc.queue().unwrap();
    assert!(q["running"].is_null() && q["queued"] == json!([]));
    assert_eq!(q["errors_since_start"], 1, "the killed one: cancelled");
    assert_eq!(s.svc.records("hub:a1", 20)[0]["error"], "cancelled");
    // order of a fresh batch
    let gate2 = Event::default();
    gate2.set();
    let r2 = use_(
        &s.svc,
        FakeEvalRunner::with(None, None, None, Some(gate2)),
        LAUNCHERS,
        None,
        None,
    );
    s.svc
        .enqueue(&subj(&["hub:d1", "hub:b1", "hub:c1"]), "static", false, "t")
        .unwrap();
    s.svc.join(Some(10.0));
    assert_eq!(r2.names_in_order(), ["d1", "b1", "c1"]);
    assert_eq!(s.svc.queue().unwrap()["finished_since_start"], 4);
}

#[test]
fn test_enqueue_validation_and_missing_evaluator() {
    let s = standalone();
    use_(&s.svc, FakeEvalRunner::new(), &[], None, None);
    s.skill("alpha");
    let one = ["hub:alpha".to_string()];
    assert_eq!(
        s.svc.enqueue(&one, "static", false, "t").err().map(|p| p.status),
        Some(503)
    );
    use_default(&s.svc);
    assert_eq!(
        s.svc.enqueue(&one, "tier9", false, "t").err().map(|p| p.status),
        Some(422)
    );
    assert_eq!(
        s.svc
            .enqueue(&["hub:alpha".into(), "hub:../x".into()], "static", false, "t")
            .err()
            .map(|p| p.status),
        Some(400)
    );
    assert_eq!(s.svc.queue().unwrap()["queued_total"], 0);
}

#[test]
fn test_interrupted_runs_are_recorded_as_errors() {
    let s = standalone();
    use_default(&s.svc);
    s.skill("alpha");
    let ghost = s.eval_root().join("alpha/20260101T000000000Z-0a0a0a0a");
    std::fs::create_dir_all(&ghost).unwrap();
    s.svc.enqueue(&["hub:alpha".into()], "static", true, "t").unwrap();
    s.svc.join(Some(10.0));
    let rec = s
        .svc
        .records("hub:alpha", 20)
        .into_iter()
        .find(|x| x["run_id"] == "20260101T000000000Z-0a0a0a0a")
        .unwrap();
    assert_eq!(
        (rec["verdict"].as_str(), rec["error"].as_str()),
        (Some("error"), Some("interrupted"))
    );
}

// ---- the gate -------------------------------------------------------------------------------------------------------
#[test]
fn test_gate_missing_blocked_warn_ok() {
    let s = standalone();
    assert_eq!(s.svc.gate(&"0".repeat(64))["status"], "missing");
    assert_eq!(s.svc.gate("junk")["status"], "missing");
    // ok: the publication validator has a HIGH finding (Author not specified) and does not matter
    let h = evaluated(&s, FakeEvalRunner::tier1(canned(&[])), "okskill", "static");
    let g = s.svc.gate(&h);
    assert_eq!(
        (
            g["status"].as_str(),
            g["reasons"].clone(),
            g["verdict"].as_str(),
            g["tiers"].as_str()
        ),
        (Some("ok"), json!([]), Some("failed"), Some("static"))
    );
    // warn: medium security findings ALONE
    let h = evaluated(
        &s,
        FakeEvalRunner::tier1(canned(&[("PII_Scan", "failed", &[("medium", "email address")])])),
        "medskill",
        "static",
    );
    let g = s.svc.gate(&h);
    assert_eq!(
        (g["status"].as_str(), g["reasons"].clone()),
        (Some("warn"), json!(["PII Scan: 1 medium finding(s)"]))
    );
    // M1 fail closed: a security scan that did not finish BLOCKS, naming it
    for st in ["incomplete", "error", "skipped", "weird"] {
        let h = evaluated(
            &s,
            FakeEvalRunner::tier1(canned(&[("Security_Scan", st, &[])])),
            &format!("inc{st}"),
            "static",
        );
        let g = s.svc.gate(&h);
        assert_eq!(g["status"], "blocked", "{st}");
        assert_eq!(
            g["reasons"],
            json!([format!("Security Scan: status {st} (the scan did not finish)")]),
            "{st}"
        );
    }
    // blocked: a high finding of a security validator, with reasons "validator: message"
    let h = evaluated(
        &s,
        FakeEvalRunner::tier1(canned(&[(
            "Code_Risk_Analysis",
            "failed",
            &[("high", "eval of a remote string")],
        )])),
        "badskill",
        "static",
    );
    let g = s.svc.gate(&h);
    assert_eq!(
        (g["status"].as_str(), g["reasons"].clone()),
        (Some("blocked"), json!(["Code Risk Analysis: eval of a remote string"]))
    );
    assert!(g["run_id"].is_string());
    let h = evaluated(
        &s,
        FakeEvalRunner::tier1(canned(&[(
            "Secrets_Detection",
            "failed",
            &[("critical", "private key")],
        )])),
        "keyskill",
        "static",
    );
    assert_eq!(s.svc.gate(&h)["status"], "blocked");
}

#[test]
fn test_gate_requires_the_expected_security_validators_to_be_present() {
    let s = standalone();
    for gone in [
        "Security Scan",
        "PII Scan",
        "Code Risk Analysis",
        "Secrets Detection",
        "Code Integrity & Hygiene",
        "Unicode Smuggling Detection",
    ] {
        let mut c = canned(&[]);
        let rep = c.report.as_mut().unwrap();
        let kept: Vec<Value> = rep["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| x["validator"] != gone)
            .cloned()
            .collect();
        rep["results"] = Value::Array(kept);
        let name = format!("no{}", gone.split(' ').next().unwrap().to_lowercase());
        let h = evaluated(&s, FakeEvalRunner::tier1(c), &name, "static");
        let g = s.svc.gate(&h);
        assert_eq!(g["status"], "blocked", "{gone}");
        assert_eq!(
            g["reasons"],
            json!([format!("{gone}: validator missing from the report")]),
            "{gone}"
        );
    }
    let mut empty = canned(&[]);
    empty.report.as_mut().unwrap()["results"] = json!([]);
    let h = evaluated(&s, FakeEvalRunner::tier1(empty), "emptyreport", "static");
    assert_eq!(
        s.svc.gate(&h)["status"],
        "missing",
        "no validators at all: not evaluated"
    );
}

#[test]
fn test_unknown_validators_count_as_security_and_the_judge_never_gates() {
    let s = standalone();
    let h = evaluated(
        &s,
        FakeEvalRunner::tier1(canned(&[("Brand_New_Scanner", "failed", &[("high", "something new")])])),
        "unk",
        "static",
    );
    let g = s.svc.gate(&h);
    assert_eq!(
        (g["status"].as_str(), g["reasons"].clone()),
        (Some("blocked"), json!(["Brand New Scanner: something new"]))
    );
    for name in [
        "QUALITY",
        "SCRIPT_LINT",
        "RUBRIC_EVAL",
        "License_Compliance",
        "Semantic_Version_Validation",
    ] {
        let skill = format!("n{}", name.to_lowercase().replace('_', "-"));
        let h = evaluated(
            &s,
            FakeEvalRunner::tier1(canned(&[(name, "failed", &[("critical", "nope")])])),
            &skill,
            "static",
        );
        assert_eq!(s.svc.gate(&h)["status"], "ok", "{name}");
    }
}

#[test]
fn test_llm_pass_can_only_add_to_the_gate_never_clear_it() {
    let s = standalone();
    let clean = canned(&[]);
    let noise = canned(&[
        ("RUBRIC_EVAL", "failed", &[("critical", "judge says no")]),
        ("Security_Scan", "incomplete", &[("medium", "meh")]),
    ]);
    let h = evaluated(
        &s,
        FakeEvalRunner::with(Some(clean.clone()), None, Some(noise), None),
        "judged",
        "tier1",
    );
    assert_eq!(s.svc.gate(&h)["status"], "ok", "judge + LLM medium/incomplete: ignored");
    let blocking_llm = canned(&[(
        "Code_Risk_Analysis",
        "failed",
        &[("high", "llm found exec"), ("medium", "x")],
    )]);
    let h = evaluated(
        &s,
        FakeEvalRunner::with(Some(clean), None, Some(blocking_llm), None),
        "llmblock",
        "tier1",
    );
    let g = s.svc.gate(&h);
    assert_eq!(
        (g["status"].as_str(), g["reasons"].clone(), g["tiers"].as_str()),
        (
            Some("blocked"),
            json!(["Code Risk Analysis: llm found exec"]),
            Some("tier1")
        )
    );
    let static_high = canned(&[("Code_Risk_Analysis", "failed", &[("high", "static hit")])]);
    let downgraded = canned(&[("Code_Risk_Analysis", "passed", &[])]); // the LLM verifier "cleared" it
    let h = evaluated(
        &s,
        FakeEvalRunner::with(Some(static_high), None, Some(downgraded), None),
        "cleared",
        "tier1",
    );
    let g = s.svc.gate(&h);
    assert_eq!(
        (g["status"].as_str(), g["reasons"].clone()),
        (Some("blocked"), json!(["Code Risk Analysis: static hit"]))
    );
    let rec = s.svc.records("hub:cleared", 20)[0].clone();
    assert!(
        rec["validators"].as_array().is_some_and(|v| !v.is_empty())
            && rec["validators_llm"].as_array().is_some_and(|v| !v.is_empty())
    );
    assert_eq!(rec["static"]["status"], "failed");
    let v0 = rec["validators"][0].as_object().unwrap();
    assert!(["validator", "status", "severity"].iter().all(|k| v0.contains_key(*k)));
}

/// Through the HUB's service, so the gate reads the hub setting `eval_gate_min_tiers` (PUT /settings is CORE). The
/// evaluated subject is an observed skill (no content lock needed); Python uses a hub skill: the gate is per content.
#[test]
fn test_gate_min_tiers_and_error_runs() {
    let r = FakeEvalRunner::tier1(canned(&[]));
    let t = hub_with(
        agent_hub::services::hub::HubOptions {
            eval_runner: Some(r.clone()),
            ..Default::default()
        },
        |_| {},
    );
    use_(&t.hub.skilleval, r, LAUNCHERS, None, None);
    let idx = t
        .hub
        .observe_svc
        .store("obs1", &observation(vec![os1("gm")], true))
        .unwrap();
    let subject = format!("obs1:{}", key_of(&idx, "gm"));
    let rec = t.hub.skilleval.evaluate(&subject, "static", "t").unwrap();
    let h = rec["content_hash"].as_str().unwrap().to_string();
    assert_eq!(t.hub.skilleval.gate(&h)["status"], "ok");
    let patch: serde_json::Map<String, Value> = [("eval_gate_min_tiers".to_string(), json!("tier1"))]
        .into_iter()
        .collect();
    t.hub.update_settings(&patch).unwrap();
    assert_eq!(
        t.hub.skilleval.gate(&h)["status"],
        "missing",
        "a static run is below tier1"
    );
    t.hub.skilleval.evaluate(&subject, "tier1", "t").unwrap();
    let g = t.hub.skilleval.gate(&h);
    assert_eq!((g["status"].as_str(), g["tiers"].as_str()), (Some("ok"), Some("tier1")));
    t.hub.skilleval.evaluate(&subject, "static", "t").unwrap(); // a later weaker run keeps the better one
    assert_eq!(t.hub.skilleval.gate(&h)["tiers"], "tier1");
}

// ---- settings -------------------------------------------------------------------------------------------------------
#[tokio::test]
async fn test_enum_settings_validate_persist_and_default() {
    let t = hub();
    let a = app(&t);
    let h = bearer(&t.key("ev-admin", "admin"));
    let hdr = [("authorization", h.as_str())];
    let cur = api(&a, "GET", "/api/v1/settings", &hdr, None).await.json();
    assert_eq!(
        (
            cur["editable"]["eval_gate"].as_str(),
            cur["editable"]["eval_gate_min_tiers"].as_str()
        ),
        (Some("advisory"), Some("static"))
    );
    assert_eq!(cur["editable"]["eval_gate_observed"], "require");
    assert_eq!(
        cur["enums"]["eval_gate_observed"],
        json!(["off", "advisory", "require"])
    );
    assert_eq!(cur["enums"]["eval_gate"], json!(["off", "advisory", "require"]));
    for bad in [
        json!({"eval_gate": "strict"}),
        json!({"eval_gate_min_tiers": "tier2"}),
        json!({"eval_gate": 1}),
        json!({"eval_gate": ["off"]}),
        json!({"eval_gate": "require", "plan_retention": 0}),
        json!({"nope": "x"}),
    ] {
        assert_eq!(
            api(
                &a,
                "PUT",
                "/api/v1/settings",
                &[("authorization", h.as_str()), ("x-agent-hub", "1")],
                Some(bad.clone())
            )
            .await
            .status,
            422,
            "{bad}"
        );
    }
    assert_eq!(
        api(&a, "GET", "/api/v1/settings", &hdr, None).await.json()["editable"]["eval_gate"],
        "advisory",
        "a bad patch changed nothing"
    );
    let ok = api(
        &a,
        "PUT",
        "/api/v1/settings",
        &[("authorization", h.as_str())],
        Some(json!({"eval_gate": "require", "eval_gate_min_tiers": "tier1+2"})),
    )
    .await;
    assert_eq!(ok.status, 200);
    assert_eq!(ok.json()["editable"]["eval_gate"], "require");
    assert_eq!(t.hub.setting_str("eval_gate_min_tiers"), "tier1+2");
}

#[test]
fn test_env_defaults_for_the_gate() {
    let root = tempfile::tempdir().unwrap();
    let env = |pairs: &[(&str, &str)]| -> std::collections::HashMap<String, String> {
        let mut m: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        m.insert("HOME".into(), root.path().to_string_lossy().into_owned());
        m.insert(
            "AGENT_HUB_ENV_FILE".into(),
            root.path().join("none").to_string_lossy().into_owned(),
        );
        m
    };
    let s = agent_hub::config::load_settings_from(
        &env(&[("HUB_EVAL_GATE", "require"), ("HUB_EVAL_GATE_MIN_TIERS", "tier1")]),
        &vec![],
    )
    .unwrap();
    assert_eq!(
        (s.eval_gate.as_str(), s.eval_gate_min_tiers.as_str()),
        ("require", "tier1")
    );
    assert!(agent_hub::config::load_settings_from(&env(&[("HUB_EVAL_GATE", "nonsense")]), &vec![]).is_err());
}

// ---- observed import with the gate (WS-B1 importer) -----------------------------------------------------------------
fn obs_client(t: &T) {
    t.hub.init_content().unwrap();
    put_client(
        t,
        "obs1",
        &json!({"adapter": "fakeobs", "display_name": "Obs", "roots": {"home": wsc_support::client_home(&t.root).to_string_lossy()}}),
    )
    .unwrap();
}

fn observe_one(t: &T, a: &ObsAdapter, sk: ObservedSkill) -> (String, String) {
    a.set(vec![sk], true);
    t.hub.observe("obs1", "hub").unwrap();
    let item = t.hub.observed_view("obs1", None, None, None, 100, None).unwrap()["items"][0].clone();
    (
        item["key"].as_str().unwrap().to_string(),
        item["content_hash"].as_str().unwrap().to_string(),
    )
}

fn eval_observed(t: &T, key: &str, r: Arc<FakeEvalRunner>) -> Value {
    use_(&t.hub.skilleval, r, LAUNCHERS, None, None);
    t.hub.skilleval.evaluate(&format!("obs1:{key}"), "static", "t").unwrap()
}

fn set_gate(t: &T, k: &str, v: &str) {
    let patch: serde_json::Map<String, Value> = [(k.to_string(), json!(v))].into_iter().collect();
    t.hub.update_settings(&patch).unwrap();
}

#[test]
fn test_observed_import_writes_provenance_and_does_not_enable() {
    let (t, a) = obs_hub(FakeEvalRunner::new());
    obs_client(&t);
    let (key, h) = observe_one(
        &t,
        &a,
        oskill("devops/foo", None, "agent", "Do it.\n", &[("ref/n.md", b"N")]),
    );
    let rec = eval_observed(&t, &key, FakeEvalRunner::tier1(canned(&[])));
    let out = t
        .hub
        .import_observed("obs1", &[json!({"key": key})], "skip", false, None, "hub")
        .unwrap();
    let row = &out["results"][0];
    assert_eq!(
        (row["status"].as_str(), row["final_name"].as_str(), row["gate"].as_str()),
        (Some("created"), Some("foo"), Some("ok"))
    );
    let side = t.hub.item_detail("skill", "foo").unwrap()["sidecar"].clone();
    assert_eq!(side["source"], "observed:obs1");
    let prov = &side["provenance"];
    assert_eq!(
        (prov["client"].as_str(), prov["path"].as_str(), prov["origin"].as_str()),
        (Some("obs1"), Some("devops/foo"), Some("agent"))
    );
    assert_eq!(prov["content_hash"], json!(h));
    assert_eq!(
        prov["evaluation"],
        json!({"run_id": rec["run_id"], "verdict": "failed", "gate": "ok"})
    );
    assert_eq!(std::fs::read(t.content().join("skills/foo/ref/n.md")).unwrap(), b"N");
    assert_eq!(out["enabled"], false);
    let again = t
        .hub
        .import_observed("obs1", &[json!({"key": key})], "skip", false, None, "hub")
        .unwrap()["results"][0]
        .clone();
    assert!(again["status"] == "skipped" && again["detail"].as_str().unwrap().contains("identical"));
    let renamed = t
        .hub
        .import_observed("obs1", &[json!({"key": key, "name": "bar"})], "skip", true, None, "hub")
        .unwrap()["results"][0]
        .clone();
    assert_eq!(renamed["status"], "created");
    let miss = t
        .hub
        .import_observed(
            "obs1",
            &[json!({"key": "nothere-00000000"})],
            "skip",
            false,
            None,
            "hub",
        )
        .unwrap()["results"][0]
        .clone();
    assert_eq!(miss["status"], "error");
    assert_eq!(
        t.hub
            .import_observed("obs1", &[json!({"key": key})], "weird", false, None, "hub")
            .err()
            .map(|p| p.status),
        Some(400)
    );
}

#[test]
fn test_observed_import_conflicts_replace_and_rename() {
    let (t, a) = obs_hub(FakeEvalRunner::new());
    obs_client(&t);
    set_gate(&t, "eval_gate_observed", "advisory");
    put(
        &t,
        "skill",
        "foo",
        &json!({"description": "x", "body": "Hub version.\n"}),
    )
    .unwrap();
    let (key, _) = observe_one(&t, &a, oskill("devops/foo", None, "agent", "Observed version.\n", &[]));
    let skipped = t
        .hub
        .import_observed("obs1", &[json!({"key": key})], "skip", false, None, "hub")
        .unwrap()["results"][0]
        .clone();
    assert_eq!(skipped["status"], "skipped");
    let renamed = t
        .hub
        .import_observed("obs1", &[json!({"key": key})], "rename", false, None, "hub")
        .unwrap()["results"][0]
        .clone();
    assert_eq!(
        (renamed["status"].as_str(), renamed["final_name"].as_str()),
        (Some("renamed"), Some("foo-2"))
    );
    let replaced = t
        .hub
        .import_observed("obs1", &[json!({"key": key})], "replace", false, None, "hub")
        .unwrap()["results"][0]
        .clone();
    assert_eq!(replaced["status"], "replaced");
    assert!(
        std::fs::read_to_string(t.content().join("skills/foo/SKILL.md"))
            .unwrap()
            .contains("Observed version")
    );
}

#[test]
fn test_require_refuses_missing_and_blocked_and_override_is_audited() {
    let (t, a) = obs_hub(FakeEvalRunner::new());
    obs_client(&t);
    set_gate(&t, "eval_gate_observed", "require");
    let (key, _) = observe_one(&t, &a, os1("devops/foo"));
    let refused = t
        .hub
        .import_observed("obs1", &[json!({"key": key})], "skip", false, None, "hub")
        .unwrap()["results"][0]
        .clone();
    assert_eq!(
        (
            refused["status"].as_str(),
            refused["code"].as_str(),
            refused["gate"].as_str()
        ),
        (Some("refused"), Some("eval_required"), Some("missing"))
    );
    eval_observed(
        &t,
        &key,
        FakeEvalRunner::tier1(canned(&[(
            "Code_Risk_Analysis",
            "failed",
            &[("high", "eval of remote string")],
        )])),
    );
    let blocked = t
        .hub
        .import_observed("obs1", &[json!({"key": key})], "skip", false, None, "hub")
        .unwrap()["results"][0]
        .clone();
    assert_eq!(
        (blocked["status"].as_str(), blocked["code"].as_str()),
        (Some("refused"), Some("eval_blocked"))
    );
    assert!(blocked["detail"].as_str().unwrap().contains("eval of remote string"));
    assert_eq!(
        t.hub
            .import_observed("obs1", &[json!({"key": key})], "skip", false, Some("too short"), "hub")
            .err()
            .map(|p| p.status),
        Some(422)
    );
    let long = "x".repeat(501);
    assert_eq!(
        t.hub
            .import_observed("obs1", &[json!({"key": key})], "skip", false, Some(&long), "hub")
            .err()
            .map(|p| p.status),
        Some(422)
    );
    let ok = t
        .hub
        .import_observed(
            "obs1",
            &[json!({"key": key})],
            "skip",
            false,
            Some("reviewed by hand, the eval is a known false positive"),
            "alice",
        )
        .unwrap();
    assert_eq!(
        (ok["results"][0]["status"].as_str(), ok["results"][0]["gate"].as_str()),
        (Some("created"), Some("overridden"))
    );
    let page = t.hub.audit_svc.list_page(50, None).unwrap();
    let ev: Vec<&Value> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["path"] == "skill.import.override")
        .collect();
    assert_eq!(ev.len(), 1);
    assert_eq!(
        (
            ev[0]["actor"].as_str(),
            ev[0]["params"]["skill"].as_str(),
            ev[0]["params"]["gate_status"].as_str()
        ),
        (Some("alice"), Some("foo"), Some("blocked"))
    );
}

#[test]
fn test_require_passes_warn_and_ok_advisory_never_refuses_off_has_no_gate() {
    let (t, a) = obs_hub(FakeEvalRunner::new());
    obs_client(&t);
    let (key, _) = observe_one(&t, &a, os1("devops/foo"));
    set_gate(&t, "eval_gate_observed", "off");
    let row = t
        .hub
        .import_observed("obs1", &[json!({"key": key})], "skip", false, None, "hub")
        .unwrap()["results"][0]
        .clone();
    assert!(row["status"] == "created" && row["gate"].is_null());
    t.hub.work.delete("skill", "foo").unwrap();
    set_gate(&t, "eval_gate_observed", "advisory");
    let row = t
        .hub
        .import_observed("obs1", &[json!({"key": key})], "skip", false, None, "hub")
        .unwrap()["results"][0]
        .clone();
    assert!(row["status"] == "created" && row["gate"] == "missing");
    t.hub.work.delete("skill", "foo").unwrap();
    eval_observed(
        &t,
        &key,
        FakeEvalRunner::tier1(canned(&[("PII_Scan", "failed", &[("medium", "email address")])])),
    );
    set_gate(&t, "eval_gate_observed", "require");
    let row = t
        .hub
        .import_observed("obs1", &[json!({"key": key})], "skip", false, None, "hub")
        .unwrap()["results"][0]
        .clone();
    assert!(row["status"] == "created" && row["gate"] == "warn");
}

#[test]
fn test_import_of_discovered_skills_is_gated_too() {
    let (t, _) = obs_hub(FakeEvalRunner::new());
    t.hub.init_content().unwrap();
    let home = t.root.join("client-home");
    std::fs::create_dir_all(home.join("skills/disc")).unwrap();
    std::fs::write(home.join("skills/disc/SKILL.md"), md("disc", "Do it.\n")).unwrap();
    put_client(
        &t,
        "fake1",
        &json!({"adapter": "fake", "display_name": "Fake 1", "roots": {"home": home.to_string_lossy()}}),
    )
    .unwrap();
    set_gate(&t, "eval_gate", "require");
    let sel = [("skill".to_string(), "disc".to_string())];
    let none: indexmap::IndexMap<(String, String), String> = indexmap::IndexMap::new();
    let row = t
        .hub
        .import_items("fake1", Some(&sel), None, true, &none, None, "hub")
        .unwrap()["results"][0]
        .clone();
    assert_eq!(
        (row["status"].as_str(), row["code"].as_str()),
        (Some("refused"), Some("eval_required"))
    );
    use_(
        &t.hub.skilleval,
        FakeEvalRunner::tier1(canned(&[])),
        LAUNCHERS,
        None,
        None,
    );
    let mut o = os1("disc");
    o.files = [("SKILL.md".to_string(), md("disc", "Do it.\n"))].into_iter().collect();
    let idx = t.hub.observe_svc.store("fake1", &observation(vec![o], true)).unwrap();
    let k = key_of(&idx, "disc");
    t.hub.skilleval.evaluate(&format!("fake1:{k}"), "static", "t").unwrap();
    let row = t
        .hub
        .import_items("fake1", Some(&sel), None, true, &none, None, "hub")
        .unwrap()["results"][0]
        .clone();
    assert_eq!(
        (row["status"].as_str(), row["gate"].as_str()),
        (Some("created"), Some("ok"))
    );
    t.hub.work.delete("skill", "disc").unwrap();
    use_(
        &t.hub.skilleval,
        FakeEvalRunner::tier1(canned(&[("Code_Risk_Analysis", "failed", &[("high", "bad")])])),
        LAUNCHERS,
        None,
        None,
    );
    t.hub.skilleval.evaluate(&format!("fake1:{k}"), "static", "t").unwrap();
    assert_eq!(
        t.hub
            .import_items("fake1", Some(&sel), None, true, &none, None, "hub")
            .unwrap()["results"][0]["code"],
        "eval_blocked"
    );
    let row = t
        .hub
        .import_items(
            "fake1",
            Some(&sel),
            None,
            true,
            &none,
            Some("accepted after a manual review"),
            "hub",
        )
        .unwrap()["results"][0]
        .clone();
    assert_eq!(
        (row["status"].as_str(), row["gate"].as_str()),
        (Some("created"), Some("overridden"))
    );
}

// ---- listing, detail and run over HTTP ------------------------------------------------------------------------------
#[tokio::test]
async fn test_evaluations_listing_filters_summary_and_run() {
    let r = FakeEvalRunner::tier1(canned(&[]));
    let (t, a) = obs_hub(r.clone());
    obs_client(&t);
    alpha(&t);
    a.set(vec![os1("one"), oskill("two", None, "shipped", "Do it.\n", &[])], true);
    t.hub.observe("obs1", "hub").unwrap();
    let app_ = app(&t);
    let h = bearer(&t.key("ev-admin", "admin"));
    let hdr = [("authorization", h.as_str())];
    let get = |q: &str| format!("/api/v1/skill-evaluations{q}");
    let hub_only = api(&app_, "GET", &get(""), &hdr, None).await.json();
    assert_eq!(
        hub_only["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["skill"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["alpha"]
    );
    let all = api(&app_, "GET", &get("?source=all"), &hdr, None).await.json();
    assert_eq!(all["summary"]["total"], 3);
    assert_eq!(all["summary"]["by_state"], json!({"none": 3}));
    let run = api(
        &app_,
        "POST",
        "/api/v1/skill-evaluations/run",
        &hdr,
        Some(json!({"select": "none_or_stale", "source": "obs1"})),
    )
    .await
    .json();
    assert_eq!(run["queued"].as_array().unwrap().len(), 2);
    assert_eq!(run["skipped"], json!([]));
    t.hub.skilleval.join(Some(10.0));
    assert_eq!(r.names_in_order().len(), 2);
    let rows = api(&app_, "GET", &get("?source=obs1&state=current&gate=ok"), &hdr, None)
        .await
        .json();
    assert_eq!(rows["items"].as_array().unwrap().len(), 2);
    assert_eq!(rows["summary"]["by_gate"], json!({"ok": 2}));
    let again = api(
        &app_,
        "POST",
        "/api/v1/skill-evaluations/run",
        &hdr,
        Some(json!({"select": "none_or_stale", "source": "all"})),
    )
    .await
    .json();
    assert_eq!(again["queued"], json!(["hub:alpha"]));
    t.hub.skilleval.join(Some(10.0));
    for body in [
        json!({}),
        json!({"subjects": ["hub:alpha"], "select": "all"}),
        json!({"select": "bogus"}),
        json!({"tiers": "x", "select": "all"}),
    ] {
        assert_eq!(
            api(&app_, "POST", "/api/v1/skill-evaluations/run", &hdr, Some(body.clone()))
                .await
                .status,
            422,
            "{body}"
        );
    }
    assert_eq!(api(&app_, "GET", &get("?source=BAD/x"), &hdr, None).await.status, 400);
    assert_eq!(api(&app_, "GET", &get("?state=weird"), &hdr, None).await.status, 400);
    assert_eq!(
        api(
            &app_,
            "GET",
            "/api/v1/skill-evaluations/obs1:nothere-00000000",
            &hdr,
            None
        )
        .await
        .status,
        404
    );
    assert_eq!(
        api(&app_, "POST", "/api/v1/skill-evaluations/queue/cancel", &hdr, None)
            .await
            .json(),
        json!({"cancelled_queued": 0, "killed_running": false})
    );
}

#[tokio::test]
async fn test_viewer_can_read_but_not_post() {
    let r = FakeEvalRunner::new();
    let (t, a) = obs_hub(r.clone());
    obs_client(&t);
    alpha(&t);
    a.set(vec![os1("one")], true);
    t.hub.observe("obs1", "hub").unwrap();
    let app_ = app(&t);
    let v = bearer(&t.key("v", "viewer"));
    for path in [
        "/skill-evaluations",
        "/skill-evaluations/queue",
        "/clients/obs1/observed",
        "/skill-evaluations/hub:alpha",
    ] {
        assert_eq!(
            api(
                &app_,
                "GET",
                &format!("/api/v1{path}"),
                &[("authorization", v.as_str())],
                None
            )
            .await
            .status,
            200,
            "{path}"
        );
    }
    for (path, body) in [
        ("/clients/obs1/observe", None),
        ("/skill-evaluations/run", Some(json!({"select": "all"}))),
        ("/skill-evaluations/queue/cancel", None),
        (
            "/clients/obs1/observed/import",
            Some(json!({"items": [{"key": "x-00000000"}]})),
        ),
        ("/import", Some(json!({"client": "obs1"}))),
    ] {
        let resp = api(
            &app_,
            "POST",
            &format!("/api/v1{path}"),
            &[("authorization", v.as_str())],
            body,
        )
        .await;
        assert_eq!((resp.status, resp.code()), (403, "forbidden".to_string()), "{path}");
    }
    let anon = api(
        &app_,
        "POST",
        "/api/v1/skill-evaluations/run",
        &[("x-agent-hub", "1")],
        Some(json!({"select": "all"})),
    )
    .await;
    assert_eq!(anon.status, 401);
    assert!(r.tool_calls().is_empty());
}

#[tokio::test]
async fn test_observed_import_over_http() {
    let (t, a) = obs_hub(FakeEvalRunner::new());
    obs_client(&t);
    let (key, _) = observe_one(&t, &a, os1("devops/foo"));
    let app_ = app(&t);
    let h = bearer(&t.key("ev-admin", "admin"));
    let hdr = [("authorization", h.as_str())];
    let url = "/api/v1/clients/obs1/observed/import";
    let resp = api(
        &app_,
        "POST",
        url,
        &hdr,
        Some(json!({"items": [{"key": key, "name": "mine"}]})),
    )
    .await;
    let row = resp.json()["results"][0].clone();
    assert_eq!(
        (resp.status, row["status"].as_str(), row["code"].as_str()),
        (200, Some("refused"), Some("eval_required"))
    );
    assert_eq!(resp.json()["gate_mode"], "require");
    set_gate(&t, "eval_gate_observed", "advisory");
    let resp = api(
        &app_,
        "POST",
        url,
        &hdr,
        Some(json!({"items": [{"key": key, "name": "mine"}]})),
    )
    .await
    .json();
    assert_eq!(
        (
            resp["results"][0]["final_name"].as_str(),
            resp["results"][0]["gate"].as_str()
        ),
        (Some("mine"), Some("missing"))
    );
    assert_eq!(
        api(&app_, "POST", url, &hdr, Some(json!({"items": []}))).await.status,
        400
    );
    assert_eq!(
        api(
            &app_,
            "POST",
            url,
            &hdr,
            Some(json!({"items": [{"key": key}], "enable": "x"}))
        )
        .await
        .status,
        422
    );
    assert_eq!(
        api(
            &app_,
            "POST",
            "/api/v1/clients/nope/observed/import",
            &hdr,
            Some(json!({"items": [{"key": key}]}))
        )
        .await
        .status,
        404
    );
}

// ---- runner and CLI -------------------------------------------------------------------------------------------------
#[test]
fn test_real_runner_max_stdout_keyword() {
    use agent_hub::adapters::base::CommandRunner as _;
    let argv = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "head -c 300000 /dev/zero | tr '\\0' x".to_string(),
    ];
    let (_, out, _) = agent_hub::services::fs::RealRunner::default().run(&argv, 5.0).unwrap();
    assert_eq!(out.len(), 100_000, "default unchanged");
}

#[test]
fn test_hub_builds_the_observe_runner_with_the_big_cap() {
    // A-4: the hub's runners are `Arc<dyn CommandRunner>`, so the default observe runner is checked where it is built
    let r = agent_hub::services::hub::default_observe_runner();
    assert_eq!(r.max_stdout, 48 * 1024 * 1024);
    assert!((r.max_timeout - 120.0).abs() < f64::EPSILON);
    // `Hub(cfg, runner=hub.runner)`: an injected runner is the one the hub uses (`h2.runner is hub.runner`)
    let t = hub();
    let h2 = agent_hub::services::hub::Hub::new(
        t.hub.cfg(),
        agent_hub::services::hub::HubOptions {
            runner: Some(t.hub.runner.clone()),
            ..agent_hub::services::hub::HubOptions::default()
        },
    )
    .ok();
    assert!(
        h2.as_ref()
            .is_some_and(|h2| std::sync::Arc::ptr_eq(&h2.runner, &t.hub.runner))
    );
    if let Some(h2) = h2 {
        h2.close();
    }
}

#[test]
fn test_cli_observe_evaluate_and_import_observed() {
    let r = FakeEvalRunner::tier1(canned(&[("Code_Risk_Analysis", "failed", &[("high", "bad thing")])]));
    let (t, a) = obs_hub(FakeEvalRunner::new());
    obs_client(&t);
    a.set(vec![os1("devops/foo"), os1("bar")], true);
    let out = cli(&t, &["observe", "--client", "obs1"]);
    assert_eq!(out.code, 0, "{}", out.err);
    assert!(out.out.contains("2 skill(s) observed"));
    use_(&t.hub.skilleval, r.clone(), LAUNCHERS, None, None);
    let out = cli(&t, &["evaluate", "--source", "obs1"]);
    assert_eq!(out.code, 0);
    assert!(out.out.contains("nothing was run") && r.tool_calls().is_empty());
    let out = cli(&t, &["evaluate", "--source", "obs1", "--wait"]);
    assert_eq!(out.code, 2, "failed verdicts exist");
    assert_eq!(out.out.matches("obs1:").count(), 2);
    assert_eq!(r.names_in_order().len(), 2);
    let out = cli(&t, &["evaluate", "--source", "obs1", "--wait"]);
    assert_eq!(out.code, 0);
    assert!(out.out.contains("nothing to evaluate"));
    assert_eq!(
        cli(&t, &["evaluate", "--source", "obs1", "--select", "all", "--wait"]).code,
        2
    );
    assert_eq!(r.names_in_order().len(), 2, "reused: no new process");
    assert_eq!(
        cli(&t, &["evaluate", "--subject", "obs1:bar-nothere0", "--wait"]).code,
        1
    );
    set_gate(&t, "eval_gate_observed", "require");
    let keys: Vec<String> = t.hub.observed_view("obs1", None, None, None, 100, None).unwrap()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap().to_string())
        .collect();
    let out = cli(&t, &["import-observed", "--client", "obs1", "--key", &keys[0]]);
    assert_eq!(out.code, 1, "blocked");
    assert!(out.out.contains("refused"));
    let out = cli(
        &t,
        &[
            "import-observed",
            "--client",
            "obs1",
            "--key",
            &keys[0],
            "--override-reason",
            "known false positive, reviewed",
        ],
    );
    assert_eq!(out.code, 0);
    assert!(out.out.contains("overridden"));
    assert_eq!(
        cli(
            &t,
            &[
                "import-observed",
                "--client",
                "obs1",
                "--key",
                &keys[0],
                "--key",
                &keys[1],
                "--name",
                "x"
            ]
        )
        .code,
        1
    );
    assert!(r.tool_calls()[0].argv.join(" ").contains(SAFE_CHECKS));
}

#[test]
fn test_missing_observed_snapshot_is_not_found() {
    let s = standalone();
    use_default(&s.svc);
    assert_eq!(
        s.svc.snapshot("obs1:nothere-00000000").err().map(|p| p.status),
        Some(404)
    );
    assert_eq!(
        s.svc.detail("obs1:nothere-00000000", 20).err().map(|p| p.status),
        Some(404)
    );
}

/// Python reads the rows over HTTP (WS-B1 lists the hub skills); the field is `security_severity(latest)`, checked here
/// on the stored record (see the companion below for the rows).
#[test]
fn test_list_rows_carry_security_severity() {
    let s = standalone();
    let c = canned(&[
        ("Code_Risk_Analysis", "failed", &[("high", "a"), ("critical", "b")]),
        ("Brand_New_Scanner", "failed", &[("high", "c")]),
        ("QUALITY", "failed", &[("critical", "not security")]),
    ]);
    let llm = canned(&[
        ("Code_Risk_Analysis", "failed", &[("high", "a")]),
        ("Secrets_Detection", "failed", &[("high", "llm only")]),
    ]);
    use_(
        &s.svc,
        FakeEvalRunner::with(Some(c), None, Some(llm), None),
        LAUNCHERS,
        None,
        None,
    );
    s.skill("sev");
    s.skill("never");
    s.svc.evaluate("sev", "tier1", "t").unwrap();
    let mut rec = s.svc.records("hub:sev", 20)[0].clone();
    assert_eq!(
        agent_hub::services::skilleval::security_severity(&rec),
        Some(json!({"critical": 1, "high": 3}))
    );
    assert!(s.svc.records("hub:never", 1).is_empty());
    rec.as_object_mut().unwrap().remove("validators"); // an older record: the field is omitted
    assert_eq!(agent_hub::services::skilleval::security_severity(&rec), None);
}

#[tokio::test]
async fn test_list_rows_carry_security_severity_over_http() {
    let c = canned(&[
        ("Code_Risk_Analysis", "failed", &[("high", "a"), ("critical", "b")]),
        ("Brand_New_Scanner", "failed", &[("high", "c")]),
    ]);
    let llm = canned(&[("Secrets_Detection", "failed", &[("high", "llm only")])]);
    let r = FakeEvalRunner::with(Some(c), None, Some(llm), None);
    let (t, _) = obs_hub(r);
    t.hub.init_content().unwrap();
    skill(&t, "sev", "x\n", &[]);
    skill(&t, "never", "y\n", &[]);
    t.hub.skilleval.evaluate("sev", "tier1", "t").unwrap();
    let a = app(&t);
    let h = bearer(&t.key("ev-admin", "admin"));
    let items = api(
        &a,
        "GET",
        "/api/v1/skill-evaluations",
        &[("authorization", h.as_str())],
        None,
    )
    .await
    .json()["items"]
        .clone();
    let rows: std::collections::HashMap<String, Value> = items
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["name"].as_str().unwrap().to_string(), r.clone()))
        .collect();
    assert_eq!(
        rows["sev"]["latest"]["security_severity"],
        json!({"critical": 1, "high": 3})
    );
    assert!(rows["never"]["latest"].is_null());
}

#[test]
fn test_observed_bytecode_is_left_out_of_the_store_the_hash_evaluation_and_import() {
    let s = standalone();
    let idx = store(
        &s,
        vec![oskill(
            "tool",
            None,
            "agent",
            "Do it.\n",
            &[
                ("scripts/t.py", b"print(1)\n"),
                ("scripts/__pycache__/t.cpython-313.pyc", b"\x00bc"),
                ("x.pyc", b"\x00bc"),
            ],
        )],
    );
    let file: Value =
        serde_json::from_str(&std::fs::read_to_string(s.data().join("observed/obs1/index.json")).unwrap()).unwrap();
    assert_eq!(file, idx);
    let sk = &file["skills"][0];
    assert_eq!(sk["files"], 2);
    assert!(
        file["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("2 compiled Python bytecode"))
    );
    let key = sk["key"].as_str().unwrap();
    let mut names: Vec<String> = s.obs.files("obs1", key).unwrap().keys().cloned().collect();
    names.sort();
    assert_eq!(names, ["SKILL.md", "scripts/t.py"]);
    assert_eq!(
        s.svc.current_hash(&format!("obs1:{key}")).as_deref(),
        sk["content_hash"].as_str()
    );
}
