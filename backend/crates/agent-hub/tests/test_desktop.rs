//! Port of backend/tests/core/test_desktop.py: `agent-hub open` / `stop` (URL, ownership check, background start and
//! stop). The Python tests monkeypatch desktop's functions; here a fake [`Launcher`] stands in for them, so nothing is
//! opened or signalled. The probe tests talk to real local sockets (answers recorded from Python's own probe()), and
//! the background-start tests spawn a harmless command in place of the hub.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use agent_hub::config::{Settings, load_settings_from};
use agent_hub::desktop::{
    Check, HubProcess, LOG_ROTATE_BYTES, Launcher, LauncherError, classify, open_hub_with, probe, run_with,
    start_background_argv, stop_hub_with,
};
use hub_core::value::PyValue;

type R = Result<(), Box<dyn std::error::Error>>;
const URL: &str = "http://127.0.0.1:8792/";
const SERVER: &str = "/opt/agent-hub/bin/agent-hub";

fn me() -> u32 {
    nix::unistd::getuid().as_raw()
}

fn settings(host: &str, port: i64, data_dir: &Path) -> Result<Settings, Box<dyn std::error::Error>> {
    let env = std::collections::HashMap::from([
        ("HOME".to_string(), data_dir.join("home").to_string_lossy().to_string()),
        (
            "AGENT_HUB_ENV_FILE".to_string(),
            "/nonexistent/agent-hub-env".to_string(),
        ),
    ]);
    let mut c = load_settings_from(
        &env,
        &vec![("data_dir".into(), PyValue::str(data_dir.to_string_lossy()))],
    )?;
    c.host = host.into();
    c.port = port;
    Ok(c)
}

fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0)
}

// ---- the fake launcher ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Rec {
    checks: Vec<Check>,
    started: u32,
    opened: Vec<String>,
    notes: Vec<String>,
    alive: Vec<(u32, Vec<String>)>,
    signals: Vec<i128>,
}

/// tests' `FakeProc`: pid 4242, an optional exit code, and whether it was stopped.
struct FakeProc {
    exit_code: Option<i32>,
    terminated: Arc<AtomicBool>,
}

impl HubProcess for FakeProc {
    fn pid(&self) -> u32 {
        4242
    }
    fn poll(&mut self) -> Option<i32> {
        self.exit_code
    }
    fn stop(&mut self) {
        self.terminated.store(true, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Fake {
    rec: Mutex<Rec>,
    exit_code: Option<i32>,
    terminated: Arc<AtomicBool>,
    start_timeout_s: Option<f64>,
}

impl Fake {
    fn with_checks(checks: &[Check]) -> Self {
        let f = Self::default();
        f.r().checks = checks.to_vec();
        f
    }
    fn r(&self) -> MutexGuard<'_, Rec> {
        self.rec.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Launcher for Fake {
    fn check(&self, url: &str, port: i64) -> Result<Check, LauncherError> {
        assert!(url == URL || url == "http://[::1]:8792/", "{url}");
        assert_eq!(port, 8792);
        let mut r = self.r();
        Ok(if r.checks.len() > 1 {
            r.checks.remove(0)
        } else {
            r.checks[0]
        })
    }
    fn start_background(&self, cfg: &Settings) -> Result<Box<dyn HubProcess>, LauncherError> {
        self.r().started += 1;
        std::fs::write(cfg.data_dir.join("hub.pid"), "4242\n").map_err(|e| LauncherError(e.to_string()))?;
        Ok(Box::new(FakeProc {
            exit_code: self.exit_code,
            terminated: self.terminated.clone(),
        }))
    }
    fn open_browser(&self, url: &str) -> bool {
        self.r().opened.push(url.to_string());
        true
    }
    fn notify(&self, message: &str) {
        self.r().notes.push(message.to_string());
    }
    fn pid_cmdline(&self, pid: i128) -> Option<(u32, Vec<String>)> {
        assert_eq!(pid, 4242);
        let mut r = self.r();
        if r.alive.is_empty() {
            None
        } else {
            Some(r.alive.remove(0))
        }
    }
    fn terminate(&self, pid: i128) -> Result<(), LauncherError> {
        self.r().signals.push(pid);
        Ok(())
    }
    fn server_argv(&self) -> Vec<String> {
        vec![SERVER.to_string()]
    }
    fn say(&self, _line: &str) {}
    fn poll_s(&self) -> f64 {
        0.0
    }
    fn start_timeout_s(&self) -> f64 {
        self.start_timeout_s.unwrap_or(30.0)
    }
}

// ---- who holds the port ----------------------------------------------------------------------------------------------

#[test]
fn test_check_flags_a_hub_shaped_answer_from_another_users_socket() {
    assert_eq!(classify(Check::Hub, Some(&BTreeSet::from([0]))), Check::OtherUser);
    assert_eq!(
        classify(Check::Hub, Some(&BTreeSet::from([i128::from(me()), 0]))),
        Check::OtherUser
    );
    assert_eq!(classify(Check::Hub, None), Check::OtherUser); // unreadable: fail closed
    assert_eq!(classify(Check::Hub, Some(&BTreeSet::new())), Check::OtherUser);
    assert_eq!(
        classify(Check::Hub, Some(&BTreeSet::from([i128::from(me())]))),
        Check::Hub
    );
    assert_eq!(classify(Check::Foreign, Some(&BTreeSet::from([0]))), Check::Foreign);
}

// ---- probe -------------------------------------------------------------------------------------------------------

const HEALTH: &str = r#"{"status": "ok", "version": "0.1.0", "content_initialised": true, "adapters": ["claude-code"], "trust_loopback": false, "warnings": []}"#;

fn resp(body: &[u8]) -> Vec<u8> {
    let mut v = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    v.extend_from_slice(body);
    v
}

/// A one-shot local server: reads the request head, then sends `payload` in pieces of `piece` bytes, `delay` apart.
fn serve(payload: Vec<u8>, piece: usize, delay: Duration) -> Result<u16, Box<dyn std::error::Error>> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    let port = l.local_addr()?.port();
    std::thread::spawn(move || {
        if let Ok((mut c, _)) = l.accept() {
            let _ = c.set_read_timeout(Some(Duration::from_secs(2)));
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                match c.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
            for chunk in payload.chunks(piece.max(1)) {
                if c.write_all(chunk).is_err() {
                    return;
                }
                std::thread::sleep(delay);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    Ok(port)
}

fn probe_answer(payload: Vec<u8>) -> Result<Check, Box<dyn std::error::Error>> {
    let port = serve(payload, usize::MAX, Duration::ZERO)?;
    Ok(probe(&format!("http://127.0.0.1:{port}/"), 2.0))
}

#[test]
fn test_probe_tells_hub_foreign_and_down_apart() -> R {
    let h = HEALTH.as_bytes();
    assert_eq!(probe_answer(resp(h))?, Check::Hub);
    assert_eq!(probe_answer(resp(b"<html>some other service</html>"))?, Check::Foreign);
    assert_eq!(probe_answer(resp(br#"{"status": "ok"}"#))?, Check::Foreign);
    let closed = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port(); // nothing listens once dropped
    assert_eq!(probe(&format!("http://127.0.0.1:{closed}/"), 2.0), Check::Down);
    // more answers, each recorded from Python's own desktop.probe() (httpx 0.28 / h11)
    let mut chunked = format!("HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n", h.len()).into_bytes();
    chunked.extend_from_slice(h);
    chunked.extend_from_slice(b"\r\n0\r\n\r\n");
    let mut bom = b"\xef\xbb\xbf".to_vec();
    bom.extend_from_slice(h);
    let with_head = |head: &str| {
        let mut v = head.as_bytes().to_vec();
        v.extend_from_slice(h);
        v
    };
    let cases: Vec<(&str, Vec<u8>, Check)> = vec![
        (
            "hub-500",
            String::from_utf8_lossy(&resp(h))
                .replace("200 OK", "500 Internal Server Error")
                .into_bytes(),
            Check::Hub,
        ),
        ("hub-chunked", chunked, Check::Hub),
        ("hub-bom", resp(&bom), Check::Hub),
        ("hub-until-close", with_head("HTTP/1.1 200 OK\r\n\r\n"), Check::Hub),
        (
            "hub-http10",
            String::from_utf8_lossy(&resp(h))
                .replace("HTTP/1.1", "HTTP/1.0")
                .into_bytes(),
            Check::Hub,
        ),
        (
            "hub-after-100",
            [b"HTTP/1.1 100 Continue\r\n\r\n".to_vec(), resp(h)].concat(),
            Check::Hub,
        ),
        ("ssh-banner", b"SSH-2.0-OpenSSH_9.6\r\n".to_vec(), Check::Down),
        ("close-silently", Vec::new(), Check::Down),
        (
            "short-body",
            with_head("HTTP/1.1 200 OK\r\ncontent-length: 500\r\n\r\n"),
            Check::Down,
        ),
        (
            "bad-content-length",
            with_head("HTTP/1.1 200 OK\r\ncontent-length: abc\r\n\r\n"),
            Check::Down,
        ),
        (
            "gzip-te",
            with_head("HTTP/1.1 200 OK\r\ntransfer-encoding: gzip\r\n\r\n"),
            Check::Down,
        ),
        ("204", b"HTTP/1.1 204 No Content\r\n\r\n".to_vec(), Check::Foreign),
        (
            "huge-head",
            format!(
                "HTTP/1.1 200 OK\r\nx-pad: {}\r\ncontent-length: 2\r\n\r\n{{}}",
                "a".repeat(20000)
            )
            .into_bytes(),
            Check::Foreign,
        ),
    ];
    for (name, payload, want) in cases {
        assert_eq!(probe_answer(payload)?, want, "{name}");
    }
    Ok(())
}

#[test]
fn test_probe_caps_the_body_and_the_total_read_time() -> R {
    let pad = vec![b' '; 100 * 1024]; // 100 KiB of padding before any JSON
    assert_eq!(probe_answer(resp(&pad))?, Check::Foreign);
    // a drip that would take 10 s, against a 0.05 s probe
    let mut drip = b"HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\n".to_vec();
    drip.extend(std::iter::repeat_n(b' ', 1000));
    let port = serve(drip, 1, Duration::from_millis(10))?;
    let t0 = Instant::now();
    let got = probe(&format!("http://127.0.0.1:{port}/"), 0.05);
    assert_eq!(got, Check::Foreign);
    assert!(t0.elapsed() < Duration::from_secs(1));
    Ok(())
}

// ---- open -------------------------------------------------------------------------------------------------------

#[test]
fn test_open_when_already_running_starts_nothing() -> R {
    let d = tempfile::tempdir()?;
    let f = Fake::with_checks(&[Check::Hub]);
    assert_eq!(open_hub_with(&[], Some(settings("127.0.0.1", 8792, d.path())?), &f), 0);
    let r = f.r();
    assert_eq!(
        (r.started, r.opened.clone(), r.notes.len()),
        (0, vec![URL.to_string()], 0)
    );
    Ok(())
}

#[test]
fn test_open_starts_the_hub_then_waits_for_health() -> R {
    let d = tempfile::tempdir()?;
    let f = Fake::with_checks(&[Check::Down, Check::Down, Check::Hub]);
    assert_eq!(open_hub_with(&[], Some(settings("127.0.0.1", 8792, d.path())?), &f), 0);
    let r = f.r();
    assert_eq!((r.started, r.opened.clone()), (1, vec![URL.to_string()]));
    Ok(())
}

#[test]
fn test_open_refuses_a_port_it_cannot_trust() -> R {
    for (state, note) in [
        (Check::Foreign, "something other than Agent Hub"),
        (Check::OtherUser, "another user's process"),
    ] {
        let d = tempfile::tempdir()?;
        let f = Fake::with_checks(&[state]);
        assert_eq!(open_hub_with(&[], Some(settings("127.0.0.1", 8792, d.path())?), &f), 1);
        let r = f.r();
        assert_eq!((r.started, r.opened.len()), (0, 0));
        assert!(r.notes[0].contains(note), "{}", r.notes[0]);
    }
    Ok(())
}

#[test]
fn test_open_refuses_a_squatter_that_wins_the_port_during_start() -> R {
    let d = tempfile::tempdir()?;
    let mut f = Fake::with_checks(&[Check::Down, Check::OtherUser]); // our child lost the bind; the squatter answers
    f.exit_code = Some(1);
    assert_eq!(open_hub_with(&[], Some(settings("127.0.0.1", 8792, d.path())?), &f), 1);
    let r = f.r();
    assert!(r.opened.is_empty() && r.notes[0].contains("another user's process"));
    Ok(())
}

#[test]
fn test_open_reports_a_hub_that_exits_during_start() -> R {
    let d = tempfile::tempdir()?;
    let mut f = Fake::with_checks(&[Check::Down]);
    f.exit_code = Some(1);
    assert_eq!(open_hub_with(&[], Some(settings("127.0.0.1", 8792, d.path())?), &f), 1);
    let r = f.r();
    assert!(r.opened.is_empty() && r.notes[0].contains("hub.log"), "{}", r.notes[0]);
    Ok(())
}

#[test]
fn test_open_stops_its_child_after_the_start_timeout() -> R {
    let d = tempfile::tempdir()?;
    let mut f = Fake::with_checks(&[Check::Down]);
    f.start_timeout_s = Some(0.0);
    assert_eq!(open_hub_with(&[], Some(settings("127.0.0.1", 8792, d.path())?), &f), 1);
    assert!(f.r().opened.is_empty() && f.terminated.load(Ordering::SeqCst));
    Ok(())
}

#[test]
fn test_open_refuses_a_non_loopback_host_before_probing() -> R {
    let d = tempfile::tempdir()?;
    let f = Fake::default(); // no checks: probing would panic
    assert_eq!(
        open_hub_with(&[], Some(settings("evil.example", 8792, d.path())?), &f),
        1
    );
    let r = f.r();
    assert!(r.started == 0 && r.notes[0].contains("not a loopback address"));
    Ok(())
}

#[test]
fn test_open_does_not_record_a_child_that_lost_the_port_to_another_hub() -> R {
    let d = tempfile::tempdir()?;
    let mut f = Fake::with_checks(&[Check::Down, Check::Hub]); // a hub of ours answers, but our child has exited
    f.exit_code = Some(1);
    assert_eq!(open_hub_with(&[], Some(settings("127.0.0.1", 8792, d.path())?), &f), 0);
    assert_eq!(f.r().opened, vec![URL.to_string()]);
    assert!(!d.path().join("hub.pid").exists());
    Ok(())
}

#[test]
fn test_concurrent_launches_wait_for_each_other() -> R {
    let d = tempfile::tempdir()?;
    let cfg = settings("127.0.0.1", 8792, d.path())?;
    let f = Arc::new(Fake::with_checks(&[Check::Hub]));
    let lockf = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(d.path().join("hub.lock"))?;
    // another launch is mid-start
    let held = nix::fcntl::Flock::lock(lockf, nix::fcntl::FlockArg::LockExclusive).map_err(|(_, e)| e)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let f2 = f.clone();
    std::thread::spawn(move || {
        let _ = tx.send(open_hub_with(&[], Some(cfg), f2.as_ref()));
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(f.r().opened.is_empty()); // blocked on the lock, has not even probed
    drop(held);
    assert_eq!(rx.recv_timeout(Duration::from_secs(5))?, 0);
    let r = f.r();
    assert_eq!((r.opened.clone(), r.started), (vec![URL.to_string()], 0));
    Ok(())
}

#[test]
fn test_open_never_starts_a_wildcard_bound_hub() -> R {
    for host in ["0.0.0.0", "::"] {
        let d = tempfile::tempdir()?;
        let f = Fake::with_checks(&[Check::Down]);
        assert_eq!(open_hub_with(&[], Some(settings(host, 8792, d.path())?), &f), 1);
        let r = f.r();
        assert!(r.started == 0 && r.notes[0].contains("binds every interface"), "{host}");
    }
    Ok(())
}

#[test]
fn test_open_and_stop_reject_arguments() -> R {
    let d = tempfile::tempdir()?;
    let f = Fake::with_checks(&[Check::Down]);
    let cfg = settings("127.0.0.1", 8792, d.path())?;
    assert_eq!(open_hub_with(&["--port".into(), "1".into()], Some(cfg.clone()), &f), 2);
    assert_eq!(stop_hub_with(&["now".into()], Some(cfg), &f), 2);
    assert_eq!(f.r().started, 0);
    Ok(())
}

// ---- background start ------------------------------------------------------------------------------------------

#[test]
fn test_start_background_is_isolated_detached_and_private() -> R {
    let d = tempfile::tempdir()?;
    let state = d.path().join("state");
    let cfg = settings("127.0.0.1", 8792, &state)?;
    // the hub itself is this binary (D-LAUNCH); a harmless command stands in for it here
    assert_eq!(
        agent_hub::desktop::System.server_argv(),
        agent_hub::desktop::server_argv()
    );
    let mut proc = start_background_argv(&cfg, &["sleep".into(), "30".into()], LOG_ROTATE_BYTES)?;
    let pid = proc.pid();
    let pgid = nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(i32::try_from(pid)?)))?;
    proc.stop();
    assert_eq!(pgid.as_raw(), i32::try_from(pid)?); // its own process group (D-LAUNCH: not its own session)
    assert_eq!(mode(&state), 0o700);
    for name in ["hub.log", "hub.pid"] {
        assert_eq!(mode(&state.join(name)), 0o600, "{name}");
    }
    assert_eq!(std::fs::read_to_string(state.join("hub.pid"))?, format!("{pid}\n"));
    Ok(())
}

fn refuses_a_symlinked(name: &str) -> R {
    let d = tempfile::tempdir()?;
    let (state, target, marker) = (
        d.path().join("state"),
        d.path().join("elsewhere"),
        d.path().join("spawned"),
    );
    std::fs::create_dir(&state)?;
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(&target, "keep\n")?;
    std::os::unix::fs::symlink(&target, state.join(name))?;
    let cfg = settings("127.0.0.1", 8792, &state)?;
    let argv: Vec<String> = vec!["touch".into(), marker.to_string_lossy().to_string()];
    let err = start_background_argv(&cfg, &argv, LOG_ROTATE_BYTES)
        .err()
        .map(|e| e.0)
        .unwrap_or_default();
    assert!(
        err.starts_with("[Errno 40] Too many levels of symbolic links: '"),
        "{err}"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(!marker.exists()); // nothing was spawned
    assert_eq!(std::fs::read_to_string(&target)?, "keep\n");
    Ok(())
}

#[test]
fn test_start_background_refuses_a_symlinked_log() -> R {
    refuses_a_symlinked("hub.log")
}

#[test]
fn test_start_background_refuses_a_symlinked_pid_file_before_spawning() -> R {
    refuses_a_symlinked("hub.pid")
}

#[test]
fn test_start_background_tightens_an_existing_log_and_rotates_a_big_one() -> R {
    let d = tempfile::tempdir()?;
    let state = d.path().join("state");
    std::fs::create_dir(&state)?;
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755))?;
    let log = state.join("hub.log");
    std::fs::write(&log, "x".repeat(20))?;
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644))?;
    let cfg = settings("127.0.0.1", 8792, &state)?;
    let mut proc = start_background_argv(&cfg, &["true".into()], 10)?;
    proc.stop();
    assert_eq!(std::fs::read_to_string(state.join("hub.log.1"))?, "x".repeat(20));
    assert_eq!((mode(&log), mode(&state)), (0o600, 0o700));
    Ok(())
}

// ---- stop ------------------------------------------------------------------------------------------------------

fn stopper(alive: Vec<(u32, Vec<String>)>) -> Result<(tempfile::TempDir, Fake, Settings), Box<dyn std::error::Error>> {
    let d = tempfile::tempdir()?;
    std::fs::write(d.path().join("hub.pid"), "4242\n")?;
    let cfg = settings("127.0.0.1", 8792, d.path())?;
    let f = Fake::default();
    f.r().alive = alive;
    Ok((d, f, cfg))
}

#[test]
fn test_stop_terminates_the_hub_it_started() -> R {
    let me_proc = (me(), vec![SERVER.to_string()]);
    // identity check, then one poll before it exits
    let (d, f, cfg) = stopper(vec![me_proc.clone(), me_proc])?;
    assert_eq!(stop_hub_with(&[], Some(cfg), &f), 0);
    assert_eq!(f.r().signals, vec![4242]);
    assert!(!d.path().join("hub.pid").exists());
    Ok(())
}

#[test]
fn test_stop_never_signals_a_process_it_did_not_start() -> R {
    let procs: [(u32, Vec<String>); 3] = [
        (0, vec![SERVER.to_string()]), // another user
        (
            me(),
            vec!["/usr/bin/vim".into(), "backend/src/agent_hub/main.py".into()],
        ), // recycled pid
        (
            me(),
            vec![
                "/usr/bin/python3".into(),
                "-I".into(),
                "-m".into(),
                "agent_hub.main".into(),
            ],
        ), // other program
    ];
    for p in procs {
        let (_d, f, cfg) = stopper(vec![p.clone()])?;
        assert_eq!(stop_hub_with(&[], Some(cfg), &f), 1);
        let r = f.r();
        assert!(r.signals.is_empty() && r.notes[0].contains("nothing stopped"), "{p:?}");
    }
    Ok(())
}

#[test]
fn test_stop_reports_a_hub_that_is_already_gone() -> R {
    let (d, f, cfg) = stopper(Vec::new())?;
    assert_eq!(stop_hub_with(&[], Some(cfg), &f), 1);
    assert!(f.r().signals.is_empty() && !d.path().join("hub.pid").exists());
    Ok(())
}

// ---- entry point -------------------------------------------------------------------------------------------------

#[test]
fn test_a_malformed_setting_is_reported_not_raised() {
    let f = Fake::default();
    let env = std::collections::HashMap::from([
        ("HOME".to_string(), "/nonexistent-home".to_string()),
        (
            "AGENT_HUB_ENV_FILE".to_string(),
            "/nonexistent/agent-hub-env".to_string(),
        ),
        ("HUB_PORT".to_string(), "value-that-must-not-be-echoed".to_string()),
    ]);
    let load = || load_settings_from(&env, &vec![]);
    assert_eq!(run_with("open", &[], None, &load, &f), 1);
    let r = f.r();
    assert_eq!(r.notes.len(), 1);
    assert!(
        r.notes[0].contains("port") && !r.notes[0].contains("value-that-must-not-be-echoed"),
        "{}",
        r.notes[0]
    );
}

fn agent_hub(args: &[&str], extra: &[(&str, &str)]) -> Result<(i32, String), Box<dyn std::error::Error>> {
    let d = tempfile::tempdir()?;
    let empty = d.path().join("bin");
    std::fs::create_dir(&empty)?;
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_agent-hub"));
    c.args(args)
        .env_clear()
        .env("PATH", &empty) // no notify-send, no xdg-open
        .env("HOME", d.path())
        .env("HUB_DATA_DIR", d.path().join("data"))
        .env("AGENT_HUB_ENV_FILE", d.path().join("no-env"));
    for (k, v) in extra {
        c.env(k, v);
    }
    let out = c.output()?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

#[test]
fn test_agent_hub_dispatches_open_and_stop_to_the_launcher() -> R {
    // `stop` reaches the launcher (nothing recorded), never the server
    let (code, err) = agent_hub(&["stop"], &[])?;
    assert_eq!(code, 1);
    assert!(
        err.contains("agent-hub: no hub started by `agent-hub open` is recorded"),
        "{err}"
    );
    // `open` reaches the launcher: it refuses an off-box host before anything (the server would say HUB_ALLOW_NONLOOPBACK)
    let (code, err) = agent_hub(&["open"], &[("HUB_HOST", "evil.example")])?;
    assert_eq!(code, 1);
    assert!(
        err.contains("not a loopback address") && !err.contains("HUB_ALLOW_NONLOOPBACK"),
        "{err}"
    );
    let (code, err) = agent_hub(&["open", "extra"], &[])?;
    assert_eq!((code, err.trim()), (2, "usage: agent-hub open"));
    Ok(())
}
