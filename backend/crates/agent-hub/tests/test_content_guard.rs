//! Port of backend/tests/core/test_content_guard.py (N1, security control S-CONTENTGUARD): the content repo must not be
//! a code-execution vector for whoever can write into it. Most Python tests drive the gate through the hub
//! (`changes`, `commit`, `plan`, `check_content_safety`, `migrate_content`: WS-B2); `contentguard_*` tests below run
//! the same refusals against `services::contentguard` directly, plus a golden replay of `parse_config_keys` and
//! `sanitize_git` and the two deliberate (security) differences SB-1/SB-2.

mod support_wsf;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use agent_hub::config::{app_root, load_settings_from};
use agent_hub::services::contentguard::{
    self, check_location, check_ownership, check_ownership_as, hostile_findings, parse_config_keys, plain_head,
    sanitize_git, tighten, verify_all, verify_git_metadata,
};
use hub_core::Problem;
use hub_core::value::PyValue;
use serde_json::{Value, json};
use support_wsf::{E, R, p};

/// test_content_guard.py `unsafe(fn)`: the error is content_dir_unsafe and names the fix.
fn is_unsafe<T: std::fmt::Debug>(r: Result<T, Problem>) -> Result<Problem, E> {
    let e = r.err().ok_or("expected content_dir_unsafe")?;
    if e.code != "content_dir_unsafe" || !e.detail.contains("migrate-content") || e.status != 503 {
        return Err(format!("unexpected error: {e:?}").into());
    }
    Ok(e)
}

const HOSTILE_CONFIGS: [&str; 11] = [
    "[filter \"x\"]\n\tclean = touch /tmp/hub-pwn\n",
    "[core]\n\tfsmonitor = /tmp/evil\n",
    "[core]\n\tsshCommand = /tmp/evil\n",
    "[alias]\n\tstatus = !touch /tmp/hub-pwn\n",
    "[merge \"m\"]\n\tdriver = /tmp/evil\n",
    "[include]\n\tpath = /tmp/evil\n",
    "[includeIf \"gitdir:/\"]\n\tpath = /tmp/evil\n",
    "[diff]\n\texternal = /tmp/evil\n",
    "[core]\n\thooksPath = /tmp/evil\n",
    "[url \"x\"]\n\tinsteadOf = y\n",
    "[core]\n\tpager = /tmp/evil\n",
];

const GIT_INIT_CONFIG: &str =
    "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n";

/// A repo-shaped directory (what `git init` leaves, without running git): 0700 dirs, sample hooks, a plain config.
fn fake_repo(root: &Path) -> Result<PathBuf, E> {
    let git = root.join(".git");
    for sub in ["hooks", "info", "objects", "refs/heads"] {
        std::fs::create_dir_all(git.join(sub))?;
    }
    std::fs::write(git.join("config"), GIT_INIT_CONFIG)?;
    std::fs::write(git.join("hooks/pre-commit.sample"), "#!/bin/sh\n")?;
    std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n")?;
    tighten(root).map_err(p)?;
    Ok(git)
}

#[test]
fn test_default_content_dir_is_private_and_outside_the_lab() -> R {
    let d = tempfile::tempdir()?;
    let env = HashMap::from([
        ("HOME".to_string(), d.path().join("home").to_string_lossy().to_string()),
        (
            "AGENT_HUB_ENV_FILE".to_string(),
            d.path().join("no-env").to_string_lossy().to_string(),
        ),
    ]);
    let state = d.path().join("state");
    let s = load_settings_from(&env, &vec![("data_dir".into(), PyValue::str(state.to_string_lossy()))])?;
    assert_eq!(s.content_dir, state.join("content"));
    let s2 = load_settings_from(
        &env,
        &vec![
            ("data_dir".into(), PyValue::str(d.path().join("s").to_string_lossy())),
            (
                "content_dir".into(),
                PyValue::str(d.path().join("elsewhere").to_string_lossy()),
            ),
        ],
    )?;
    assert_eq!(s2.content_dir, d.path().join("elsewhere"));
    assert!(!s.content_dir.starts_with(app_root()));
    Ok(())
}

#[test]
fn test_fresh_repo_is_0700_and_passes() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let cd = hub.cfg().content_dir;
    assert_eq!(std::fs::metadata(&cd)?.permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(cd.join(".git"))?.permissions().mode() & 0o777, 0o700);
    hub.check_content_safety().map_err(p)?;
    Ok(())
}

#[test]
fn test_hostile_git_config_is_refused_before_any_git_runs() -> R {
    for cfgtext in HOSTILE_CONFIGS {
        let d = tempfile::tempdir()?;
        let hub = support_wsf::hub(d.path())?;
        let cfg = hub.cfg().content_dir.join(".git").join("config");
        let mut text = std::fs::read_to_string(&cfg)?;
        text.push_str(cfgtext);
        std::fs::write(&cfg, text)?;
        is_unsafe(hub.changes())?;
        is_unsafe(hub.commit("x", "t"))?;
        is_unsafe(hub.plan(None, "committed"))?;
    }
    assert!(!Path::new("/tmp/hub-pwn").exists());
    Ok(())
}

#[test]
fn test_allowed_config_keys_pass() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let cfg = hub.cfg().content_dir.join(".git").join("config");
    let text = std::fs::read_to_string(&cfg)? + "[user]\n\tname = Someone\n\temail = a@b\n";
    std::fs::write(&cfg, text)?;
    hub.check_content_safety().map_err(p)?;
    Ok(())
}

#[test]
fn test_gitattributes_and_info_attributes_are_refused() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let cd = hub.cfg().content_dir;
    let ga = cd.join("skills").join(".gitattributes");
    std::fs::create_dir_all(cd.join("skills"))?;
    std::fs::write(&ga, "* filter=x\n")?;
    is_unsafe(hub.check_content_safety())?;
    std::fs::write(&ga, "")?; // an EMPTY .gitattributes is harmless
    hub.check_content_safety().map_err(p)?;
    std::fs::write(cd.join(".git/info/attributes"), "* diff=evil\n")?;
    is_unsafe(hub.check_content_safety())?;
    Ok(())
}

#[test]
fn test_non_sample_hooks_are_refused() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let h = hub.cfg().content_dir.join(".git").join("hooks");
    assert!(
        std::fs::read_dir(&h)?
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".sample"))
    );
    std::fs::write(h.join("post-checkout"), "#!/bin/sh\n")?;
    is_unsafe(hub.check_content_safety())?;
    Ok(())
}

/// Python monkeypatches `os.getuid` for the foreign-owner half; the hub has no uid seam, so that half calls
/// `check_ownership_as` (what the hub's gate runs) with the other uid.
#[test]
fn test_group_or_world_writable_and_foreign_owner_are_refused() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let cd = hub.cfg().content_dir;
    for dir in [cd.clone(), cd.join(".git"), cd.join(".git/hooks")] {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770))?;
        is_unsafe(hub.check_content_safety())?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    hub.check_content_safety().map_err(p)?;
    is_unsafe(check_ownership_as(&cd, nix::unistd::getuid().as_raw() + 1))?;
    Ok(())
}

#[test]
fn test_git_dir_must_be_a_real_directory() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let real = hub.cfg().content_dir.join(".git");
    let moved = d.path().join("elsewhere.git");
    std::fs::rename(&real, &moved)?;
    std::os::unix::fs::symlink(&moved, &real)?;
    is_unsafe(hub.check_content_safety())?;
    Ok(())
}

/// Python first adds a client (WS-B1); the asserted calls are all `check_location` with explicit roots, so the
/// client is not needed here.
#[test]
fn test_content_inside_a_client_root_or_the_app_tree_is_refused() -> R {
    let d = tempfile::tempdir()?;
    let home = support_wsf::client_home(d.path())?;
    let roots = [home.to_string_lossy().to_string()];
    let app = app_root();
    is_unsafe(check_location(&home.join("content"), &roots, &app))?;
    is_unsafe(check_location(d.path(), &roots, &app))?; // a root INSIDE the content dir
    is_unsafe(check_location(&app.join("content"), &[], &app))?;
    is_unsafe(check_location(&app, &[], &app))?;
    check_location(&d.path().join("private"), &roots, &app).map_err(p)?; // a sibling is fine
    Ok(())
}

#[test]
fn test_client_root_containing_the_content_dir_stops_planning() -> R {
    let d = tempfile::tempdir()?;
    let wide = d.path().join("wide");
    let hub = support_wsf::hub_with(
        d.path(),
        &[("content_dir", PyValue::str(wide.join("content").to_string_lossy()))],
        Vec::new(),
    )?;
    std::fs::write(
        hub.cfg().content_dir.join("clients").join("wide.yaml"),
        format!(
            "id: wide\nadapter: fake\ndisplay_name: W\nroots:\n  home: '{}'\n",
            wide.display()
        ),
    )?;
    is_unsafe(hub.plan(None, "committed"))?;
    is_unsafe(hub.changes())?;
    Ok(())
}

/// The Python test sets hostile variables in the hub process and inspects `hub.repo._env()`. Rust cannot change its
/// own environment, so the test re-runs itself with them set and points `git_bin` at a wrapper that records the
/// environment git really gets, then execs git.
#[test]
fn test_git_child_environment_is_scrubbed() -> R {
    const NAME: &str = "test_git_child_environment_is_scrubbed";
    const HOSTILE: [&str; 8] = [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_EXEC_PATH",
        "GIT_ASKPASS",
        "GIT_EXTERNAL_DIFF",
        "PAGER",
        "EDITOR",
        "GIT_SSH_COMMAND",
    ];
    let tmp = match std::env::var("WSF_TMP") {
        Ok(t) => PathBuf::from(t),
        Err(_) => {
            let d = tempfile::tempdir()?;
            let ds = d.path().to_string_lossy().to_string();
            let evil = format!("{ds}/evil");
            let mut envs: Vec<(&str, &str)> = vec![("WSF_TMP", &ds)];
            for k in HOSTILE {
                envs.push((
                    k,
                    if k.starts_with("GIT_D") || k == "GIT_WORK_TREE" || k == "GIT_EXEC_PATH" {
                        &evil
                    } else {
                        "/tmp/x"
                    },
                ));
            }
            support_wsf::reexec(NAME, &envs)?;
            return Ok(());
        }
    };
    let log = tmp.join("git-env.log");
    let wrapper = tmp.join("git-wrapper");
    std::fs::write(
        &wrapper,
        format!("#!/bin/sh\nenv >> '{}'\nexec /usr/bin/git \"$@\"\n", log.display()),
    )?;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))?;
    let hub = support_wsf::hub_with(
        &tmp,
        &[("git_bin", PyValue::str(wrapper.to_string_lossy()))],
        Vec::new(),
    )?;
    support_wsf::put(&hub, "instruction", "a", json!({"title": "A", "body": "x\n"}))?;
    let out = hub.commit("still works", "t").map_err(p)?;
    assert!(out["commit"].as_str().is_some_and(|c| !c.is_empty()));
    let env = std::fs::read_to_string(&log)?;
    for k in HOSTILE {
        assert!(
            !env.lines().any(|l| l.starts_with(&format!("{k}="))),
            "{k} leaked into git's environment"
        );
    }
    for want in [
        "GIT_CONFIG_GLOBAL=/dev/null",
        "GIT_CONFIG_NOSYSTEM=1",
        "GIT_ATTR_NOSYSTEM=1",
    ] {
        assert!(env.lines().any(|l| l == want), "{want}");
    }
    Ok(())
}

// ---- migrate-content -----------------------------------------------------------------------------------------------

fn mode(p: &Path) -> Result<u32, E> {
    Ok(std::fs::metadata(p)?.permissions().mode() & 0o777)
}

#[test]
fn test_migrate_content_moves_the_repo_safely() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    support_wsf::put(&hub, "instruction", "a", json!({"title": "A", "body": "x\n"}))?;
    let head = hub.commit("one", "t").map_err(p)?["commit"].clone();
    let src = hub.cfg().content_dir;
    std::fs::set_permissions(src.join("instructions/a.md"), std::fs::Permissions::from_mode(0o640))?;
    let target = d.path().join("new/place/content");
    let dry = hub.migrate_content(&target.to_string_lossy(), false).map_err(p)?;
    assert!(dry["dry_run"] == true && src.exists() && !target.exists());
    let out = hub.migrate_content(&target.to_string_lossy(), true).map_err(p)?;
    assert!(out["head"] == head && !src.exists() && target.is_dir());
    assert!(mode(&target)? == 0o700 && mode(&target.join(".git"))? == 0o700);
    assert_eq!(mode(&target.join("instructions/a.md"))?, 0o640); // modes preserved
    let parent = target.parent().ok_or("parent")?;
    assert!(
        !std::fs::read_dir(parent)?
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with(".migrate-"))
    );
    Ok(())
}

#[test]
fn test_migrate_content_refusals_and_cleanup() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let home = support_wsf::client_home(d.path())?;
    support_wsf::add_client(&hub, &home, "fake1", &json!({}))?;
    let existing = d.path().join("exists");
    std::fs::create_dir(&existing)?;
    let e = hub
        .migrate_content(&existing.to_string_lossy(), true)
        .err()
        .ok_or("refused")?;
    assert_eq!(e.status, 409);
    let cd = hub.cfg().content_dir;
    for bad in [home.join("content"), app_root().join("content"), cd.join("inner")] {
        let e = hub
            .migrate_content(&bad.to_string_lossy(), true)
            .err()
            .ok_or("refused")?;
        assert!(
            e.status == 400 || e.code == "content_dir_unsafe",
            "{}: {e:?}",
            bad.display()
        );
    }
    // a hostile source is reported by the dry run (never executed)
    std::fs::write(
        cd.join(".git/hooks/pre-commit"),
        "#!/bin/sh\ntouch /tmp/hub-migrate-pwn\n",
    )?;
    let dry = hub
        .migrate_content(&d.path().join("t1").to_string_lossy(), false)
        .map_err(p)?;
    assert!(dry["dry_run"] == true && dry["findings"][0].as_str().is_some_and(|f| f.contains("hooks")));
    Ok(())
}

#[test]
fn test_cli_migrate_content() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let key = hub.auth.create_key("k", "admin", None, &[]).map_err(p)?["secret"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let target = d.path().join("moved");
    let c = hub.cfg();
    let run = |args: &[&str]| -> Result<(i32, String), E> {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_hubctl"))
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", support_wsf::home())
            .env("AGENT_HUB_ENV_FILE", d.path().join("no-env"))
            .env("HUB_DATA_DIR", &c.data_dir)
            .env("HUB_CONTENT_DIR", &c.content_dir)
            .env("HUB_API_KEY", &key)
            .stdin(std::process::Stdio::null())
            .output()?;
        Ok((
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        ))
    };
    let t = target.to_string_lossy().to_string();
    let (rc, out) = run(&["migrate-content", "--to", &t])?; // dry run needs --yes
    assert!(rc == 1 && out.contains("re-run with --yes") && !target.exists());
    let (rc, out) = run(&["migrate-content", "--to", &t, "--yes"])?;
    assert!(rc == 0 && out.contains(&t) && !out.contains("HUB_API_KEY") && target.is_dir());
    Ok(())
}

#[test]
fn test_migrate_rescues_a_hostile_group_writable_repo_inside_a_client_root() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let home = support_wsf::client_home(d.path())?;
    support_wsf::add_client(&hub, &home, "fake1", &json!({}))?;
    support_wsf::put(&hub, "instruction", "a", json!({"title": "A", "body": "x\n"}))?;
    let head = hub.commit("one", "t").map_err(p)?["commit"].clone();
    let src = home.join("content"); // inside the client root: exactly what the gate refuses
    let st = std::process::Command::new("cp")
        .arg("-a")
        .arg(hub.cfg().content_dir)
        .arg(&src)
        .status()?;
    assert!(st.success());
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o775))?;
    std::fs::set_permissions(src.join(".git"), std::fs::Permissions::from_mode(0o775))?;
    let pwn = d.path().join("hub-migrate-pwn2");
    let cfg = src.join(".git/config");
    let text = std::fs::read_to_string(&cfg)?
        + &format!(
            "[core]\n\tfsmonitor = touch {0}\n\tsshCommand = touch {0}\n[alias]\n\tfsck = !touch {0}\n",
            pwn.display()
        );
    std::fs::write(&cfg, text)?;
    std::fs::write(
        src.join(".git/hooks/post-checkout"),
        format!("#!/bin/sh\ntouch {}\n", pwn.display()),
    )?;
    std::fs::write(src.join(".gitattributes"), "* filter=evil\n")?;
    hub.cfg.write().map_err(|_| "settings lock")?.content_dir = src.clone();
    assert!(hub.check_content_safety().is_err()); // the normal gate still refuses it
    let target = d.path().join("safe/content");
    let dry = hub.migrate_content(&target.to_string_lossy(), false).map_err(p)?;
    assert!(dry["dry_run"] == true && dry["findings"].as_array().is_some_and(|f| !f.is_empty()));
    assert!(!target.exists() && !pwn.exists());
    let out = hub.migrate_content(&target.to_string_lossy(), true).map_err(p)?;
    assert!(!pwn.exists()); // nothing planted was ever executed
    assert!(out["head"] == head && !src.exists());
    assert!(mode(&target)? == 0o700 && mode(&target.join(".git"))? == 0o700);
    let sanitized: Vec<String> = out["sanitized"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_lowercase).collect())
        .unwrap_or_default();
    assert!(
        sanitized
            .iter()
            .any(|r| r.contains("fsmonitor") || r.contains("sshcommand"))
    );
    assert!(sanitized.iter().any(|r| r.contains("post-checkout")));
    verify_all(&target, &[home.to_string_lossy().to_string()], &app_root()).map_err(p)?; // the result passes the gate
    let log = std::process::Command::new("/usr/bin/git")
        .args([
            "-C",
            &target.to_string_lossy(),
            "-c",
            "core.hooksPath=/dev/null",
            "log",
            "--format=%s",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()?;
    assert!(String::from_utf8_lossy(&log.stdout).contains("one"));
    Ok(())
}

/// Python also monkeypatches `os.getuid` to make the SOURCE look foreign (BadRequest): `migrate_content_as` with
/// another uid (addendum A-8).
#[test]
fn test_migrate_refuses_an_unsafe_target_and_foreign_source() -> R {
    let d = tempfile::tempdir()?;
    let hub = support_wsf::hub(d.path())?;
    let home = support_wsf::client_home(d.path())?;
    support_wsf::add_client(&hub, &home, "fake1", &json!({}))?;
    assert!(
        hub.migrate_content(&home.join("new-content").to_string_lossy(), true)
            .is_err()
    ); // inside a client root
    assert!(hub.cfg().content_dir.is_dir());
    let other = nix::unistd::getuid().as_raw() + 1;
    let foreign = hub.migrate_content_as(&d.path().join("elsewhere").to_string_lossy(), true, other);
    assert_eq!(foreign.err().map(|p| p.status), Some(400));
    assert!(hub.cfg().content_dir.is_dir() && !d.path().join("elsewhere").exists());
    Ok(())
}

// ---- the gate itself (runnable now) -----------------------------------------------------------------------------------

/// S-CONTENTGUARD: every hostile config of the Python test is refused by verify_git_metadata, without running git.
#[test]
fn contentguard_hostile_git_configs_are_refused() -> R {
    for cfgtext in HOSTILE_CONFIGS {
        let d = tempfile::tempdir()?;
        let git = fake_repo(d.path())?;
        verify_git_metadata(d.path()).map_err(p)?;
        std::fs::write(git.join("config"), format!("{GIT_INIT_CONFIG}{cfgtext}"))?;
        let e = is_unsafe(verify_git_metadata(d.path()))?;
        assert!(
            e.detail.contains(".git/config contains keys outside the allowlist: ["),
            "{}",
            e.detail
        );
        assert!(
            hostile_findings(d.path())
                .map_err(p)?
                .is_some_and(|f| !f.contains("Keep the content repo"))
        );
    }
    assert!(!Path::new("/tmp/hub-pwn").exists());
    Ok(())
}

/// S-CONTENTGUARD: the allowlisted keys pass; .gitattributes / info/attributes / non-sample hooks are refused.
#[test]
fn contentguard_attributes_and_hooks() -> R {
    let d = tempfile::tempdir()?;
    let git = fake_repo(d.path())?;
    std::fs::write(
        git.join("config"),
        format!("{GIT_INIT_CONFIG}[user]\n\tname = Someone\n\temail = a@b\n"),
    )?;
    verify_all(d.path(), &[], &app_root()).map_err(p)?;
    std::fs::create_dir_all(d.path().join("skills"))?;
    let ga = d.path().join("skills/.gitattributes");
    std::fs::write(&ga, "* filter=x\n")?;
    let e = is_unsafe(verify_git_metadata(d.path()))?;
    assert!(e.detail.starts_with(&format!(
        "content directory is not safe: {} is not empty. ",
        ga.display()
    )));
    std::fs::write(&ga, "")?; // an EMPTY .gitattributes is harmless
    verify_git_metadata(d.path()).map_err(p)?;
    std::fs::write(git.join("info/attributes"), "* diff=evil\n")?;
    is_unsafe(verify_git_metadata(d.path()))?;
    std::fs::write(git.join("info/attributes"), "")?;
    std::fs::write(git.join("hooks/post-checkout"), "#!/bin/sh\n")?;
    let e = is_unsafe(verify_git_metadata(d.path()))?;
    assert!(
        e.detail
            .contains(".git/hooks contains non-sample files: ['post-checkout']"),
        "{}",
        e.detail
    );
    Ok(())
}

/// S-CONTENTGUARD: modes, owner and a real .git directory (check_ownership).
#[test]
fn contentguard_modes_owner_and_real_git_dir() -> R {
    let d = tempfile::tempdir()?;
    let root = d.path().join("c");
    let git = fake_repo(&root)?;
    check_ownership(&root).map_err(p)?;
    for dir in [root.clone(), git.clone(), git.join("hooks")] {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770))?;
        let e = is_unsafe(check_ownership(&root))?;
        assert!(
            e.detail.contains("is group- or world-writable (mode 0o770)"),
            "{}",
            e.detail
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let e = is_unsafe(check_ownership_as(&root, nix::unistd::getuid().as_raw() + 1))?;
    assert!(e.detail.contains("is not owned by the current user"));
    let moved = d.path().join("elsewhere.git");
    std::fs::rename(&git, &moved)?;
    std::os::unix::fs::symlink(&moved, &git)?;
    let e = is_unsafe(check_ownership(&root))?;
    assert!(
        e.detail
            .contains(".git must be a real directory (not a symlink or gitdir file)")
    );
    std::fs::remove_file(&git)?;
    std::fs::write(&git, "gitdir: /elsewhere\n")?;
    is_unsafe(check_ownership(&root))?;
    Ok(())
}

/// SB-1 (deliberate difference, security): sanitize_git never writes or deletes through a symlink. Python follows a
/// symlinked `.git/hooks` and deletes the link target's files, and truncates/overwrites the targets of symlinked
/// config/attributes files.
#[test]
fn contentguard_sanitize_never_follows_symlinks() -> R {
    let d = tempfile::tempdir()?;
    let outside = d.path().join("outside");
    std::fs::create_dir_all(outside.join("docs/sub"))?;
    std::fs::write(outside.join("docs/precious.txt"), "keep me")?;
    std::fs::write(outside.join("docs/sub/also.txt"), "keep me too")?;
    std::fs::write(
        outside.join("secret.conf"),
        "[core]\n\tfsmonitor = x\nprivate-key-material\n",
    )?;
    std::fs::write(outside.join("attrs"), "* diff=x\n")?;
    let root = d.path().join("copy");
    let git = fake_repo(&root)?;
    std::fs::remove_file(git.join("config"))?;
    std::os::unix::fs::symlink(outside.join("secret.conf"), git.join("config"))?;
    std::fs::remove_dir_all(git.join("hooks"))?;
    std::os::unix::fs::symlink(outside.join("docs"), git.join("hooks"))?;
    std::os::unix::fs::symlink(outside.join("attrs"), git.join("info/attributes"))?;
    std::os::unix::fs::symlink(outside.join("attrs"), root.join(".gitattributes"))?;
    // SB-2: the gate refuses the symlinked config without echoing its content
    let e = is_unsafe(verify_git_metadata(&root))?;
    assert!(
        e.detail.contains(".git/config is a symlink") && !e.detail.contains("private-key"),
        "{}",
        e.detail
    );
    let removed = sanitize_git(&root).map_err(p)?;
    assert_eq!(
        removed,
        vec![
            "config (symlink)",
            "hooks (symlink)",
            "info/attributes",
            ".gitattributes"
        ]
    );
    assert_eq!(std::fs::read_to_string(outside.join("docs/precious.txt"))?, "keep me");
    assert_eq!(
        std::fs::read_to_string(outside.join("docs/sub/also.txt"))?,
        "keep me too"
    );
    assert!(std::fs::read_to_string(outside.join("secret.conf"))?.contains("private-key-material"));
    assert_eq!(std::fs::read_to_string(outside.join("attrs"))?, "* diff=x\n");
    assert!(!std::fs::symlink_metadata(git.join("config"))?.file_type().is_symlink());
    tighten(&root).map_err(p)?;
    check_ownership(&root).map_err(p)?;
    verify_git_metadata(&root).map_err(p)?;
    Ok(())
}

/// tighten: directories 0700, files lose group/world write but keep their exec bits, symlinks untouched.
#[test]
fn contentguard_tighten_modes() -> R {
    let d = tempfile::tempdir()?;
    let root = d.path().join("r");
    std::fs::create_dir_all(root.join("a/b"))?;
    std::fs::write(root.join("a/x.sh"), "#!/bin/sh\n")?;
    std::fs::set_permissions(root.join("a/x.sh"), std::fs::Permissions::from_mode(0o777))?;
    std::fs::write(d.path().join("target"), "t")?;
    std::fs::set_permissions(d.path().join("target"), std::fs::Permissions::from_mode(0o666))?;
    std::os::unix::fs::symlink(d.path().join("target"), root.join("a/link"))?;
    std::fs::set_permissions(root.join("a/b"), std::fs::Permissions::from_mode(0o775))?;
    tighten(&root).map_err(p)?;
    assert_eq!(mode(&root)?, 0o700);
    assert_eq!(mode(&root.join("a/b"))?, 0o700);
    assert_eq!(mode(&root.join("a/x.sh"))?, 0o755);
    assert_eq!(mode(&d.path().join("target"))?, 0o666);
    Ok(())
}

#[test]
fn contentguard_plain_head_reads_refs_without_git() -> R {
    let d = tempfile::tempdir()?;
    let git = d.path().join(".git");
    std::fs::create_dir_all(git.join("refs/heads"))?;
    assert_eq!(plain_head(d.path()), None);
    std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n")?;
    assert_eq!(plain_head(d.path()), None);
    std::fs::write(
        git.join("packed-refs"),
        "# pack-refs with: peeled\nabc123 refs/heads/main\n",
    )?;
    assert_eq!(plain_head(d.path()).as_deref(), Some("abc123"));
    std::fs::write(git.join("refs/heads/main"), "def456\n")?;
    assert_eq!(plain_head(d.path()).as_deref(), Some("def456"));
    std::fs::write(git.join("HEAD"), "  0123abcd  \n")?;
    assert_eq!(plain_head(d.path()).as_deref(), Some("0123abcd"));
    std::fs::write(git.join("HEAD"), "\n")?;
    assert_eq!(plain_head(d.path()), None);
    Ok(())
}

// ---- golden table: parse_config_keys and sanitize_git against Python --------------------------------------------------

fn golden() -> Result<Value, E> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/floor_decisions.json");
    Ok(serde_json::from_str(&std::fs::read_to_string(p)?)?)
}

#[test]
fn golden_parse_config_keys_matches_python() -> R {
    let g = golden()?;
    let cases = g["config_keys"].as_array().ok_or("cases")?;
    let bad: Vec<&Value> = cases
        .iter()
        .filter(|c| json!(parse_config_keys(c["text"].as_str().unwrap_or_default())) != c["keys"])
        .collect();
    assert!(
        bad.is_empty() && !cases.is_empty(),
        "{} differ, first {:?}",
        bad.len(),
        bad.first()
    );
    Ok(())
}

#[test]
fn golden_sanitize_git_matches_python() -> R {
    let g = golden()?;
    let cases = g["sanitize"].as_array().ok_or("cases")?;
    let s = |v: &Value| v.as_str().map(str::to_string);
    for c in cases {
        let d = tempfile::tempdir()?;
        let root = d.path().join("repo");
        let git = root.join(".git");
        std::fs::create_dir_all(git.join("hooks"))?;
        std::fs::create_dir_all(git.join("info"))?;
        std::fs::write(git.join("config"), s(&c["config"]).unwrap_or_default())?;
        for h in c["hooks"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            std::fs::write(git.join("hooks").join(h), "#!/bin/sh\n")?;
        }
        if let Some(a) = s(&c["attrs"]) {
            std::fs::write(git.join("info/attributes"), a)?;
        }
        if let Some(a) = s(&c["ga"]) {
            std::fs::write(root.join(".gitattributes"), a)?;
        }
        if let Some(a) = s(&c["sub"]) {
            std::fs::create_dir_all(root.join("sub"))?;
            std::fs::write(root.join("sub/.gitattributes"), a)?;
        }
        let removed = sanitize_git(&root).map_err(p)?;
        let mut hooks: Vec<String> = std::fs::read_dir(git.join("hooks"))?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        hooks.sort();
        let read = |p: PathBuf, present: bool| -> Result<Option<String>, E> {
            Ok(if present {
                Some(std::fs::read_to_string(p)?)
            } else {
                None
            })
        };
        let got = json!({
            "removed": removed,
            "config_after": std::fs::read_to_string(git.join("config"))?,
            "hooks_after": hooks,
            "attrs_after": read(git.join("info/attributes"), !c["attrs"].is_null())?,
            "ga_after": read(root.join(".gitattributes"), !c["ga"].is_null())?,
            "sub_after": read(root.join("sub/.gitattributes"), !c["sub"].is_null())?,
            "hostile_after": hostile_findings(&root).map_err(p)?,
        });
        for k in [
            "removed",
            "config_after",
            "hooks_after",
            "attrs_after",
            "ga_after",
            "sub_after",
            "hostile_after",
        ] {
            assert_eq!(got[k], c[k], "{k} of {c}");
        }
    }
    assert!(!cases.is_empty());
    let _ = contentguard::CODE;
    Ok(())
}
