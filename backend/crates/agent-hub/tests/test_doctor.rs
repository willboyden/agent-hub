//! Port of backend/tests/core/test_doctor.py: the read-only host-exposure report (services/doctor.rs), the hub/API
//! integration, `hubctl doctor`, the interactive key prompt and `retire-bootstrap-key`. The hubctl rows run the real
//! `hubctl` binary (WS-C), on a pseudo-terminal from `script(1)` where Python fakes `isatty`/`getpass`.

mod support_wsf;

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_hub::config::app_root;
use agent_hub::services::doctor::{CHECKS, Check, DoctorContext, Found, claude_settings_gaps, finding};
use hub_core::value::PyValue;
use serde_json::{Value, json};
use support_wsf::doctor_fakes::{Ctx, FakeFs, by_id, ctx, good_settings};
use support_wsf::{E, R, p};

fn one(rep: &Value, fid: &str) -> Result<Value, E> {
    by_id(rep, fid)
        .into_iter()
        .next()
        .ok_or_else(|| format!("no finding {fid} in {rep}").into())
}

fn sev(rep: &Value, fid: &str) -> Result<String, E> {
    Ok(one(rep, fid)?["severity"].as_str().unwrap_or_default().to_string())
}

fn has(v: &Value, k: &str, needle: &str) -> bool {
    v[k].as_str().is_some_and(|s| s.contains(needle))
}

fn uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

#[test]
fn test_findings_have_the_documented_shape_and_no_secrets() -> R {
    let d = tempfile::tempdir()?;
    let rep = ctx(
        d.path(),
        FakeFs::files(&[(d.path().join("state/bootstrap-admin.key"), "hc_SUPERSECRETKEYVALUE")]),
    )?
    .run();
    assert!(["ok", "attention", "action_needed"].contains(&rep["status"].as_str().unwrap_or_default()));
    let keys: Vec<&String> = rep["counts"].as_object().ok_or("counts")?.keys().collect();
    assert_eq!(keys, ["crit", "warn", "info", "ok"]);
    for f in rep["findings"].as_array().ok_or("findings")? {
        let k: Vec<&String> = f.as_object().ok_or("finding")?.keys().collect();
        assert_eq!(k, ["id", "severity", "title", "detail", "fix"]);
        assert!(["ok", "info", "warn", "crit"].contains(&f["severity"].as_str().unwrap_or_default()));
    }
    assert!(!rep.to_string().contains("SUPERSECRET"));
    Ok(())
}

#[test]
fn test_bootstrap_key_file_present_warns_with_the_fix() -> R {
    let d = tempfile::tempdir()?;
    let present = ctx(
        d.path(),
        FakeFs::files(&[(d.path().join("state/bootstrap-admin.key"), "k")]),
    )?
    .run();
    let f = one(&present, "bootstrap-key")?;
    assert!(
        f["severity"] == "warn" && has(&f, "fix", "hubctl retire-bootstrap-key") && present["status"] == "attention"
    );
    assert_eq!(sev(&ctx(d.path(), FakeFs::default())?.run(), "bootstrap-key")?, "ok");
    Ok(())
}

#[test]
fn test_loose_modes_are_reported_with_exact_chmod() -> R {
    let d = tempfile::tempdir()?;
    let st = d.path().join("state");
    let mut fs = FakeFs::files(&[(st.join("hub.db"), "x"), (st.join("attest.key"), "y")]);
    fs.modes.insert(st.to_string_lossy().into(), (0o755, uid()));
    fs.modes
        .insert(st.join("locks").to_string_lossy().into(), (0o700, uid()));
    fs.file_modes
        .insert(st.join("attest.key").to_string_lossy().into(), 0o644);
    let f = one(&ctx(d.path(), fs)?.run(), "modes")?;
    assert!(f["severity"] == "warn");
    assert!(has(&f, "fix", &format!("chmod 700 {}", st.display())));
    assert!(has(
        &f,
        "fix",
        &format!("chmod 600 {}", st.join("attest.key").display())
    ));
    assert!(!has(&f, "detail", "locks") && !has(&f, "detail", "hub.db"));
    assert_eq!(sev(&ctx(d.path(), FakeFs::default())?.run(), "modes")?, "ok");
    Ok(())
}

#[test]
fn test_content_dir_location_and_ownership() -> R {
    let d = tempfile::tempdir()?;
    let mut fs = FakeFs::default();
    fs.modes.insert(
        d.path().join("state/content").to_string_lossy().into(),
        (0o700, uid() + 1),
    );
    let f = one(&ctx(d.path(), fs)?.run(), "content-dir")?;
    assert!(f["severity"] == "crit" && has(&f, "detail", "not owned") && has(&f, "fix", "migrate-content"));
    let mut inside = ctx(d.path(), FakeFs::default())?;
    inside.clients =
        vec![json!({"id": "c", "adapter": "fake", "roots": {"r": d.path().to_string_lossy()}, "manage": {}})];
    assert_eq!(sev(&inside.run(), "content-dir")?, "crit"); // content dir inside a client root
    assert_eq!(sev(&ctx(d.path(), FakeFs::default())?.run(), "content-dir")?, "ok");
    Ok(())
}

#[test]
fn test_trust_loopback_and_bind_are_critical() -> R {
    let d = tempfile::tempdir()?;
    let mut c = ctx(d.path(), FakeFs::default())?;
    c.cfg.trust_loopback = true;
    c.cfg.host = "0.0.0.0".into();
    let rep = c.run();
    assert!(sev(&rep, "trust-loopback")? == "crit" && sev(&rep, "bind")? == "crit");
    assert_eq!(rep["status"], "action_needed");
    Ok(())
}

/// `proj_ctx(tmp, template, live, adapter, params)`: a claude-code client whose host paths all come from its params.
fn proj_ctx(tmp: &Path, template: Option<&str>, live: Option<&str>, adapter: &str, params: &Value) -> Result<Ctx, E> {
    let proj = tmp.join("proj");
    let home = tmp.join("home");
    let mut fs = FakeFs::default();
    if let Some(t) = template {
        fs.files.insert(
            proj.join("config/settings.template.json").to_string_lossy().into(),
            t.into(),
        );
    }
    if let Some(l) = live {
        fs.files.insert(
            home.join(".config/example/settings.json").to_string_lossy().into(),
            l.into(),
        );
    }
    let mut prm = json!({"settings_template": "config/settings.template.json",
                         "network_allowlist_key": "sandbox.network.allowedDomains",
                         "rerender_hint": "then run ./sync.sh to re-render",
                         "settings_rendered": home.join(".config/example/settings.json").to_string_lossy()});
    if let (Some(m), Some(x)) = (prm.as_object_mut(), params.as_object()) {
        for (k, v) in x {
            m.insert(k.clone(), v.clone());
        }
    }
    let mut c = ctx(tmp, fs)?;
    c.clients = vec![
        json!({"id": "claude-code", "adapter": adapter, "roots": {"project": proj.to_string_lossy()},
                            "manage": {}, "params": prm}),
    ];
    Ok(c)
}

fn pc(tmp: &Path, template: Option<&str>, live: Option<&str>) -> Result<Ctx, E> {
    proj_ctx(tmp, template, live, "claude-code", &json!({}))
}

#[test]
fn test_claude_template_ok_and_missing_entries() -> R {
    let d = tempfile::tempdir()?;
    let ok = pc(d.path(), Some(&good_settings().to_string()), None)?.run();
    assert!(sev(&ok, "claude-template")? == "ok" && sev(&ok, "claude-network")? == "info");
    let mut bad = good_settings();
    let deny: Vec<Value> = bad["permissions"]["deny"]
        .as_array()
        .ok_or("deny")?
        .iter()
        .filter(|d| {
            d.as_str()
                .is_some_and(|s| !s.contains("agent-hub") && !s.contains("policy"))
        })
        .cloned()
        .collect();
    bad["permissions"]["deny"] = Value::Array(deny);
    let files: Vec<Value> = bad["sandbox"]["credentials"]["files"].as_array().ok_or("files")?[1..].to_vec();
    bad["sandbox"]["credentials"]["files"] = Value::Array(files);
    let f = one(&pc(d.path(), Some(&bad.to_string()), None)?.run(), "claude-template")?;
    assert!(
        f["severity"] == "warn"
            && has(&f, "fix", "Read(~/.local/share/agent-hub/**)")
            && has(&f, "fix", "Write(**/hub/policy/**)")
    );
    assert!(has(&f, "fix", "\"path\": \"~/.local/share/agent-hub\""));
    Ok(())
}

#[test]
fn test_rendered_live_config_is_compared_read_only() -> R {
    let d = tempfile::tempdir()?;
    let mut stale = good_settings();
    stale["permissions"]["deny"] = json!([]);
    let good = good_settings().to_string();
    let rep = pc(d.path(), Some(&good), Some(&stale.to_string()))?.run();
    let f = one(&rep, "claude-rendered")?;
    assert!(f["severity"] == "info" && has(&f, "fix", "sync.sh"));
    assert_eq!(f["fix"], "Then run ./sync.sh to re-render.");
    assert_eq!(
        sev(&pc(d.path(), Some(&good), Some(&good))?.run(), "claude-rendered")?,
        "ok"
    );
    assert!(by_id(&pc(d.path(), Some(&good), None)?.run(), "claude-rendered").is_empty()); // unreadable: skipped
    Ok(())
}

#[test]
fn test_sandbox_network_loopback_is_flagged() -> R {
    let d = tempfile::tempdir()?;
    let mut s = good_settings();
    s["sandbox"]["network"]["allowedDomains"] = json!(["pypi.org", "localhost", "127.0.0.1"]);
    let f = one(&pc(d.path(), Some(&s.to_string()), None)?.run(), "claude-network")?;
    assert!(f["severity"] == "warn" && has(&f, "title", "localhost") && has(&f, "title", "127.0.0.1"));
    assert_eq!(
        f["title"],
        "claude-code: the sandbox may reach loopback (127.0.0.1, localhost)"
    );
    Ok(())
}

#[test]
fn test_hostile_or_malformed_settings_never_crash() -> R {
    let deep = "[".repeat(5000);
    for hostile in [
        "{not json",
        "[]",
        "null",
        r#"{"sandbox": 5, "permissions": "x"}"#,
        r#"{"sandbox": {"credentials": {"files": "no"}}, "permissions": {"deny": [1, null, {"a": 1}]}}"#,
        deep.as_str(),
        r#""\u0000""#,
        "",
    ] {
        let d = tempfile::tempdir()?;
        let rep = pc(d.path(), Some(hostile), Some(hostile))?.run();
        assert!(["ok", "attention", "action_needed"].contains(&rep["status"].as_str().unwrap_or_default()));
        assert!(
            by_id(&rep, "check-").is_empty(),
            "no check itself blew up: {hostile:.40}"
        );
    }
    Ok(())
}

#[test]
fn test_gap_computation_tolerates_any_shape() {
    for doc in [
        json!(null),
        json!(5),
        json!([]),
        json!({"sandbox": 5}),
        json!({"sandbox": {"credentials": {"files": [1, null, {"path": 3}]}}, "permissions": {"deny": [1, {"x": 1}]}}),
    ] {
        assert_eq!(
            claude_settings_gaps(&PyValue::from_json(&doc)).len(),
            4 * 4 + 2,
            "{doc}"
        ); // everything is missing
    }
}

#[test]
fn test_template_missing_and_opencode_note() -> R {
    let d = tempfile::tempdir()?;
    assert_eq!(sev(&pc(d.path(), None, None)?.run(), "claude-template")?, "info");
    let rep = proj_ctx(d.path(), None, None, "opencode", &json!({}))?.run();
    let f = one(&rep, "opencode-host")?;
    assert!(f["severity"] == "info" && has(&f, "fix", "docs/SECURITY.md"));
    assert!(by_id(&ctx(d.path(), FakeFs::default())?.run(), "opencode-host").is_empty());
    Ok(())
}

#[test]
fn test_ptrace_scope() -> R {
    let d = tempfile::tempdir()?;
    for (val, frag) in [("0\n", "= 0"), ("1\n", "= 1"), ("garbage", "unknown")] {
        let rep = ctx(d.path(), FakeFs::files(&[("/proc/sys/kernel/yama/ptrace_scope", val)]))?.run();
        let f = one(&rep, "ptrace")?;
        assert!(f["severity"] == "info" && has(&f, "title", frag), "{val:?}");
    }
    let rep = ctx(d.path(), FakeFs::files(&[("/proc/sys/kernel/yama/ptrace_scope", "0")]))?.run();
    assert!(has(&one(&rep, "ptrace")?, "fix", "sysctl"));
    // a non-decimal digit passes isdigit() but int() raises: Python reports the check as broken
    let rep = ctx(
        d.path(),
        FakeFs::files(&[("/proc/sys/kernel/yama/ptrace_scope", "\u{b2}")]),
    )?
    .run();
    assert_eq!(one(&rep, "check-check_ptrace")?["detail"], "ValueError");
    Ok(())
}

#[test]
fn test_systemd_units_read_write_paths() -> R {
    let d = tempfile::tempdir()?;
    let home = d.path().join("home");
    let ud = home.join(".config/systemd/user");
    let app = app_root();
    let parent = app.parent().ok_or("parent")?.to_path_buf();
    let fs = FakeFs::files(&[
        (
            ud.join("agent-hub.service"),
            format!(
                "[Service]\nReadWritePaths={} %h/.local/share/agent-hub\n",
                parent.display()
            )
            .as_str(),
        ),
        (
            ud.join("agent-hub-x.service"),
            format!("[Service]\nReadWritePaths={}\n", app.join("policy").display()).as_str(),
        ),
        (
            ud.join("agent-knowledge.service"),
            format!(
                "[Service]\nReadWritePaths=%h/.local/share/agent-knowledge -/tmp/x\nReadWritePaths={}\n",
                parent.join("hub/out").display()
            )
            .as_str(),
        ),
        (
            ud.join("other.service"),
            format!("[Service]\nReadWritePaths={}\n", parent.display()).as_str(),
        ),
    ]);
    let rep = ctx(d.path(), fs)?.run();
    let units: Vec<(String, String)> = by_id(&rep, "unit-")
        .iter()
        .map(|f| {
            (
                f["id"].as_str().unwrap_or_default().into(),
                f["severity"].as_str().unwrap_or_default().into(),
            )
        })
        .collect();
    let mut sorted = units;
    sorted.sort();
    assert_eq!(
        sorted,
        vec![
            ("unit-agent-hub-x.service".to_string(), "warn".to_string()),
            ("unit-agent-hub.service".to_string(), "warn".to_string()),
            ("unit-agent-knowledge.service".to_string(), "ok".to_string()),
        ]
    );
    assert!(by_id(&ctx(d.path(), FakeFs::default())?.run(), "unit-").is_empty());
    Ok(())
}

#[test]
fn test_knowledge_summary_states() -> R {
    let d = tempfile::tempdir()?;
    let with = |k: Option<Value>| -> Result<Value, E> {
        let mut c = ctx(d.path(), FakeFs::default())?;
        c.knowledge = k;
        Ok(c.run())
    };
    assert_eq!(sev(&with(None)?, "knowledge")?, "info");
    assert_eq!(sev(&with(Some(json!({"reachable": false})))?, "knowledge")?, "info");
    let f = one(
        &with(Some(
            json!({"reachable": true, "authenticated": false, "key_source": "none"}),
        ))?,
        "knowledge",
    )?;
    assert!(f["severity"] == "warn" && has(&f, "fix", "HUB_KNOWLEDGE_ADMIN_KEY"));
    let ok = with(Some(
        json!({"reachable": true, "authenticated": true, "key_source": "file"}),
    ))?;
    assert_eq!(sev(&ok, "knowledge")?, "ok");
    assert_eq!(one(&ok, "knowledge")?["detail"], "key source: file");
    Ok(())
}

#[test]
fn test_advisory_permission_gap_lists_exact_lines() -> R {
    let d = tempfile::tempdir()?;
    let mut c = ctx(d.path(), FakeFs::default())?;
    c.clients = vec![
        json!({"id": "a", "adapter": "generic", "roots": {}, "manage": {"permissions": false}}),
        json!({"id": "b", "adapter": "generic", "roots": {}, "manage": {"permissions": true}}),
        json!({"id": "c", "adapter": "opencode", "roots": {}, "manage": {"permissions": false}}),
    ];
    c.gaps = Box::new(|cid| match cid {
        "a" => Some((0..12).map(|i| format!("deny path_read ~/.aws/{i}")).collect()),
        "b" => Some(vec!["deny path_read x".into()]),
        _ => None,
    });
    let rep = c.run();
    let fs = by_id(&rep, "advisory-gap");
    assert_eq!(
        fs.iter().map(|f| f["id"].clone()).collect::<Vec<_>>(),
        vec![json!("advisory-gap-a")]
    );
    assert!(has(&fs[0], "fix", "deny path_read ~/.aws/0") && fs[0]["fix"].as_str().is_some_and(|s| s.ends_with("...")));
    Ok(())
}

#[test]
fn test_a_crashing_check_does_not_hide_the_others() -> R {
    fn boom(_c: &DoctorContext<'_>) -> Found {
        Err("RuntimeError") // Python: RuntimeError("secret detail hc_LEAK"); only the type name is reported
    }
    let d = tempfile::tempdir()?;
    let mut checks: Vec<Check> = vec![("boom", boom)];
    checks.extend(CHECKS);
    let rep = ctx(d.path(), FakeFs::default())?.run_with(&checks);
    assert!(
        by_id(&rep, "check-boom")
            .first()
            .is_some_and(|f| f["detail"] == "RuntimeError" && f["title"] == "boom could not run")
    );
    assert!(!rep.to_string().contains("LEAK") && !by_id(&rep, "modes").is_empty());
    // a panicking check is contained the same way (no message, only an exception-like type name)
    fn panics(_c: &DoctorContext<'_>) -> Found {
        std::panic::panic_any("secret detail hc_LEAK")
    }
    let rep = ctx(d.path(), FakeFs::default())?.run_with(&[("panics", panics), ("check_modes", CHECKS[1].1)]);
    assert_eq!(by_id(&rep, "check-panics")[0]["detail"], "RuntimeError");
    assert!(!rep.to_string().contains("LEAK") && !by_id(&rep, "modes").is_empty());
    Ok(())
}

// ---- API / hub integration ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn test_hub_doctor_and_endpoints() -> R {
    use agent_hub::api::security;
    use agent_hub::server::create_app;
    use http_body_util::BodyExt;
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let home = support_wsf::client_home(d.path())?;
    support_wsf::add_client(&hub, &home, "fake1", &json!({}))?;
    let rep = hub
        .doctor_with(None, Some(&FakeFs::default()), Some(&home))
        .map_err(p)?;
    assert!(["ok", "attention", "action_needed"].contains(&rep["status"].as_str().unwrap_or_default()));
    assert_eq!(sev(&rep, "trust-loopback")?, "ok");
    let admin = hub.auth.create_key("t-admin", "admin", None, &[]).map_err(p)?["secret"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let viewer = hub.auth.create_key("t-viewer", "viewer", None, &[]).map_err(p)?["secret"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let app = create_app(hub.clone());
    let get = |path: &'static str, key: Option<String>| {
        let app = app.clone();
        async move {
            let mut b = http::Request::builder()
                .method("GET")
                .uri(path)
                .header("host", "127.0.0.1:8792");
            if let Some(k) = key {
                b = b.header("authorization", format!("Bearer {k}"));
            }
            let r = b.body(axum::body::Body::empty()).unwrap_or_default();
            let resp = security::handle(app, std::net::SocketAddr::from(([10, 0, 0, 9], 5000)), 8792, r).await;
            let status = resp.status().as_u16();
            let bytes = resp
                .into_body()
                .collect()
                .await
                .map(|c| c.to_bytes().to_vec())
                .unwrap_or_default();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null))
        }
    };
    let (st, full) = get("/api/v1/doctor", Some(admin)).await;
    assert_eq!(st, 200);
    let keys: Vec<&String> = full.as_object().ok_or("report")?.keys().collect();
    assert_eq!(keys, ["status", "counts", "findings"]);
    assert_eq!(get("/api/v1/doctor", Some(viewer.clone())).await.0, 403); // the full report is admin only
    let (st, s) = get("/api/v1/doctor/summary", Some(viewer)).await;
    assert_eq!(st, 200);
    let o = s.as_object().ok_or("summary")?;
    assert!(
        o.len() == 3
            && ["crit", "warn", "info"]
                .iter()
                .all(|k| o.get(*k).is_some_and(Value::is_i64))
    );
    assert_eq!(get("/api/v1/doctor/summary", None).await.0, 401);
    Ok(())
}

// ---- hubctl (the real binary) ------------------------------------------------------------------------------------------

/// The environment hubctl gets: nothing of ours, the test hub's state, a key only when given.
fn hubctl_env(
    tmp: &Path,
    hub: &agent_hub::services::hub::Hub,
    key: Option<&str>,
    extra: &[(&str, &str)],
) -> Vec<(String, String)> {
    let c = hub.cfg();
    let mut env = vec![
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ("HOME".to_string(), tmp.join("home").to_string_lossy().to_string()),
        (
            "AGENT_HUB_ENV_FILE".to_string(),
            tmp.join("no-env").to_string_lossy().to_string(),
        ),
        ("HUB_DATA_DIR".to_string(), c.data_dir.to_string_lossy().to_string()),
        (
            "HUB_CONTENT_DIR".to_string(),
            c.content_dir.to_string_lossy().to_string(),
        ),
        ("SHELL".to_string(), "/bin/sh".to_string()),
        ("TERM".to_string(), "dumb".to_string()),
    ];
    if let Some(k) = key {
        env.push(("HUB_API_KEY".into(), k.into()));
    }
    env.extend(extra.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())));
    env
}

/// hubctl without a terminal (stdin /dev/null): (exit code, stdout, stderr).
fn hubctl(args: &[&str], env: &[(String, String)]) -> Result<(i32, String, String), E> {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_hubctl"))
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::null())
        .output()?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

const PROMPT: &str = "Hub admin key (input hidden): ";

/// hubctl on a pseudo-terminal (`script -qec`): answers `answer` once the hidden-input prompt appears (if it does).
/// Returns (exit code, everything the terminal showed, whether the prompt appeared).
fn hubctl_tty(args: &[&str], env: &[(String, String)], answer: &str) -> Result<(i32, String, bool), E> {
    let cmd = std::iter::once(env!("CARGO_BIN_EXE_hubctl"))
        .chain(args.iter().copied())
        .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ");
    let mut child = std::process::Command::new("/usr/bin/script")
        .args(["-qec", &cmd, "/dev/null"])
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().ok_or("stdout")?;
    let seen = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = seen.clone();
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 {
                break;
            }
            if let Ok(mut s) = sink.lock() {
                s.extend_from_slice(&buf[..n]);
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut prompted = false;
    let mut stdin = child.stdin.take();
    let mut held = None; // the answered stdin stays open until the child exits (script ends the session on EOF)
    while Instant::now() < deadline {
        let text = seen
            .lock()
            .map(|s| String::from_utf8_lossy(&s).into_owned())
            .unwrap_or_default();
        if text.contains(PROMPT.trim_end()) {
            prompted = true;
            if let Some(mut i) = stdin.take() {
                i.write_all(format!("{answer}\n").as_bytes())?;
                held = Some(i);
            }
            break;
        }
        if child.try_wait()?.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = child.wait()?;
    drop(stdin);
    drop(held);
    let _ = reader.join();
    let text = seen
        .lock()
        .map(|s| String::from_utf8_lossy(&s).into_owned())
        .unwrap_or_default();
    Ok((status.code().unwrap_or(-1), text, prompted))
}

fn secret(v: &Value) -> String {
    v["secret"].as_str().unwrap_or_default().to_string()
}

#[test]
fn test_cli_doctor() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let key = secret(&hub.auth.create_key("k", "admin", None, &[]).map_err(p)?);
    let env = hubctl_env(d.path(), &hub, Some(&key), &[]);
    let (rc, out, _) = hubctl(&["doctor"], &env)?;
    assert!((rc == 0 || rc == 2) && out.contains("status:"));
    let (rc, out, _) = hubctl(&["doctor", "--json"], &env)?;
    assert!(rc == 0 || rc == 2);
    let rep: Value = serde_json::from_str(&out)?;
    assert!(rep["counts"]["ok"].as_i64().is_some_and(|n| n >= 1));
    let env = hubctl_env(d.path(), &hub, Some(&key), &[("HUB_TRUST_LOOPBACK", "true")]);
    assert_eq!(hubctl(&["doctor"], &env)?.0, 2); // a critical finding exits 2
    Ok(())
}

#[test]
fn test_key_prompt_only_on_a_tty_and_never_echoed() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let key = secret(&hub.auth.create_key("k", "admin", None, &[]).map_err(p)?);
    let _ = std::fs::remove_file(hub.cfg().data_dir.join("bootstrap-admin.key")); // no key at rest
    let env = hubctl_env(d.path(), &hub, None, &[]);
    let (rc, out, err) = hubctl(&["validate"], &env)?;
    assert!(rc == 1 && !out.contains(PROMPT.trim_end()) && !err.contains(PROMPT.trim_end())); // non-TTY: no prompt
    let (rc, shown, prompted) = hubctl_tty(&["validate"], &env, &key)?;
    assert!(rc == 0 && prompted, "{shown}");
    assert!(!shown.contains(&key)); // never echoed
    let with_key = hubctl_env(d.path(), &hub, Some(&key), &[]);
    let (rc, _, prompted) = hubctl_tty(&["validate"], &with_key, "unused")?;
    assert!(rc == 0 && !prompted); // a key at hand: no prompt
    assert_eq!(hubctl_tty(&["validate"], &env, "hc_wrong")?.0, 1);
    Ok(())
}

/// Python additionally spies on `Path.unlink` to see the file zeroed before removal; a subprocess cannot be spied
/// on that way, so this checks the outcome (removed, never printed) and leaves the zeroing to WS-C's unit tests.
#[test]
fn test_retire_bootstrap_key() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let f = hub.cfg().data_dir.join("bootstrap-admin.key");
    let key = secret(&hub.auth.create_key("boot", "admin", None, &[]).map_err(p)?);
    std::fs::write(&f, format!("{key}\n"))?;
    std::fs::set_permissions(&f, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    let viewer = secret(&hub.auth.create_key("v", "viewer", None, &[]).map_err(p)?);
    let env = hubctl_env(d.path(), &hub, None, &[]);
    let (rc, _, err) = hubctl(&["retire-bootstrap-key"], &env)?;
    assert!(rc == 1 && f.exists() && err.contains("terminal")); // refuses without a TTY
    for wrong in ["hc_nope", viewer.as_str(), ""] {
        let (rc, _, _) = hubctl_tty(&["retire-bootstrap-key"], &env, wrong)?;
        assert!(rc == 1 && std::fs::read_to_string(&f)?.trim() == key); // untouched
    }
    let (rc, shown, _) = hubctl_tty(&["retire-bootstrap-key"], &env, &key)?;
    assert!(rc == 0 && !f.exists());
    assert!(shown.contains("password manager") && !shown.contains(&key));
    let (rc, out, _) = hubctl(&["retire-bootstrap-key"], &env)?;
    assert!(rc == 0 && out.contains("already gone")); // idempotent
    assert_eq!(hubctl_tty(&["validate"], &env, &key)?.0, 0); // the key itself still works, via the prompt
    Ok(())
}

#[test]
fn test_retire_refuses_a_symlinked_key_path() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let key = secret(&hub.auth.create_key("boot", "admin", None, &[]).map_err(p)?);
    let target = d.path().join("precious");
    std::fs::write(&target, "do not zero me")?;
    let link = hub.cfg().data_dir.join("bootstrap-admin.key");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&target, &link)?;
    let env = hubctl_env(d.path(), &hub, None, &[]);
    let (rc, _, _) = hubctl_tty(&["retire-bootstrap-key"], &env, &key)?;
    assert!(rc == 1 && std::fs::read_to_string(&target)? == "do not zero me");
    Ok(())
}

#[test]
fn test_finding_helper() {
    assert_eq!(
        finding("x", "ok", "t", "", ""),
        json!({"id": "x", "severity": "ok", "title": "t", "detail": "", "fix": ""})
    );
}
