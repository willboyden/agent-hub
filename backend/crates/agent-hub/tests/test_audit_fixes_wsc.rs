//! WS-C part of backend/tests/core/test_audit_fixes.py: the real evaluator runner's cancel, the observation store's
//! refusals and path rules, forget/prune of stored runs, the per-request select cap and the separate observed gate.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod wsc_support;

use std::collections::BTreeSet;
use std::sync::Arc;

use agent_hub::services::skilleval::{EvalRunner as _, RealEvalRunner};
use serde_json::{Value, json};
use wsc_support::*;

#[test]
fn test_cancel_kills_every_live_child_even_after_a_version_probe() {
    let tmp = tempfile::tempdir().unwrap();
    let r = Arc::new(RealEvalRunner::default());
    let env = vec![("PATH".to_string(), "/usr/bin:/bin".to_string())];
    let sleeper = vec!["/bin/sleep".to_string(), "60".to_string()];
    let mut threads = Vec::new();
    for _ in 0..2 {
        let (r, env, sleeper, cwd) = (r.clone(), env.clone(), sleeper.clone(), tmp.path().to_path_buf());
        threads.push(std::thread::spawn(move || r.run(&sleeper, &cwd, &env, 60.0)));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while r.live_children() < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(r.live_children(), 2);
    let probe = r.run(&["/bin/echo".into(), "version 1".into()], tmp.path(), &env, 30.0);
    assert_eq!(probe.returncode, 0);
    assert_eq!(r.live_children(), 2, "a probe neither hides nor removes the others");
    let t0 = std::time::Instant::now();
    r.cancel();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(t0.elapsed().as_secs() < 12);
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|x| x.returncode != 0 && !x.timed_out));
    assert_eq!(r.live_children(), 0);
}

/// Python goes through `hub.observe` (WS-B1 client registry); the refusal is the store's, exercised directly.
#[test]
fn test_incomplete_and_empty_observation_keeps_the_previous_one() {
    let s = standalone();
    s.obs.store("obs1", &observation(vec![os1("one")], true)).unwrap();
    let e = s.obs.store("obs1", &observation(vec![], false)).err().unwrap();
    assert_eq!((e.code.as_str(), e.status), ("observe_failed", 503));
    assert!(e.detail.contains("note one"), "{}", e.detail);
    let ann = s.obs.annotated("obs1").unwrap();
    assert_eq!(
        ann["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["one"]
    );
    let idx = s.obs.store("obs1", &observation(vec![], true)).unwrap(); // an empty but COMPLETE result is real
    assert_eq!(idx["skills"], json!([]));
}

/// A Rust string cannot hold a lone surrogate (Python's `chr(0xD800)` case); the other unprintable and colliding
/// paths are the same.
#[test]
fn test_unprintable_unencodable_and_colliding_paths_are_skipped() {
    let s = standalone();
    let (rlo, sep) = ('\u{202E}', '\u{2028}');
    let bad_md = format!("bad{rlo}.md");
    let skills = vec![
        os1("good"),
        oskill(&format!("rlo{rlo}name"), Some("rlo"), "agent", "Do it.\n", &[]),
        oskill(&format!("sep{sep}x"), Some("sep"), "agent", "Do it.\n", &[]),
        oskill("del\x7fx", Some("del"), "agent", "Do it.\n", &[]),
        oskill(
            "fine/file",
            None,
            "agent",
            "Do it.\n",
            &[("a/x", b"1"), ("a/x/y", b"2"), ("ok.md", b"3"), (bad_md.as_str(), b"4")],
        ),
    ];
    let idx = s.obs.store("obs1", &observation(skills, true)).unwrap();
    let items = idx["skills"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let paths: BTreeSet<&str> = items.iter().map(|i| i["path"].as_str().unwrap()).collect();
    assert_eq!(paths, ["fine/file", "good"].into_iter().collect());
    let key = items.iter().find(|i| i["path"] == "fine/file").unwrap()["key"]
        .as_str()
        .unwrap()
        .to_string();
    let root = s.data().join("observed/obs1/skills").join(&key);
    let mut files = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                files.push(p.strip_prefix(&root).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    files.sort();
    assert_eq!(files, ["SKILL.md", "a/x/y", "ok.md"]);
    let text = idx["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        !text.contains(rlo) && text.contains("also used as a directory"),
        "{text}"
    );
}

/// `hub.observe` prunes the runs of skills that left the observation and `hub.delete_client` forgets both stores;
/// those are `prune_observed` and `forget`, called here the way the hub calls them.
#[test]
fn test_forget_and_prune_remove_stored_runs() {
    let s = standalone();
    use_(&s.svc, FakeEvalRunner::tier1(canned(&[])), LAUNCHERS, None, None);
    let idx = s
        .obs
        .store("obs1", &observation(vec![os1("one"), os1("two")], true))
        .unwrap();
    let keys: std::collections::HashMap<String, String> = idx["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["path"].as_str().unwrap().to_string(),
                i["key"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    for k in keys.values() {
        s.svc.evaluate(&format!("obs1:{k}"), "static", "t").unwrap();
    }
    let runs = s.eval_root().join("@observed/obs1");
    let names = || {
        let mut v: Vec<String> = std::fs::read_dir(&runs)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    let mut all: Vec<String> = keys.values().cloned().collect();
    all.sort();
    assert_eq!(names(), all);
    let idx = s.obs.store("obs1", &observation(vec![os1("one")], true)).unwrap(); // "two" is gone: its runs go with it
    let keep: BTreeSet<String> = idx["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap().to_string())
        .collect();
    s.svc.prune_observed("obs1", &keep);
    assert_eq!(names(), [keys["one"].clone()]);
    s.obs.forget("obs1");
    assert!(!runs.exists() && !s.data().join("observed/obs1").exists());
}

#[test]
fn test_forget_and_prune_never_remove_the_running_subject() {
    let s = standalone();
    let gate = Event::default();
    let r = use_(
        &s.svc,
        FakeEvalRunner::with(Some(canned(&[])), None, None, Some(gate.clone())),
        LAUNCHERS,
        None,
        None,
    );
    let idx = s.obs.store("obs1", &observation(vec![os1("one")], true)).unwrap();
    let key = idx["skills"][0]["key"].as_str().unwrap().to_string();
    s.svc.enqueue(&[format!("obs1:{key}")], "static", true, "t").unwrap();
    assert!(r.started.wait(5.0));
    s.svc.prune_observed("obs1", &BTreeSet::new());
    assert!(
        s.eval_root().join("@observed/obs1").join(&key).exists(),
        "the running one is kept"
    );
    gate.set();
    s.svc.join(Some(10.0));
}

#[test]
fn test_select_is_capped_per_request() {
    let (t, _) = obs_hub(FakeEvalRunner::tier1(canned(&[])));
    t.hub.init_content().unwrap();
    put_client(
        &t,
        "obs1",
        &json!({"adapter": "fakeobs", "display_name": "Obs", "roots": {"home": wsc_support::client_home(&t.root).to_string_lossy()}}),
    )
    .unwrap();
    let n = agent_hub::services::hub::SELECT_CAP + 2;
    let skills = (0..n).map(|i| os1(&format!("s{i}"))).collect();
    t.hub.observe_svc.store("obs1", &observation(skills, true)).unwrap();
    let out = t.hub.eval_run(None, Some("all"), "obs1", "static", false, "t").unwrap();
    assert_eq!(
        out["queued"].as_array().unwrap().len(),
        agent_hub::services::hub::SELECT_CAP
    );
    assert_eq!(
        out["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["reason"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["request_cap", "request_cap"]
    );
    t.hub.skilleval.cancel().unwrap();
    t.hub.skilleval.join(Some(30.0));
}

#[test]
fn test_observed_gate_default_is_require_and_import_gate_is_separate() {
    let (t, _) = obs_hub(FakeEvalRunner::new());
    assert_eq!(t.hub.setting_str("eval_gate"), "advisory");
    assert_eq!(t.hub.setting_str("eval_gate_observed"), "require");
    let root = tempfile::tempdir().unwrap();
    let env: std::collections::HashMap<String, String> = [
        ("HOME".to_string(), root.path().to_string_lossy().into_owned()),
        (
            "AGENT_HUB_ENV_FILE".to_string(),
            root.path().join("none").to_string_lossy().into_owned(),
        ),
        ("HUB_EVAL_GATE_OBSERVED".to_string(), "advisory".to_string()),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        agent_hub::config::load_settings_from(&env, &vec![])
            .unwrap()
            .eval_gate_observed,
        "advisory"
    );
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
    let sel = [("skill".to_string(), "disc".to_string())];
    let row: Value = t
        .hub
        .import_items("fake1", Some(&sel), None, true, &indexmap::IndexMap::new(), None, "hub")
        .unwrap();
    assert_eq!(
        (row["results"][0]["status"].as_str(), row["gate_mode"].as_str()),
        (Some("created"), Some("advisory"))
    );
}
