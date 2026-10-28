//! Port of backend/tests/core/test_targets.py (H1, security control S-TARGETS): delivery roots and targets are
//! constrained by the floor's write denies and the hub's own paths. The Python tests go through put_client (WS-B1) and
//! plan (WS-B2); `targets_*` tests below exercise the guard itself and run now.

mod support_wsf;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_hub::adapters::base::{Artifact, ClientAdapter};
use agent_hub::config::app_root;
use agent_hub::services::floor::load_floor;
use agent_hub::services::targets::TargetGuard;
use hub_core::pathsafe::expand_root;
use serde_json::json;
use support_wsf::{E, FakeAdapter, R, p};

fn hostile_roots() -> Vec<String> {
    let app = app_root();
    let mut v: Vec<String> = [
        "~",
        "/",
        "/home",
        "~/.ssh",
        "~/.aws/x",
        "~/.config/gcloud",
        "~/.gnupg",
        "~/.config/agent-hub",
        "~/.local/share/agent-knowledge",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    v.push(app.join("policy").to_string_lossy().to_string());
    v.push(app.join("backend/src").to_string_lossy().to_string());
    v.push("/tmp/x/.git".into());
    v.push("/tmp/x/.git/hooks".into());
    v
}

#[test]
fn test_hostile_roots_are_refused_at_creation_and_in_plans() -> R {
    if !support_wsf::reexec(
        "test_hostile_roots_are_refused_at_creation_and_in_plans",
        &[("HOME", support_wsf::FIXED_HOME)],
    )? {
        return Ok(()); // it ran, and passed, in a child process with HOME = FIXED_HOME
    }
    for root in hostile_roots() {
        let d = tempfile::tempdir()?;
        let hub = support_wsf::hub(d.path())?;
        let doc = json!({"adapter": "fake", "display_name": "E", "roots": {"home": root}});
        let e = hub
            .put_client("evil", &support_wsf::dict(&doc), true)
            .err()
            .ok_or(format!("{root} accepted"))?;
        assert!(
            e.status == 422 && e.detail.contains("invalid roots"),
            "{root}: {}",
            e.detail
        );
        // a client file that bypassed the API is still refused by the planner
        std::fs::write(
            hub.cfg().content_dir.join("clients").join("evil.yaml"),
            format!("id: evil\nadapter: fake\ndisplay_name: E\nroots:\n  home: '{root}'\n"),
        )?;
        support_wsf::commit(&hub)?;
        let plan = support_wsf::plan(&hub, Some(&["evil"]))?;
        let cp = plan.clients.first().ok_or("one client")?;
        assert!(cp.blocked && cp.diagnostics[0]["code"] == "bad_root", "{root}");
    }
    Ok(())
}

#[test]
fn test_roots_that_contain_or_are_the_hub_state_are_refused() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let c = hub.cfg();
    let parent = c.data_dir.parent().map(Path::to_path_buf).ok_or("data dir parent")?;
    for root in [
        c.data_dir.clone(),
        c.data_dir.join("backups"),
        parent,
        c.content_dir.clone(),
    ] {
        let doc = json!({"adapter": "fake", "display_name": "E", "roots": {"home": root.to_string_lossy()}});
        let e = hub
            .put_client("evil", &support_wsf::dict(&doc), true)
            .err()
            .ok_or("accepted")?;
        assert_eq!(e.status, 422, "{}", root.display());
    }
    Ok(())
}

/// Python appends `proj/hub/policy` and `proj/hub/backend` to the live hub's `guard.inside_only`. The Rust guard is
/// shared immutably (`Arc`), so the same situation is built honestly instead: the test re-runs itself with
/// AGENT_HUB_APP_ROOT=<tmp>/proj/hub, which makes the whole `proj/hub` tree the protected app tree.
#[test]
fn test_project_root_containing_the_app_is_allowed_but_targets_inside_protected_dirs_are_not() -> R {
    const NAME: &str = "test_project_root_containing_the_app_is_allowed_but_targets_inside_protected_dirs_are_not";
    let tmp = match std::env::var("WSF_TMP") {
        Ok(t) => PathBuf::from(t),
        Err(_) => {
            let d = tempfile::tempdir()?;
            let app = d.path().join("proj").join("hub");
            std::fs::create_dir_all(app.join("policy"))?;
            std::fs::create_dir_all(app.join("backend"))?;
            std::fs::copy(app_root().join("policy/floor.yaml"), app.join("policy/floor.yaml"))?;
            support_wsf::reexec(
                NAME,
                &[
                    ("WSF_TMP", &d.path().to_string_lossy()),
                    ("AGENT_HUB_APP_ROOT", &app.to_string_lossy()),
                ],
            )?;
            return Ok(());
        }
    };
    let sneaky = FakeAdapter::scripted("sneaky", |_ctx| {
        [
            "hub/policy/floor.yaml",
            "hub/backend/src/x.py",
            "a/.git/config",
            "ok.txt",
        ]
        .iter()
        .map(|p| Artifact::new("home", p, b"x".to_vec(), "skill"))
        .collect()
    });
    let hub = support_wsf::hub_with(&tmp, &[], vec![sneaky])?;
    let proj = tmp.join("proj");
    let doc = json!({"adapter": "sneaky", "display_name": "S", "roots": {"home": proj.to_string_lossy()}});
    hub.put_client("sn", &support_wsf::dict(&doc), true).map_err(p)?; // the root itself is fine
    support_wsf::commit(&hub)?;
    let plan = support_wsf::plan(&hub, Some(&["sn"]))?;
    let cp = plan.clients.first().ok_or("one client")?;
    let msgs: Vec<_> = cp.diagnostics.iter().filter(|d| d["code"] == "unsafe_path").collect();
    assert!(msgs.len() == 3 && cp.blocked);
    assert!(!proj.join("hub/policy/floor.yaml").exists() || std::fs::read(proj.join("hub/policy/floor.yaml"))? != b"x");
    Ok(())
}

#[test]
fn test_generic_spec_cannot_write_hooks_or_git() -> R {
    for f in ["hooks/pre-commit", ".git/hooks/pre-commit", "sub/hooks/x"] {
        let d = tempfile::tempdir()?;
        let generic: Arc<dyn ClientAdapter> = agent_hub::adapters::generic::adapter()
            .ok_or("the generic adapter is not built")?
            .map_err(|e| -> E { e.into() })?;
        let hub = support_wsf::hub_with(d.path(), &[], vec![generic])?;
        let proj = d.path().join("proj");
        std::fs::create_dir_all(&proj)?;
        let spec = json!({"version": 1, "instructions": {"root": "project", "file": f, "mode": "own"}});
        let doc = json!({"adapter": "generic", "display_name": "G", "roots": {"project": proj.to_string_lossy()}, "spec": spec});
        hub.put_client("gen", &support_wsf::dict(&doc), true).map_err(p)?;
        support_wsf::put(&hub, "instruction", "i1", json!({"title": "I", "body": "x\n"}))?;
        support_wsf::put_profile(&hub, "gen", json!({"enable": {"instructions": ["i1"]}}))?;
        hub.commit("g", "t").map_err(p)?;
        let plan = support_wsf::plan(&hub, Some(&["gen"]))?;
        let cp = plan.clients.first().ok_or("one client")?;
        assert!(
            cp.blocked && cp.diagnostics.iter().any(|d| d["code"] == "unsafe_path"),
            "{f}"
        );
        assert!(!proj.join(f).exists(), "{f}");
    }
    Ok(())
}

#[test]
fn test_normal_roots_still_work() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let home = support_wsf::client_home(d.path())?;
    support_wsf::add_client(&hub, &home, "fake1", &json!({}))
}

// ---- the guard itself (runnable now) ----------------------------------------------------------------------------------

/// A guard built exactly like Hub::new builds it (real floor, temp data/content dirs, the real app root).
fn guard(tmp: &Path) -> Result<TargetGuard, E> {
    let policy = load_floor(&app_root().join("policy/floor.yaml")).map_err(p)?;
    Ok(TargetGuard::new(
        &policy,
        &tmp.join("state"),
        &tmp.join("content"),
        &app_root(),
        None,
    ))
}

/// S-TARGETS: every hostile root of the Python test is refused by `root_problem` after `expand_root`.
#[test]
fn targets_hostile_roots_are_refused_by_the_guard() -> R {
    if !support_wsf::reexec(
        "targets_hostile_roots_are_refused_by_the_guard",
        &[("HOME", support_wsf::FIXED_HOME)],
    )? {
        return Ok(()); // it ran, and passed, in a child process with HOME = FIXED_HOME
    }
    let d = tempfile::tempdir()?;
    let g = guard(d.path())?;
    for root in hostile_roots() {
        let rp = expand_root(&root).map_err(|e| e.0)?;
        assert!(g.root_problem(&rp).is_some(), "{root}");
    }
    let home = support_wsf::client_home(d.path())?;
    assert_eq!(g.root_problem(&home), None);
    Ok(())
}

/// S-TARGETS: the hub's own state can be neither a root nor inside one, and a root may not contain it.
#[test]
fn targets_hub_state_roots_are_refused_by_the_guard() -> R {
    let d = tempfile::tempdir()?;
    let g = guard(d.path())?;
    let state = d.path().join("state");
    std::fs::create_dir_all(state.join("backups"))?;
    for root in [
        state.clone(),
        state.join("backups"),
        d.path().to_path_buf(),
        d.path().join("content"),
    ] {
        assert!(g.root_problem(&root).is_some(), "{}", root.display());
    }
    Ok(())
}

/// S-TARGETS: a symlink cannot smuggle a target into a protected directory (paths are realpath'ed first).
#[test]
fn targets_symlinks_are_resolved_before_the_check() -> R {
    let d = tempfile::tempdir()?;
    let g = guard(d.path())?;
    let state = d.path().join("state");
    std::fs::create_dir_all(&state)?;
    let proj = d.path().join("proj");
    std::fs::create_dir_all(&proj)?;
    std::os::unix::fs::symlink(&state, proj.join("link"))?;
    let msg = g
        .target_problem(&proj.join("link").join("x"), "link/x", false)
        .ok_or("refused")?;
    assert!(msg.contains("is inside the protected path"), "{msg}");
    assert!(g.root_problem(&proj.join("link")).is_some());
    assert_eq!(g.target_problem(&proj.join("ok.txt"), "ok.txt", false), None);
    // .git and (for generic specs) hooks segments are refused whatever the target
    assert!(g.target_problem(&proj.join("x"), "a/.git/config", false).is_some());
    assert!(g.target_problem(&proj.join("x"), "sub/hooks/x", true).is_some());
    assert_eq!(g.target_problem(&proj.join("x"), "sub/hooks/x", false), None);
    Ok(())
}

/// S-TARGETS: a root inside the app tree is refused except under out/, the adapters' staging area.
#[test]
fn targets_app_tree_except_out() -> R {
    let d = tempfile::tempdir()?;
    let g = guard(d.path())?;
    let app = app_root();
    let msg = g.root_problem(&app.join("docs")).ok_or("refused")?;
    assert!(
        msg.ends_with(&format!(
            "is inside the protected path {}",
            hub_core::py::os::realpath(&app).display()
        )),
        "{msg}"
    );
    assert_eq!(g.root_problem(&app.join("out/x")), None);
    Ok(())
}

/// Audit F12: the login-shell, git, Docker, Kubernetes and registry files are protected like ~/.bashrc: no root may be
/// one of them or contain them, and no artifact may be written into them. The floor denies reading the credentials.
#[test]
fn targets_new_protected_home_paths_0_3_0() -> R {
    let d = tempfile::tempdir()?;
    let home = d.path().join("home");
    std::fs::create_dir_all(&home)?;
    let policy = load_floor(&app_root().join("policy/floor.yaml")).map_err(p)?;
    let g = TargetGuard::new(
        &policy,
        &d.path().join("state"),
        &d.path().join("content"),
        &app_root(),
        Some(&home),
    );
    for rel in [
        ".bash_profile",
        ".bash_login",
        ".zprofile",
        ".zshenv",
        ".config/fish",
        ".config/environment.d",
        ".config/git",
        ".gitconfig",
        ".git-credentials",
        ".docker",
        ".kube",
        ".npmrc",
        ".pypirc",
    ] {
        let t = home.join(rel);
        assert!(g.target_problem(&t, "x", false).is_some(), "write into {rel}");
        assert!(
            g.target_problem(&t.join("x"), "x", false).is_some(),
            "write under {rel}"
        );
        assert!(g.root_problem(&t).is_some(), "root {rel}");
    }
    assert!(g.root_problem(&home.join(".config")).is_some()); // contains .config/git, .config/fish, ...
    assert_eq!(
        g.target_problem(&home.join(".claude/CLAUDE.md"), "CLAUDE.md", false),
        None
    );
    for glob in [
        "~/.docker/**",
        "~/.kube/**",
        "~/.git-credentials",
        "~/.config/gh/**",
        "~/.npmrc",
    ] {
        assert!(policy.deny_path_read.iter().any(|g| g == glob), "{glob}");
    }
    Ok(())
}

/// Only `opencode` and `generic` may write a file named `AGENTS.md`, and only as managed instructions (Python's rule,
/// kept in 0.3.0). The stage adapters that render one (`hermes-agent`, `deepseek-harness`) are therefore blocked at
/// apply when instructions are on; the docs say to keep `instructions` off for them. Widening this is a security-rule
/// change for a later release (PORTING.md A-15).
#[test]
fn targets_only_opencode_and_generic_may_write_agents_md() {
    let on = indexmap::IndexMap::from([("instructions".to_string(), true)]);
    for adapter in ["opencode", "generic"] {
        assert_eq!(
            TargetGuard::sensitive_target(adapter, &on, "instruction", "AGENTS.md"),
            None,
            "{adapter}"
        );
    }
    for adapter in ["hermes-agent", "deepseek-harness", "claude-code", "turnstone"] {
        assert!(
            TargetGuard::sensitive_target(adapter, &on, "instruction", "AGENTS.md").is_some(),
            "{adapter}"
        );
    }
}
