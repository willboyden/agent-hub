//! config.py: `HUB_*` environment variables plus the optional per-user env file (`AGENT_HUB_ENV_FILE`, default
//! `~/.config/agent-hub/env`, python-dotenv syntax). Real environment variables win over the file; names are
//! case-insensitive (pydantic-settings `case_sensitive=False`); list fields are JSON; a bad value refuses to start with
//! pydantic's message. Owner: CORE (LOCKED).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hub_core::dotenv;
#[cfg(test)]
use hub_core::value::PyValue;

/// The repository root (Python: `Path(__file__).resolve().parents[3]`). `AGENT_HUB_APP_ROOT` wins; otherwise the
/// checkout the running binary was built in, found from the executable's own (resolved) location by
/// [`app_root_for_exe`]. DEVIATION D-APPROOT: a Rust binary has no source file next to the policy and frontend at run
/// time; no path of the build machine is compiled in (audit F2).
pub fn app_root() -> PathBuf {
    if let Some(v) = std::env::var_os("AGENT_HUB_APP_ROOT") {
        return PathBuf::from(v);
    }
    let exe = std::env::current_exe()
        .map(|p| hub_core::py::os::realpath(&p))
        .unwrap_or_default();
    app_root_for_exe(&exe)
}

/// Cargo puts the binaries in `<checkout>/backend/target/<profile>/` (`.../<profile>/deps/` for test binaries,
/// `.../<target triple>/<profile>/` when cross-building), so the checkout is the directory above the nearest
/// `backend/target` that contains the executable. A binary moved out of its build tree (or built with a
/// `CARGO_TARGET_DIR` elsewhere) needs `AGENT_HUB_APP_ROOT`; without it the app root is the binary's own directory, where
/// no policy file exists, and the hub refuses to start and names the missing file.
pub fn app_root_for_exe(exe: &Path) -> PathBuf {
    for dir in exe.ancestors().skip(1) {
        let Some(backend) = dir.parent() else { break };
        if dir.file_name().is_some_and(|n| n == "target")
            && backend.file_name().is_some_and(|n| n == "backend")
            && let Some(root) = backend.parent()
        {
            return root.to_path_buf();
        }
    }
    exe.parent().map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
}

/// `Path.home()`.
pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

/// `ENV_FILE` (from the process environment).
pub fn env_file() -> PathBuf {
    let env: HashMap<String, String> = std::env::vars().collect();
    env_file_from(&env)
}

/// `ENV_FILE` for a given environment: `AGENT_HUB_ENV_FILE`, else `$HOME/.config/agent-hub/env`.
pub fn env_file_from(environ: &HashMap<String, String>) -> PathBuf {
    match environ.get("AGENT_HUB_ENV_FILE") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home_from(environ).join(".config").join("agent-hub").join("env"),
    }
}

/// The settings model, its loader and `is_loopback_host` live in `hub_core::settings` (pure, so the config editor's
/// check also runs in the browser); they are re-exported here under their old paths.
pub use hub_core::settings::{
    EVAL_GATES, EVAL_TIERS, MAX_DRAFT_EXPANSION, Overrides, Settings, SettingsError, is_loopback_host,
    settings_env_names,
};
use hub_core::settings::{home_from, settings_from_sources};

/// `load_settings(**overrides)` with the given environment (normally `std::env::vars()`).
pub fn load_settings_from(environ: &HashMap<String, String>, overrides: &Overrides) -> Result<Settings, SettingsError> {
    // env file (lowest of the sources), then real env (wins); keys are case-insensitive
    let ef = env_file_from(environ);
    let mut file_vals: Vec<(String, Option<String>)> = Vec::new();
    if ef.is_file()
        && let Ok(text) = std::fs::read_to_string(&ef)
    {
        file_vals = dotenv::values(&text, environ);
    }
    settings_from_sources(environ, file_vals, overrides, &app_root)
}

/// The settings a draft env file gives on its own, read exactly as [`load_settings_from`] reads the real one but with
/// an empty environment (`hub_core::settings::settings_from_env_text`, with this hub's app root for the default
/// `policy_file`). Used by the config editor's dry run; nothing is read from or written to disk.
pub fn settings_from_env_text(text: &str) -> Result<Settings, SettingsError> {
    hub_core::settings::settings_from_env_text(text, &app_root)
}

/// `load_settings(**overrides)` from the process environment.
pub fn load_settings(overrides: &Overrides) -> Result<Settings, SettingsError> {
    let env: HashMap<String, String> = std::env::vars().collect();
    load_settings_from(&env, overrides)
}

/// `check_bind(host)`: refuse a non-loopback bind unless `HUB_ALLOW_NONLOOPBACK=1`. Loopback is exactly `localhost` or
/// an IP address that is loopback (127.0.0.0/8, ::1). Python accepted any host starting with `127.`, so a NAME such as
/// `127.example.org` passed as loopback; it is now refused like any other name (audit F10, deviation D-BIND).
pub fn check_bind(host: &str) -> Result<(), String> {
    if is_loopback_host(host) {
        return Ok(());
    }
    if std::env::var("HUB_ALLOW_NONLOOPBACK").as_deref() != Ok("1") {
        return Err(format!(
            "refusing to bind {}: the hub is loopback-only. Set HUB_ALLOW_NONLOOPBACK=1 to override (and put an authenticating reverse proxy in front)",
            hub_core::py::repr(host)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// Audit F2: the app root comes from where the binary is, never from a path compiled into it.
    #[test]
    fn app_root_is_found_from_the_executable() {
        for (exe, root) in [
            ("/srv/hub/backend/target/release/agent-hub", "/srv/hub"),
            ("/srv/hub/backend/target/debug/hubctl", "/srv/hub"),
            ("/srv/hub/backend/target/debug/deps/test_apply-0123abcd", "/srv/hub"),
            (
                "/srv/hub/backend/target/x86_64-unknown-linux-gnu/release/agent-hub",
                "/srv/hub",
            ),
            (
                "/a/backend/target/b/backend/target/release/agent-hub",
                "/a/backend/target/b",
            ), // the nearest one
            ("/usr/local/bin/agent-hub", "/usr/local/bin"), // moved out of its build tree
            ("/srv/target/release/agent-hub", "/srv/target/release"), // no backend/target
        ] {
            assert_eq!(app_root_for_exe(Path::new(exe)), PathBuf::from(root), "{exe}");
        }
        // this test binary itself runs from backend/target/<profile>/deps of the checkout
        assert!(app_root().join("policy/floor.yaml").is_file());
        assert!(app_root().join("backend/Cargo.toml").is_file());
    }

    #[test]
    fn test_check_bind() {
        assert!(check_bind("127.0.0.1").is_ok());
        assert!(check_bind("127.1.2.3").is_ok());
        assert!(check_bind("::1").is_ok());
        assert!(check_bind("localhost").is_ok());
        assert!(check_bind("127.255.255.254").is_ok());
        // audit F10: names that merely start with "127." are not loopback (HUB_ALLOW_NONLOOPBACK is unset in tests)
        for name in [
            "127.evil.example",
            "127.0.0.1.nip.io",
            "127.",
            "0127.0.0.1",
            "localhost.example",
            "::ffff:8.8.8.8",
        ] {
            assert!(check_bind(name).is_err(), "{name}");
        }
        assert_eq!(
            check_bind("0.0.0.0").err().unwrap_or_default(),
            "refusing to bind '0.0.0.0': the hub is loopback-only. Set HUB_ALLOW_NONLOOPBACK=1 to override (and put an authenticating reverse proxy in front)"
        );
    }

    #[test]
    fn test_env_file_is_read_but_real_env_wins() -> Result<(), Box<dyn std::error::Error>> {
        let d = tempfile::tempdir()?;
        let f = d.path().join("env");
        std::fs::write(&f, "HUB_PORT=9001\nHUB_GIT_AUTHOR_NAME=FromFile\n")?;
        let file = f.to_string_lossy().to_string();
        let mut e = env(&[("HOME", "/h"), ("AGENT_HUB_ENV_FILE", &file)]);
        assert_eq!(load_settings_from(&e, &vec![])?.port, 9001);
        e.insert("HUB_PORT".into(), "9002".into());
        let s = load_settings_from(&e, &vec![])?;
        assert_eq!((s.port, s.git_author_name.as_str()), (9002, "FromFile"));
        Ok(())
    }

    #[test]
    fn test_autouse_fixture_ignores_the_users_env_file() -> Result<(), SettingsError> {
        // the Rust tests never read the developer's env file: every load names a missing one explicitly
        let e = env(&[("HOME", "/h"), ("AGENT_HUB_ENV_FILE", "/nonexistent/agent-hub-env")]);
        assert!(!env_file_from(&e).exists());
        assert_eq!(load_settings_from(&e, &vec![])?.port, 8792);
        let s = load_settings_from(
            &e,
            &vec![
                ("knowledge_token".into(), PyValue::str("hunter2")),
                ("knowledge_admin_key".into(), PyValue::str("hunter3")),
            ],
        )?;
        assert_eq!(s.knowledge_token.as_deref(), Some("hunter2"));
        let shown = format!("{s:?}");
        assert!(!shown.contains("token") && !shown.contains("hunter") && !shown.contains("knowledge_admin_key:"));
        Ok(())
    }

    #[test]
    fn env_values_parse_like_pydantic_settings() -> Result<(), SettingsError> {
        let e = env(&[
            ("HOME", "/h"),
            ("hub_port", "9000"),
            ("HUB_TRUST_LOOPBACK", "yes"),
            ("HUB_ALLOWED_HOSTS", "[\"a:1\"]"),
            ("KNOWLEDGE_ADMIN_KEY_FILE", "/k"),
            ("AGENT_HUB_ENV_FILE", "/nonexistent"),
        ]);
        let s = load_settings_from(&e, &vec![("data_dir".into(), PyValue::str("/d"))])?;
        assert_eq!(s.port, 9000);
        assert!(s.trust_loopback);
        assert_eq!(s.allowed_hosts, vec!["a:1".to_string()]);
        assert_eq!(s.knowledge_admin_key_file, PathBuf::from("/k"));
        assert_eq!(s.content_dir, PathBuf::from("/d/content"));
        let bad = load_settings_from(&env(&[("HUB_PORT", "x")]), &vec![]);
        let msg = bad.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            msg.starts_with("1 validation error for Settings\nport\n  Input should be a valid integer"),
            "{msg}"
        );
        Ok(())
    }
}
