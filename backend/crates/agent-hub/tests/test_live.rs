//! Port of backend/tests/core/test_live.py: the LIVE self-test with real git, real files and the real claude-code /
//! opencode / generic adapters, in temp directories only (nothing touches a real client location). Needs the real
//! adapters (WS-A), the content store / importer / organizer (WS-B1) and the guards (WS-F).

mod b2_fakes;

use std::path::Path;
use std::sync::Arc;

use agent_hub::adapters::load_registry;
use agent_hub::services::hub::{Hub, HubOptions};
use b2_fakes::*;
use hub_core::value::PyValue;
use indexmap::IndexMap;
use serde_json::{Value, json};

fn git(cwd: &Path, args: &[&str]) -> Result<String, E> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `Hub(Settings(content_dir, data_dir, policy_file), registry=load_registry(), runner=FakeRunner())`.
fn live_hub(tmp: &Path) -> Result<Arc<Hub>, E> {
    Ok(Hub::new(
        settings(tmp)?,
        HubOptions {
            registry: Some(load_registry()),
            runner: Some(Arc::new(FakeRunner::default())),
            ..HubOptions::default()
        },
    )
    .map_err(|p| p.detail)?)
}

/// A put_client payload with the adapter's default params (a PyValue, not JSON).
fn client_payload(v: Value, params: Option<&PyValue>) -> IndexMap<String, PyValue> {
    let mut m = obj(v);
    if let Some(p) = params {
        m.insert("params".into(), p.clone());
    }
    m
}

fn files_of(plan: &hub_core::models::Plan) -> IndexMap<String, String> {
    plan.clients[0]
        .files
        .iter()
        .map(|f| (f.path.clone(), f.action.clone()))
        .collect()
}

#[test]
fn test_init_import_organise_commit_plan_apply_edit_revert() -> R {
    let t = tempfile::tempdir()?;
    let tmp = t.path();
    let project = tmp.join("fake-claude-project");
    std::fs::create_dir_all(project.join(".claude/skills/serve-thing"))?;
    std::fs::write(
        project.join(".claude/skills/serve-thing/SKILL.md"),
        "---\nname: serve-thing\ndescription: Serve a thing\n---\nSteps.\n",
    )?;
    std::fs::create_dir(project.join(".claude/agents"))?;
    std::fs::write(
        project.join(".claude/agents/mesh-doctor.md"),
        "---\nname: mesh-doctor\ndescription: Diagnoses the mesh\ntools: Read, Grep\nmodel: sonnet\n---\nLook around.\n",
    )?;
    std::fs::write(project.join("CLAUDE.md"), "# Hand written\n\nDo not delete me.\n")?;
    std::fs::write(
        project.join(".claude/settings.json"),
        json!({"permissions": {"allow": ["Read"], "deny": ["Bash(rm -rf *)"]}, "env": {"FOO": "bar"}}).to_string(),
    )?;
    let hub = live_hub(tmp)?;
    let content = tmp.join("content");
    // 1. init: a real nested git repo with a fixed identity
    let r = hub.init_content()?;
    assert!(content.join(".git").is_dir() && r["commit"].is_string());
    assert_eq!(git(&content, &["log", "-1", "--format=%an"])?, "Agent Hub");
    let client_cfg = hub
        .registry
        .get("claude-code")
        .ok_or("no claude-code adapter")?
        .default_config();
    hub.put_client(
        "claude-live",
        &client_payload(
            json!({"adapter": "claude-code", "display_name": "Claude (live test)", "roots": {"project": s(&project)}}),
            Some(&client_cfg.params),
        ),
        true,
    )?;
    // 2. import what the client already has
    let disc = hub.importer.discover("claude-live")?["items"].clone();
    let skills: Vec<Value> = disc["skill"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| x["name"].clone())
        .collect();
    assert_eq!(skills, vec![json!("serve-thing")]);
    assert_eq!(disc["agent"][0]["name"], "mesh-doctor");
    let res =
        hub.import_items("claude-live", None, Some("skip"), true, &IndexMap::new(), None, "hub")?["results"].clone();
    assert!(
        res.as_array()
            .into_iter()
            .flatten()
            .all(|x| ["created", "skipped", "identical"].contains(&x["status"].as_str().unwrap_or(""))),
        "{res}"
    );
    assert!(hub.work.load("skill", "serve-thing")?.valid() && hub.work.load("agent", "mesh-doctor")?.valid());
    // 3. organise, then commit (the approval of content)
    hub.organizer
        .create_collection(&py(json!({"id": "ops", "title": "Ops", "icon": "wrench",
        "members": [{"kind": "skill", "name": "serve-thing"}, {"kind": "agent", "name": "mesh-doctor"}]})))?;
    hub.work.put(
        "instruction",
        "house-style",
        &py(json!({"title": "House style", "order": 5, "body": "Be brief.\n"})),
        None,
    )?;
    hub.organizer
        .toggle("claude-live", "instruction", "house-style", true)?;
    assert!(hub.changes()?["count"].as_i64().is_some_and(|n| n > 3));
    let sha1 = hub.commit("organise: import + ops collection", "live-test")?["commit"].clone();
    assert_eq!(json!(git(&content, &["rev-parse", "HEAD"])?), sha1);
    assert_eq!(hub.changes()?["count"], 0);
    // 4. plan (pure) then apply
    let plan = hub.plan(Some(&["claude-live".into()]), "committed")?;
    let files = files_of(&plan);
    assert!(!plan.clients[0].blocked, "{:?}", plan.clients[0].blocked_reasons);
    assert!(files["CLAUDE.md"] == "add" || files["CLAUDE.md"] == "change");
    assert_eq!(files[".claude/settings.json"], "advisory"); // permissions stay advisory by default
    let before_settings = std::fs::read(project.join(".claude/settings.json"))?;
    assert!(["change", "unchanged"].contains(&files[".claude/agents/mesh-doctor.md"].as_str()));
    assert!(!files.values().any(|a| a == "conflict"));
    let out = hub.apply(&plan.id, true, &[], "live-test")?["results"][0].clone();
    assert!(
        out["status"] == "applied" && out["skipped_conflicts"] == json!([]),
        "{out}"
    );
    let claude_md = read(&project.join("CLAUDE.md"));
    assert!(claude_md.contains("Do not delete me.") && claude_md.contains("Be brief."));
    assert_eq!(std::fs::read(project.join(".claude/settings.json"))?, before_settings); // advisory: untouched
    assert!(project.join(".claude/skills/serve-thing/SKILL.md").is_file());
    let st = hub.drift("claude-live")?["status"].clone();
    assert!(st == "in_sync" || st == "drift");
    let again = hub.plan(Some(&["claude-live".into()]), "committed")?;
    assert!(
        again.clients[0]
            .files
            .iter()
            .all(|f| f.action == "unchanged" || f.action == "advisory")
    );
    // 5. edit -> plan shows the change
    hub.work.put(
        "instruction",
        "house-style",
        &py(json!({"title": "House style", "order": 5, "body": "Be VERY brief.\n"})),
        None,
    )?;
    let sha2 = hub.commit("tighten style", "live-test")?["commit"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let plan2 = hub.plan(Some(&["claude-live".into()]), "committed")?;
    let changed: Vec<&hub_core::models::PlanFile> =
        plan2.clients[0].files.iter().filter(|f| f.action == "change").collect();
    assert_eq!(
        changed.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
        vec!["CLAUDE.md"]
    );
    assert!(changed[0].diff.contains("+Be VERY brief."));
    hub.apply(&plan2.id, true, &[], "live-test")?;
    assert!(read(&project.join("CLAUDE.md")).contains("VERY brief"));
    // 6. revert the content commit -> plan shows the change back -> apply restores the original bytes
    hub.revert(&sha2, "live-test")?;
    let plan3 = hub.plan(Some(&["claude-live".into()]), "committed")?;
    let ch3: Vec<&str> = plan3.clients[0]
        .files
        .iter()
        .filter(|f| f.action == "change")
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(ch3, vec!["CLAUDE.md"]);
    hub.apply(&plan3.id, true, &[], "live-test")?;
    let text = read(&project.join("CLAUDE.md"));
    assert!(text.contains("Be brief.") && !text.contains("VERY") && text.contains("Do not delete me."));
    assert!(hub.history(50)?.len() >= 4);
    assert_eq!(git(&content, &["status", "--porcelain"])?, "");
    // nothing was written outside the temp roots (the test's own frontend/ fixture aside)
    let mut names: Vec<String> = std::fs::read_dir(tmp)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["content", "fake-claude-project", "frontend", "state"]);
    hub.close();
    Ok(())
}

#[test]
fn test_generic_spec_example_validate_and_deliver() -> R {
    let t = tempfile::tempdir()?;
    let tmp = t.path();
    let example = hub_core::yamlsafe::load(
        &std::fs::read_to_string(agent_hub::config::app_root().join("examples/clients/codex-cli.yaml"))?,
        256 * 1024,
    )
    .map_err(|e| e.0)?;
    let (proj, chome) = (tmp.join("proj"), tmp.join("codex-home"));
    std::fs::create_dir(&proj)?;
    std::fs::create_dir(&chome)?;
    let hub = live_hub(tmp)?;
    hub.init_content()?;
    let roots = py(json!({"project": s(&proj), "codex_home": s(&chome)}));
    let spec = example.get("spec").cloned().ok_or("no spec")?;
    let body = |spec: PyValue| IndexMap::from([("spec".to_string(), spec), ("roots".to_string(), roots.clone())]);
    let dry = hub.validate_spec(&body(spec.clone()))?;
    assert_eq!(dry["ok"], json!(true));
    assert!(
        dry["artifacts"]
            .as_array()
            .ok_or("artifacts")?
            .iter()
            .any(|a| a["path"] == "AGENTS.md")
    );
    let mut bad_spec = spec.as_dict().map(<[_]>::to_vec).unwrap_or_default();
    bad_spec.retain(|(k, _)| k.as_str() != Some("instructions"));
    bad_spec.push((
        PyValue::str("instructions"),
        py(json!({"root": "project", "file": "../../x", "mode": "block"})),
    ));
    let bad = hub.validate_spec(&body(PyValue::Dict(bad_spec)))?;
    assert!(
        bad["ok"] == json!(false)
            || bad["artifacts"]
                .as_array()
                .into_iter()
                .flatten()
                .all(|a| !a["path"].as_str().unwrap_or("").contains(".."))
    );
    let mut payload: IndexMap<String, PyValue> = example
        .as_dict()
        .into_iter()
        .flatten()
        .filter(|(k, _)| k.as_str() != Some("id"))
        .map(|(k, v)| (k.py_str(), v.clone()))
        .collect();
    payload.insert("roots".into(), roots.clone());
    hub.put_client("codex-live", &payload, true)?;
    hub.work.put(
        "instruction",
        "style",
        &py(json!({"title": "Style", "body": "Be terse.\n"})),
        None,
    )?;
    hub.organizer.toggle("codex-live", "instruction", "style", true)?;
    hub.commit("codex", "t")?;
    let plan = hub.plan(Some(&["codex-live".into()]), "committed")?;
    assert!(!plan.clients[0].blocked, "{:?}", plan.clients[0].blocked_reasons);
    hub.apply(&plan.id, true, &[], "t")?;
    assert!(read(&proj.join("AGENTS.md")).contains("Be terse."));
    hub.close();
    Ok(())
}

#[test]
fn test_import_then_first_plan_has_no_conflicts_and_second_is_unchanged() -> R {
    let t = tempfile::tempdir()?;
    let tmp = t.path();
    let project = tmp.join("proj");
    std::fs::create_dir_all(project.join(".claude/skills/one"))?;
    std::fs::write(
        project.join(".claude/skills/one/SKILL.md"),
        "---\nname: one\ndescription: First\n---\nBody\n",
    )?;
    std::fs::create_dir(project.join(".claude/agents"))?;
    std::fs::write(
        project.join(".claude/agents/a.md"),
        "---\nname: a\ndescription: Agent a\ntools: Read\nmodel: sonnet\n---\nDo a.\n",
    )?;
    let hub = live_hub(tmp)?;
    hub.init_content()?;
    hub.put_client(
        "c1",
        &obj(json!({"adapter": "claude-code", "display_name": "C", "roots": {"project": s(&project)}})),
        true,
    )?;
    let out = hub.import_items("c1", None, Some("skip"), true, &IndexMap::new(), None, "hub")?;
    assert!(out["adopted_live"].as_i64().is_some_and(|n| n >= 2));
    hub.commit("import", "t")?;
    let plan = hub.plan(Some(&["c1".into()]), "committed")?;
    let acts = files_of(&plan);
    assert!(
        !acts.values().any(|a| a == "conflict") && !plan.clients[0].blocked,
        "{acts:?}"
    );
    assert_eq!(acts[".claude/skills/one/SKILL.md"], "unchanged");
    let res = hub.apply(&plan.id, true, &[], "t")?["results"][0].clone();
    assert!((res["status"] == "applied" || res["status"] == "nothing") && res["skipped_conflicts"] == json!([]));
    let again = hub.plan(Some(&["c1".into()]), "committed")?;
    assert!(
        again.clients[0]
            .files
            .iter()
            .all(|f| f.action == "unchanged" || f.action == "advisory")
    );
    hub.close();
    Ok(())
}

#[test]
fn test_opencode_whole_file_instructions_first_plan_unchanged() -> R {
    let t = tempfile::tempdir()?;
    let tmp = t.path();
    let proj = tmp.join("oc");
    std::fs::create_dir(&proj)?;
    std::fs::write(proj.join("AGENTS.md"), "# Team rules\n\nBe kind.\n")?;
    let hub = live_hub(tmp)?;
    hub.init_content()?;
    let d = hub
        .registry
        .get("opencode")
        .ok_or("no opencode adapter")?
        .default_config();
    let roots: serde_json::Map<String, Value> = d.roots.keys().map(|k| (k.clone(), json!(s(&proj)))).collect();
    hub.put_client(
        "oc",
        &client_payload(
            json!({"adapter": "opencode", "display_name": "oc", "roots": roots}),
            Some(&d.params),
        ),
        true,
    )?;
    hub.import_items("oc", None, Some("skip"), true, &IndexMap::new(), None, "hub")?;
    let doc = hub.work.get_client("oc")?;
    assert_eq!(
        doc.params.get("instructions_mode").and_then(PyValue::as_str),
        Some("own")
    );
    hub.commit("import", "t")?;
    let acts = files_of(&hub.plan(Some(&["oc".into()]), "committed")?);
    assert_eq!(acts["AGENTS.md"], "unchanged", "{acts:?}");
    hub.close();
    Ok(())
}

#[test]
fn test_adapter_default_manage_flags_are_honoured_for_the_real_default_configs() -> R {
    let t = tempfile::tempdir()?;
    let tmp = t.path();
    let hub = live_hub(tmp)?;
    hub.init_content()?;
    hub.work
        .put("skill", "s1", &py(json!({"description": "S", "body": "b\n"})), None)?;
    hub.work.put(
        "agent",
        "a1",
        &py(json!({"description": "A", "capabilities": ["read"], "body": "b\n"})),
        None,
    )?;
    hub.work
        .put("instruction", "i1", &py(json!({"title": "I", "body": "b\n"})), None)?;
    let expected: Vec<(&str, Vec<&str>)> = vec![
        ("claude-code", vec!["skills", "agents", "instructions"]),
        ("opencode", vec!["agents", "instructions"]),
        ("hermes-agent", vec!["skills"]),
    ]
    .into_iter()
    .filter(|(k, _)| hub.registry.get(k).is_some()) // adapters may be added/removed
    .collect();
    assert!(!expected.is_empty());
    for (aid, _) in &expected {
        let d = hub.registry.get(aid).ok_or("adapter")?.default_config();
        let mut roots = serde_json::Map::new();
        for k in d.roots.keys() {
            let p = tmp.join(aid).join(k);
            std::fs::create_dir_all(&p)?;
            roots.insert(k.clone(), json!(s(&p)));
        }
        hub.put_client(
            aid,
            &client_payload(
                json!({"adapter": aid, "display_name": aid, "roots": roots}),
                Some(&d.params),
            ),
            true,
        )?;
        hub.work.put_profile(
            aid,
            &py(json!({"enable": {"skills": ["s1"], "agents": ["a1"], "instructions": ["i1"]}})),
        )?;
    }
    hub.commit("clients", "t")?;
    for (aid, managed) in &expected {
        let eff = hub.client_view(aid)?["manage_effective"].clone();
        let on: Vec<&str> = ["skills", "agents", "instructions"]
            .into_iter()
            .filter(|c| eff[*c] == json!(true))
            .collect();
        let mut want = managed.clone();
        want.sort_unstable();
        let mut got = on.clone();
        got.sort_unstable();
        assert_eq!(got, want, "{aid}: {eff}");
        assert!(!(eff["mcp"] == json!(true) || eff["permissions"] == json!(true) || eff["memory"] == json!(true)));
        let plan = hub.plan(Some(&[(*aid).to_string()]), "committed")?;
        for f in &plan.clients[0].files {
            if (f.action == "add" || f.action == "change") && f.managed {
                assert!(
                    ["skill", "agent", "instruction"].contains(&f.kind.as_str()),
                    "{aid}: {}",
                    f.kind
                );
                assert!(managed.contains(&format!("{}s", f.kind).as_str()), "{aid}: {}", f.kind);
            }
        }
    }
    hub.close();
    Ok(())
}
