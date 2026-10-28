//! desktop.py: `agent-hub open` / `agent-hub stop` (the desktop launcher, Linux). `open` starts the hub in the background
//! if nothing serves yet, waits for /api/v1/health, then opens the UI with `xdg-open`, only on a loopback address and
//! only when every socket listening on the port belongs to this user and the answer looks like Agent Hub. Launches are
//! serialised by `<data>/hub.lock` (flock); `stop` stops the hub `open` started (`<data>/hub.pid`), never a stranger.
//! Failures also go to `notify-send`. Owner: CORE (LOCKED).
//!
//! Everything the launcher does to the outside world (probe the port, start the hub, open the browser, notify, read
//! `/proc`, signal) goes through the [`Launcher`] trait; [`System`] is the real one and the tests substitute their own
//! (the Python tests monkeypatch the same functions).
//!
//! DEVIATION D-LAUNCH: the background server argv is `[<agent-hub binary>]` (Python: `[sys.executable, "-I", "-m",
//! "agent_hub.main"]`), so a hub started by the Python launcher is not stopped by this one (it refuses, as for any
//! foreign pid); and the hub gets its own process group instead of its own session (Python `start_new_session=True`;
//! `setsid()` in the child needs `pre_exec`, i.e. unsafe code, which this workspace forbids). With stdin on /dev/null and
//! the output in hub.log, the hub survives the terminal that ran `agent-hub open` either way.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use hub_core::py::{os::error_str, text};

use crate::config::{Settings, SettingsError, load_settings};

pub const START_TIMEOUT_S: f64 = 30.0;
pub const STOP_TIMEOUT_S: f64 = 10.0;
pub const POLL_S: f64 = 0.5;
/// The whole /health exchange, not one socket operation.
pub const PROBE_TIMEOUT_S: f64 = 2.0;
pub const PROBE_MAX_BYTES: usize = 64 * 1024;
/// httpcore's `MAX_INCOMPLETE_EVENT_SIZE` (h11's limit as httpx sets it): a response head still incomplete with more
/// than this buffered is a protocol error (httpx: "down"). httpcore reads `PROBE_READ_BYTES` at a time.
pub const PROBE_MAX_HEAD_BYTES: usize = 100 * 1024;
pub const PROBE_READ_BYTES: usize = 64 * 1024;
pub const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;
pub const HEALTH_KEYS: [&str; 5] = ["status", "version", "content_initialised", "adapters", "trust_loopback"];
pub const PROC_NET_TABLES: [&str; 2] = ["/proc/net/tcp", "/proc/net/tcp6"];
pub const LOG_NAME: &str = "hub.log";
pub const PID_NAME: &str = "hub.pid";
pub const LOCK_NAME: &str = "hub.lock";
pub const WILDCARD_HOSTS: [&str; 4] = ["", "0.0.0.0", "::", "[::]"];

/// A refusal the launcher reports (notification + exit status 1). Python also reports `OSError` and `ValueError`
/// text this way; here those are LauncherErrors carrying Python's `str(exc)`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct LauncherError(pub String);

impl LauncherError {
    fn os(e: &std::io::Error, path: Option<&Path>) -> Self {
        Self(error_str(e, path.map(|p| p.to_string_lossy()).as_deref()))
    }
}

/// What answers on the hub's port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// an Agent Hub owned by this user
    Hub,
    /// something answers like the hub, but not every listener on the port is ours
    OtherUser,
    /// something other than Agent Hub
    Foreign,
    /// nothing
    Down,
}

impl Check {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hub => "hub",
            Self::OtherUser => "other-user",
            Self::Foreign => "foreign",
            Self::Down => "down",
        }
    }
}

fn uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

/// `ipaddress.ip_address(s)` + `.is_loopback`, as Python 3.13 decides it: the canonical text of a loopback address
/// (`str(ip)`, IPv6 with its `%scope`) and whether it is IPv6, or None for anything that is not one.
fn loopback_ip(s: &str) -> Option<(String, bool)> {
    if let Ok(v4) = s.parse::<std::net::Ipv4Addr>() {
        return v4.is_loopback().then(|| (v4.to_string(), false));
    }
    let (addr, scope) = match s.split_once('%') {
        Some((a, sc)) if !sc.is_empty() && !sc.contains('%') => (a, Some(sc)),
        Some(_) => return None,
        None => (s, None),
    };
    let v6: std::net::Ipv6Addr = addr.parse().ok()?;
    let loop6 = v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|m| m.is_loopback());
    loop6.then(|| (scope.map_or_else(|| v6.to_string(), |sc| format!("{v6}%{sc}")), true))
}

/// `hub_url(cfg)`: the hub's URL, loopback only: neither the probe nor the browser may be sent off the box.
pub fn hub_url(cfg: &Settings) -> Result<String, LauncherError> {
    let mut host = text::strip(&cfg.host).to_string();
    if host.is_empty() || host == "0.0.0.0" {
        host = "127.0.0.1".into();
    } else if host == "::" || host == "[::]" {
        host = "::1".into();
    }
    if host != "localhost" {
        let bare = host.strip_prefix('[').unwrap_or(&host);
        let bare = bare.strip_suffix(']').unwrap_or(bare);
        match loopback_ip(bare) {
            Some((ip, true)) => host = format!("[{ip}]"),
            Some((ip, false)) => host = ip,
            None => {
                return Err(LauncherError(format!(
                    "host {} is not a loopback address; the launcher only opens a local hub",
                    text::repr(&cfg.host)
                )));
            }
        }
    }
    if !(1..=65535).contains(&cfg.port) {
        return Err(LauncherError(format!("port {} is out of range", cfg.port)));
    }
    Ok(format!("http://{host}:{}/", cfg.port))
}

/// `int(s, 16)`: Python's rules (sign, optional `0x` then an optional `_`, `_` between digits, any script's digits).
fn int_hex(s: &str) -> Option<i128> {
    let t = text::strip(s);
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let t = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(r) => r.strip_prefix('_').unwrap_or(r),
        None => t,
    };
    if t.is_empty() || t.starts_with('_') || t.ends_with('_') || t.contains("__") {
        return None;
    }
    let mut v: i128 = 0;
    for c in t.chars() {
        if c == '_' {
            continue;
        }
        let d = match text::decimal(c) {
            Some(d) => d,
            None if c.is_ascii_hexdigit() => c.to_digit(16)?,
            None => return None,
        };
        v = v.saturating_mul(16).saturating_add(i128::from(d));
    }
    Some(if neg { -v } else { v })
}

fn int_error(s: &str, base: u32) -> LauncherError {
    LauncherError(format!("invalid literal for int() with base {base}: {}", text::repr(s)))
}

/// `listen_uids(port, tables)`: owners of every LISTEN socket on `port`, any address, IPv4 and IPv6. `Ok(None)` if a
/// table is unreadable (the caller fails closed); `Err` for text Python cannot parse (its ValueError).
pub fn listen_uids(port: i64, tables: &[&str]) -> Result<Option<BTreeSet<i128>>, LauncherError> {
    let mut uids = BTreeSet::new();
    for t in tables {
        let raw = match std::fs::read(t) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue, // no IPv6
            Err(_) => return Ok(None),
        };
        if let Some(pos) = raw.iter().position(|b| !b.is_ascii()) {
            return Err(LauncherError(format!(
                "'ascii' codec can't decode byte 0x{:02x} in position {pos}: ordinal not in range(128)",
                raw[pos]
            )));
        }
        let s = String::from_utf8_lossy(&raw);
        for row in text::splitlines(&s, false).into_iter().skip(1) {
            let f = text::split_ws(row);
            if f.len() > 7 && f[3] == "0A" {
                let Some((_, hexport)) = f[1].rsplit_once(':') else {
                    return Err(LauncherError("list index out of range".into()));
                };
                let p = int_hex(hexport).ok_or_else(|| int_error(hexport, 16))?;
                if p == i128::from(port) {
                    uids.insert(text::int_str(f[7]).ok_or_else(|| int_error(f[7], 10))?);
                }
            }
        }
    }
    Ok(Some(uids))
}

/// `held_by_me(port)` over the given listener sets: every listener (and at least one) is this user's.
fn held_by_me(uids: Option<&BTreeSet<i128>>) -> bool {
    uids.is_some_and(|u| !u.is_empty() && u.iter().all(|x| *x == i128::from(uid())))
}

/// `check(url, port)` after the probe: a hub-shaped answer counts only when every listener is ours.
pub fn classify(state: Check, uids: Option<&BTreeSet<i128>>) -> Check {
    if state == Check::Hub && !held_by_me(uids) {
        return Check::OtherUser;
    }
    state
}

type Head = (u16, Vec<(String, String)>);

/// h11's status line and header syntax, loosely: `HTTP/1.x NNN reason`, `name: value` lines.
fn parse_head(head: &str) -> Option<Head> {
    let mut lines = head.split("\r\n");
    let status_line = lines.next()?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next()?;
    if !(version == "HTTP/1.1" || version == "HTTP/1.0") {
        return None;
    }
    let code = parts.next()?;
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let status: u16 = code.parse().ok()?;
    let mut headers = Vec::new();
    for l in lines {
        let (k, v) = l.split_once(':')?;
        if k.is_empty() || k.bytes().any(|b| b.is_ascii_whitespace() || b.is_ascii_control()) {
            return None;
        }
        headers.push((k.to_string(), v.to_string()));
    }
    Some((status, headers))
}

#[derive(Default)]
struct ChunkState {
    /// bytes still to read of the current chunk's data plus its CRLF; None = expecting a size line
    left: Option<usize>,
    done: bool,
}

/// Decode as much chunked data as `rest` holds; Ok(true) once the last chunk has been read, Err on bad framing.
fn decode_chunks(rest: &mut Vec<u8>, st: &mut ChunkState, body: &mut Vec<u8>) -> Result<bool, ()> {
    loop {
        if st.done {
            return Ok(true);
        }
        match st.left {
            None => {
                let Some(i) = rest.windows(2).position(|w| w == b"\r\n") else {
                    return if rest.len() > PROBE_MAX_HEAD_BYTES {
                        Err(())
                    } else {
                        Ok(false)
                    };
                };
                let line = String::from_utf8_lossy(&rest[..i]).into_owned();
                rest.drain(..i + 2);
                let size_txt = line.split(';').next().unwrap_or("").trim();
                let size = usize::from_str_radix(size_txt, 16).map_err(|_| ())?;
                if size == 0 {
                    st.done = true; // trailers are not read: the body is complete
                    return Ok(true);
                }
                st.left = Some(size + 2);
            }
            Some(n) => {
                if rest.is_empty() {
                    return Ok(false);
                }
                let take = n.min(rest.len());
                let data_take = take.min(n.saturating_sub(2));
                body.extend_from_slice(&rest[..data_take]);
                rest.drain(..take);
                st.left = if take == n { None } else { Some(n - take) };
            }
        }
    }
}

/// How reading the response head ended.
enum HeadRead {
    Head(u16, Vec<(String, String)>, Vec<u8>),
    /// a protocol error, an oversized head, a read timeout or a closed connection (httpx: "down")
    Down,
    /// the probe's total deadline passed while the head was still arriving (see PORTING.md P-1)
    Late,
}

/// The response head: (status, headers, bytes of the body already read), skipping informational 1xx answers other than
/// 101 like httpcore. Reads `PROBE_READ_BYTES` at a time; an incomplete head with over `PROBE_MAX_HEAD_BYTES` buffered
/// is an error, as in h11.
fn read_head(s: &mut TcpStream, deadline: Instant) -> HeadRead {
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; PROBE_READ_BYTES];
    loop {
        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..i]).into_owned();
            let rest = raw[i + 4..].to_vec();
            let Some((status, headers)) = parse_head(&head) else {
                return HeadRead::Down;
            };
            if (100..200).contains(&status) && status != 101 {
                raw = rest;
                continue;
            }
            return HeadRead::Head(status, headers, rest);
        }
        if raw.len() > PROBE_MAX_HEAD_BYTES {
            return HeadRead::Down;
        }
        if Instant::now() > deadline {
            return HeadRead::Late;
        }
        match s.read(&mut buf) {
            Ok(0) | Err(_) => return HeadRead::Down,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    }
}

/// How the body ends: Ok(Some(n)) = Content-Length n, Ok(None) = at close (or chunked), Err = a framing error.
fn body_length(status: u16, headers: &[(String, String)], chunked: bool) -> Result<Option<usize>, ()> {
    if matches!(status, 101 | 204 | 304) {
        return Ok(Some(0));
    }
    if chunked {
        return Ok(None);
    }
    let cl: Vec<&str> = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.trim())
        .collect();
    match cl.as_slice() {
        [] => Ok(None),
        [first, others @ ..] => {
            if others.iter().any(|o| o != first) || first.is_empty() || !first.bytes().all(|b| b.is_ascii_digit()) {
                return Err(());
            }
            first.parse().map(Some).map_err(|_| ())
        }
    }
}

/// One HTTP/1.1 exchange on `s`, as httpx + h11 read it: the head (see `read_head`), then the body by Content-Length,
/// chunked or until close (an incomplete body or a read timeout = down), capped in size and in total time (= foreign).
/// Python checks its total deadline only between body chunks; here it also covers the head (PORTING.md P-1).
fn read_response(s: &mut TcpStream, deadline: Instant) -> Check {
    let (status, headers, mut rest) = match read_head(s, deadline) {
        HeadRead::Head(st, h, rest) => (st, h, rest),
        HeadRead::Down => return Check::Down,
        HeadRead::Late => return Check::Foreign,
    };
    let te: Vec<&str> = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"))
        .map(|(_, v)| v.trim())
        .collect();
    let chunked = match te.as_slice() {
        [] => false,
        [one] if one.eq_ignore_ascii_case("chunked") => true,
        _ => return Check::Down, // h11 supports only `chunked`
    };
    let Ok(length) = body_length(status, &headers, chunked) else {
        return Check::Down;
    };
    let mut body: Vec<u8> = Vec::new();
    let mut chunk_state = ChunkState::default();
    let mut buf = vec![0u8; PROBE_READ_BYTES];
    loop {
        if chunked {
            match decode_chunks(&mut rest, &mut chunk_state, &mut body) {
                Err(()) => return Check::Down,
                Ok(true) => break,
                Ok(false) => {}
            }
        } else {
            body.append(&mut rest);
        }
        if body.len() > PROBE_MAX_BYTES || Instant::now() > deadline {
            return Check::Foreign;
        }
        if let Some(n) = length
            && body.len() >= n
        {
            body.truncate(n);
            break;
        }
        match s.read(&mut buf) {
            Ok(0) if chunked || length.is_some() => return Check::Down, // incomplete message body
            Ok(0) => break,
            Ok(n) => rest.extend_from_slice(&buf[..n]),
            Err(_) => return Check::Down,
        }
    }
    let body = body.strip_prefix(b"\xef\xbb\xbf".as_slice()).unwrap_or(&body);
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(serde_json::Value::Object(m)) if HEALTH_KEYS.iter().all(|k| m.contains_key(*k)) => Check::Hub,
        _ => Check::Foreign,
    }
}

/// `probe(url)`: hub if the answer to /health has the hub's shape, foreign for any other answer, down for none. The
/// body is capped in size and the exchange in total time, so a listener that drips bytes cannot hold the launcher.
pub fn probe(url: &str, timeout_s: f64) -> Check {
    let timeout = Duration::from_secs_f64(timeout_s.max(0.001));
    let deadline = Instant::now() + timeout;
    let Some(rest) = url.strip_prefix("http://") else {
        return Check::Down;
    };
    let authority = rest.trim_end_matches('/');
    let Ok(addrs) = authority.to_socket_addrs() else {
        return Check::Down;
    };
    let Some(mut s) = addrs
        .into_iter()
        .find_map(|a| TcpStream::connect_timeout(&a, timeout).ok())
    else {
        return Check::Down;
    };
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    let req = format!(
        "GET /api/v1/health HTTP/1.1\r\nHost: {authority}\r\nAccept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
    );
    if s.write_all(req.as_bytes()).is_err() {
        return Check::Down;
    }
    read_response(&mut s, deadline)
}

/// `check(url, port)`: hub | other-user | foreign | down.
pub fn check(url: &str, port: i64) -> Result<Check, LauncherError> {
    let state = probe(url, PROBE_TIMEOUT_S);
    if state != Check::Hub {
        return Ok(state);
    }
    Ok(classify(state, listen_uids(port, &PROC_NET_TABLES)?.as_ref()))
}

fn private_data_dir(d: &Path) -> Result<(), LauncherError> {
    // Path.mkdir(mode=0o700, parents=True, exist_ok=True): parents get the default mode, the directory itself 0700
    if let Some(parent) = d.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|e| LauncherError::os(&e, Some(parent)))?;
    }
    if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(d)
        && !(e.kind() == std::io::ErrorKind::AlreadyExists && d.is_dir())
    {
        return Err(LauncherError::os(&e, Some(d)));
    }
    let st = std::fs::symlink_metadata(d).map_err(|e| LauncherError::os(&e, Some(d)))?;
    if !st.is_dir() || st.uid() != uid() {
        return Err(LauncherError(format!(
            "data dir {} is not a directory you own",
            d.display()
        )));
    }
    std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).map_err(|e| LauncherError::os(&e, Some(d)))
}

#[derive(Clone, Copy)]
enum Access {
    Append,
    Write,
    ReadWrite,
}

/// `_open_private(path, flags)`: a regular file we own, never through a symlink or a FIFO, forced to mode 0600.
fn open_private(path: &Path, access: Access) -> Result<File, LauncherError> {
    let mut o = OpenOptions::new();
    match access {
        Access::Append => o.append(true),
        Access::Write => o.write(true),
        Access::ReadWrite => o.read(true).write(true),
    };
    let f = o
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| LauncherError::os(&e, Some(path)))?;
    let st = f.metadata().map_err(|e| LauncherError::os(&e, None))?;
    if !st.is_file() || st.uid() != uid() {
        return Err(LauncherError(format!(
            "{} is not a regular file you own",
            path.display()
        )));
    }
    f.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| LauncherError::os(&e, None))?;
    Ok(f)
}

fn rotate(log: &Path, rotate_bytes: u64) -> Result<(), LauncherError> {
    let st = match std::fs::symlink_metadata(log) {
        Ok(st) => st,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(LauncherError::os(&e, Some(log))),
    };
    if st.is_file() && st.len() > rotate_bytes {
        let to = log.with_file_name(format!("{LOG_NAME}.1"));
        std::fs::rename(log, &to).map_err(|e| LauncherError::os(&e, Some(log)))?;
    }
    Ok(())
}

/// The command `open` starts: this binary, serving (D-LAUNCH).
pub fn server_argv() -> Vec<String> {
    vec![
        std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "agent-hub".into()),
    ]
}

/// A started hub process, as the launcher uses `subprocess.Popen`.
pub trait HubProcess: Send {
    fn pid(&self) -> u32;
    /// `proc.poll()`: the exit status once the process has exited.
    fn poll(&mut self) -> Option<i32>;
    /// `_stop_child(proc)`: SIGTERM, up to 5 s to exit, then SIGKILL.
    fn stop(&mut self);
}

/// The real child process.
pub struct RealProcess(pub Child);

impl HubProcess for RealProcess {
    fn pid(&self) -> u32 {
        self.0.id()
    }
    fn poll(&mut self) -> Option<i32> {
        match self.0.try_wait() {
            Ok(Some(st)) => Some(st.code().unwrap_or(-1)),
            _ => None,
        }
    }
    fn stop(&mut self) {
        if self.poll().is_some() {
            return;
        }
        let pid = nix::unistd::Pid::from_raw(i32::try_from(self.0.id()).unwrap_or(0));
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `start_background(cfg)` with an explicit argv and rotation size: both files are opened (and refused through a
/// symlink) BEFORE the spawn, so a refused file never leaves an unrecorded hub running.
pub fn start_background_argv(cfg: &Settings, argv: &[String], rotate_bytes: u64) -> Result<RealProcess, LauncherError> {
    private_data_dir(&cfg.data_dir)?;
    let log = cfg.data_dir.join(LOG_NAME);
    rotate(&log, rotate_bytes)?;
    let logf = open_private(&log, Access::Append)?;
    let mut pidf = open_private(&cfg.data_dir.join(PID_NAME), Access::Write)?;
    let Some(prog) = argv.first() else {
        return Err(LauncherError("empty server command".into()));
    };
    let out = logf.try_clone().map_err(|e| LauncherError::os(&e, None))?;
    let child = Command::new(prog)
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(logf)
        .process_group(0)
        .spawn()
        .map_err(|e| LauncherError::os(&e, Some(Path::new(prog))))?;
    pidf.set_len(0).map_err(|e| LauncherError::os(&e, None))?;
    pidf.write_all(format!("{}\n", child.id()).as_bytes())
        .map_err(|e| LauncherError::os(&e, None))?;
    Ok(RealProcess(child))
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var("PATH")
        .ok()?
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(name))
        .find(|p| {
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// `open_browser(url)`: `xdg-open <url>`, detached; false when there is no xdg-open.
pub fn open_browser(url: &str) -> bool {
    let Some(opener) = which("xdg-open") else { return false };
    let _ = Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    true
}

/// `notify(message)`: stderr, and notify-send when present (the body may be parsed as markup, so it is escaped).
pub fn notify(message: &str) {
    #[allow(clippy::print_stderr)]
    {
        eprintln!("agent-hub: {message}");
    }
    if let Some(tool) = which("notify-send") {
        let esc = message.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        if let Ok(mut c) = Command::new(tool)
            .args(["--", "Agent Hub", &esc])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if matches!(c.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// `_pid_cmdline(pid)`: (owner uid, argv) of a live process, or None if it is gone.
pub fn pid_cmdline(pid: i128) -> Option<(u32, Vec<String>)> {
    let uid = std::fs::metadata(format!("/proc/{pid}")).ok()?.uid();
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some((
        uid,
        raw.split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect(),
    ))
}

/// Everything `open`/`stop` do outside their own logic. [`System`] is the real implementation.
pub trait Launcher: Send + Sync {
    fn check(&self, url: &str, port: i64) -> Result<Check, LauncherError>;
    fn start_background(&self, cfg: &Settings) -> Result<Box<dyn HubProcess>, LauncherError>;
    fn open_browser(&self, url: &str) -> bool;
    fn notify(&self, message: &str);
    fn pid_cmdline(&self, pid: i128) -> Option<(u32, Vec<String>)>;
    /// `os.kill(pid, SIGTERM)`.
    fn terminate(&self, pid: i128) -> Result<(), LauncherError>;
    /// The argv `start_background` runs (the identity `stop` checks before it signals).
    fn server_argv(&self) -> Vec<String>;
    /// A line on stdout.
    fn say(&self, line: &str);
    fn poll_s(&self) -> f64 {
        POLL_S
    }
    fn start_timeout_s(&self) -> f64 {
        START_TIMEOUT_S
    }
    fn stop_timeout_s(&self) -> f64 {
        STOP_TIMEOUT_S
    }
}

/// The real launcher.
pub struct System;

impl Launcher for System {
    fn check(&self, url: &str, port: i64) -> Result<Check, LauncherError> {
        check(url, port)
    }
    fn start_background(&self, cfg: &Settings) -> Result<Box<dyn HubProcess>, LauncherError> {
        Ok(Box::new(start_background_argv(cfg, &server_argv(), LOG_ROTATE_BYTES)?))
    }
    fn open_browser(&self, url: &str) -> bool {
        open_browser(url)
    }
    fn notify(&self, message: &str) {
        notify(message);
    }
    fn pid_cmdline(&self, pid: i128) -> Option<(u32, Vec<String>)> {
        pid_cmdline(pid)
    }
    fn terminate(&self, pid: i128) -> Result<(), LauncherError> {
        let p = i32::try_from(pid).map_err(|_| LauncherError("signed integer is greater than maximum".into()))?;
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), nix::sys::signal::Signal::SIGTERM)
            .map_err(|e| LauncherError::os(&std::io::Error::from_raw_os_error(e as i32), None))
    }
    fn server_argv(&self) -> Vec<String> {
        server_argv()
    }
    #[allow(clippy::print_stdout)]
    fn say(&self, line: &str) {
        println!("{line}");
    }
}

fn other_user(url: &str) -> LauncherError {
    LauncherError(format!(
        "another user's process is listening on the hub's port ({url}); not opening it"
    ))
}

fn launch_lock(d: &Path) -> Result<nix::fcntl::Flock<File>, LauncherError> {
    let f = open_private(&d.join(LOCK_NAME), Access::ReadWrite)?;
    nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, e)| LauncherError::os(&std::io::Error::from_raw_os_error(e as i32), None))
}

fn do_open(cfg: &Settings, l: &dyn Launcher) -> Result<(), LauncherError> {
    let url = hub_url(cfg)?;
    private_data_dir(&cfg.data_dir)?;
    {
        let _lock = launch_lock(&cfg.data_dir)?;
        match l.check(&url, cfg.port)? {
            Check::OtherUser => return Err(other_user(&url)),
            Check::Foreign => {
                return Err(LauncherError(format!(
                    "something other than Agent Hub answers on {url}; not opening it"
                )));
            }
            Check::Down => {
                if WILDCARD_HOSTS.contains(&text::strip(&cfg.host)) {
                    return Err(LauncherError(format!(
                        "HUB_HOST {} binds every interface; the launcher only starts a loopback-bound hub",
                        text::repr(&cfg.host)
                    )));
                }
                start_and_wait(cfg, &url, l)?;
            }
            Check::Hub => {}
        }
    }
    if !l.open_browser(&url) {
        return Err(LauncherError(format!("xdg-open not found; open {url} in a browser")));
    }
    Ok(())
}

fn unlink_quiet(p: &Path) -> Result<(), LauncherError> {
    match std::fs::remove_file(p) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(LauncherError::os(&e, Some(p))),
        _ => Ok(()),
    }
}

fn start_and_wait(cfg: &Settings, url: &str, l: &dyn Launcher) -> Result<(), LauncherError> {
    let mut proc = l.start_background(cfg)?;
    let log = cfg.data_dir.join(LOG_NAME);
    let deadline = Instant::now() + Duration::from_secs_f64(l.start_timeout_s().max(0.0));
    loop {
        // check first: another hub may have won the port
        let state = l.check(url, cfg.port)?;
        if state == Check::Hub {
            break;
        }
        if state == Check::OtherUser {
            proc.stop();
            return Err(other_user(url));
        }
        if proc.poll().is_some() {
            return Err(LauncherError(format!("the hub did not start; see {}", log.display())));
        }
        if Instant::now() > deadline {
            proc.stop();
            return Err(LauncherError(format!(
                "the hub did not become healthy in {:.0} s; see {}",
                l.start_timeout_s(),
                log.display()
            )));
        }
        std::thread::sleep(Duration::from_secs_f64(l.poll_s().max(0.0)));
    }
    if proc.poll().is_some() {
        // a hub of ours answers, but our child lost the port to it: do not record a dead pid
        unlink_quiet(&cfg.data_dir.join(PID_NAME))?;
        l.say(&format!(
            "Agent Hub was started by something else; log of the failed start: {}",
            log.display()
        ));
        return Ok(());
    }
    l.say(&format!(
        "started Agent Hub in the background (pid {}); log: {}",
        proc.pid(),
        log.display()
    ));
    Ok(())
}

fn do_stop(cfg: &Settings, l: &dyn Launcher) -> Result<(), LauncherError> {
    let pid_file = cfg.data_dir.join(PID_NAME);
    let mut f = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(&pid_file)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(LauncherError("no hub started by `agent-hub open` is recorded".into()));
        }
        Err(e) => return Err(LauncherError::os(&e, Some(&pid_file))),
    };
    let mut buf = [0u8; 64];
    let n = f.read(&mut buf).map_err(|e| LauncherError::os(&e, None))?;
    drop(f);
    let decoded = String::from_utf8_lossy(&buf[..n]);
    let txt = text::strip(&decoded);
    // str.isdigit(): every character a digit (decimal digits of any script; see PORTING.md D-PIDFILE)
    if txt.is_empty() || !txt.chars().all(|c| text::decimal(c).is_some()) {
        return Err(LauncherError(format!("{} does not hold a pid", pid_file.display())));
    }
    let pid = text::int_str(txt).ok_or_else(|| int_error(txt, 10))?;
    let Some((owner, argv)) = l.pid_cmdline(pid) else {
        unlink_quiet(&pid_file)?;
        return Err(LauncherError(
            "the hub started by `agent-hub open` is no longer running".into(),
        ));
    };
    if owner != uid() || argv != l.server_argv() {
        // a recycled pid: never signal a stranger
        unlink_quiet(&pid_file)?;
        return Err(LauncherError(format!(
            "pid {pid} is not a hub started by `agent-hub open`; nothing stopped"
        )));
    }
    l.terminate(pid)?;
    let deadline = Instant::now() + Duration::from_secs_f64(l.stop_timeout_s().max(0.0));
    while l.pid_cmdline(pid).is_some() {
        if Instant::now() > deadline {
            return Err(LauncherError(format!(
                "pid {pid} did not exit within {:.0} s",
                l.stop_timeout_s()
            )));
        }
        std::thread::sleep(Duration::from_secs_f64(l.poll_s().max(0.0)));
    }
    unlink_quiet(&pid_file)?;
    l.say(&format!("stopped Agent Hub (pid {pid})"));
    Ok(())
}

/// `_run(command, argv, cfg)` with the settings loader and the launcher given: usage errors exit 2, every refusal is
/// notified and exits 1. A setting that does not validate is reported by name and reason, never by its value.
pub fn run_with(
    command: &str,
    argv: &[String],
    cfg: Option<Settings>,
    load: &dyn Fn() -> Result<Settings, SettingsError>,
    l: &dyn Launcher,
) -> i32 {
    if !argv.is_empty() {
        #[allow(clippy::print_stderr)]
        {
            eprintln!("usage: agent-hub {command}");
        }
        return 2;
    }
    let cfg = match cfg.map_or_else(load, Ok) {
        Ok(c) => c,
        Err(SettingsError::Invalid(e)) => {
            // names and reasons only: pydantic's text would echo the offending values
            let parts: Vec<String> = e
                .errors
                .iter()
                .map(|x| format!("{}: {}", hub_core::pyd::loc_str(&x.loc), x.msg))
                .collect();
            l.notify(&format!("invalid setting: {}", parts.join("; ")));
            return 1;
        }
        Err(e) => {
            l.notify(&e.to_string());
            return 1;
        }
    };
    let res = if command == "open" {
        do_open(&cfg, l)
    } else {
        do_stop(&cfg, l)
    };
    match res {
        Ok(()) => 0,
        Err(e) => {
            l.notify(&e.0);
            1
        }
    }
}

fn load_from_env() -> Result<Settings, SettingsError> {
    load_settings(&Vec::new())
}

/// `open_hub(argv, cfg)` with a launcher.
pub fn open_hub_with(argv: &[String], cfg: Option<Settings>, l: &dyn Launcher) -> i32 {
    run_with("open", argv, cfg, &load_from_env, l)
}

/// `stop_hub(argv, cfg)` with a launcher.
pub fn stop_hub_with(argv: &[String], cfg: Option<Settings>, l: &dyn Launcher) -> i32 {
    run_with("stop", argv, cfg, &load_from_env, l)
}

/// `agent-hub open`.
pub fn open_hub(argv: &[String]) -> i32 {
    open_hub_with(argv, None, &System)
}

/// `agent-hub stop`.
pub fn stop_hub(argv: &[String]) -> i32 {
    stop_hub_with(argv, None, &System)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_ok(host: &str, port: i64) -> Settings {
        let env = std::collections::HashMap::from([
            ("HOME".to_string(), "/tmp".to_string()),
            ("AGENT_HUB_ENV_FILE".to_string(), "/nonexistent".to_string()),
        ]);
        let mut c = match crate::config::load_settings_from(&env, &Vec::new()) {
            Ok(c) => c,
            Err(e) => unreachable_settings(&e),
        };
        c.host = host.into();
        c.port = port;
        c
    }

    #[allow(clippy::panic)]
    fn unreachable_settings(e: &SettingsError) -> Settings {
        panic!("default settings must load: {e}")
    }

    #[test]
    fn test_hub_url_is_loopback_only() {
        // the Python file's three hub_url tests plus ipaddress edge cases, each checked against Python 3.13
        let ok = [
            ("127.0.0.1", 8792, "http://127.0.0.1:8792/"),
            ("0.0.0.0", 9000, "http://127.0.0.1:9000/"),
            ("::", 8792, "http://[::1]:8792/"),
            ("::1", 8792, "http://[::1]:8792/"),
            ("localhost", 8792, "http://localhost:8792/"),
            ("::ffff:127.0.0.1", 8792, "http://[::ffff:127.0.0.1]:8792/"),
            ("[::1]", 8792, "http://[::1]:8792/"),
            ("::1%lo", 8792, "http://[::1%lo]:8792/"),
            ("0:0:0:0:0:0:0:1", 8792, "http://[::1]:8792/"),
            (" 127.0.0.2 ", 8792, "http://127.0.0.2:8792/"),
            ("[127.0.0.1]", 8792, "http://127.0.0.1:8792/"),
        ];
        for (h, p, want) in ok {
            assert_eq!(hub_url(&cfg_ok(h, p)), Ok(want.to_string()), "{h}");
        }
        for h in [
            "127.0.0.1@evil.example",
            "evil.example/#",
            "evil.example",
            "192.168.1.7",
            "10.0.0.1",
            "[[::1]]",
            "127.000.0.1",
            "0177.0.0.1",
            "127.1",
            "LOCALHOST",
            "localhost.",
            "::1%",
        ] {
            let e = hub_url(&cfg_ok(h, 8792)).err().map(|e| e.0).unwrap_or_default();
            assert!(e.contains("not a loopback address"), "{h}: {e}");
            assert!(e.starts_with(&format!("host {}", text::repr(h))));
        }
        assert_eq!(
            hub_url(&cfg_ok("127.0.0.1", 70000)),
            Err(LauncherError("port 70000 is out of range".into()))
        );
    }

    #[test]
    fn test_listen_uids_parses_proc_net_tables() -> std::io::Result<()> {
        let header =
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
        let d = tempfile::tempdir()?;
        let (tcp, tcp6) = (d.path().join("tcp"), d.path().join("tcp6"));
        std::fs::write(
            &tcp,
            format!(
                "{header}   0: 0100007F:2258 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 11\n   1: 0100007F:2258 0100007F:9C40 01 00000000:00000000 00:00000000 00000000     0        0 12\n   2: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 13\n"
            ),
        )?;
        std::fs::write(
            &tcp6,
            format!(
                "{header}   0: 00000000000000000000000000000000:2258 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000   999        0 14\n"
            ),
        )?;
        let (t4, t6) = (tcp.to_string_lossy().to_string(), tcp6.to_string_lossy().to_string());
        let missing = d.path().join("missing").to_string_lossy().to_string();
        // ESTABLISHED and other ports ignored; a wildcard IPv6 squatter counts; a missing table is skipped
        assert_eq!(listen_uids(8792, &[&t4]), Ok(Some(BTreeSet::from([1000]))));
        assert_eq!(listen_uids(8792, &[&t4, &t6]), Ok(Some(BTreeSet::from([999, 1000]))));
        assert_eq!(listen_uids(8792, &[&missing]), Ok(Some(BTreeSet::new())));
        // Python's int(x, 16) accepts a 0x prefix; a garbled row is Python's ValueError, not a silent skip
        std::fs::write(&tcp, format!("{header}   0: 0100007F:0x2258 0:0 0A 0:0 0:0 0 77 0 1\n"))?;
        assert_eq!(listen_uids(8792, &[&t4]), Ok(Some(BTreeSet::from([77]))));
        std::fs::write(&tcp, format!("{header}   0: 0100007F:zz 0:0 0A 0:0 0:0 0 77 0 1\n"))?;
        assert_eq!(
            listen_uids(8792, &[&t4]),
            Err(LauncherError("invalid literal for int() with base 16: 'zz'".into()))
        );
        Ok(())
    }

    #[test]
    fn int_hex_follows_python() {
        for s in [
            "2258",
            "0x2258",
            "0x_22_58",
            "+2258",
            " 2258 ",
            "\u{0662}\u{0662}\u{0665}\u{0668}",
        ] {
            assert_eq!(int_hex(s), Some(0x2258), "{s}");
        }
        for s in ["22__58", "zz", "", "0x", "_1", "0x__1"] {
            assert_eq!(int_hex(s), None, "{s}");
        }
    }
}
