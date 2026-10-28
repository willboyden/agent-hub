//! Port of services/fs.py: the real I/O ports. `RealFs` (adapter FileSystem port) refuses anything outside the allowed
//! roots and, for discovery, makes secret-looking files invisible; `RealRunner`/`StreamingRunner` (CommandRunner) never
//! use a shell, always have a timeout and pass a minimal environment (PATH, HOME, LC_ALL=C); `StreamingRunner` kills the
//! child's process group on the first byte over a cap. Security controls S-FS, S-RUNNER. Owner: WS-B2.
//!
//! Also home of [`capture`], the one subprocess primitive of this crate's delivery code (gitrepo uses it): argv only,
//! explicit cwd/env, optional stdin bytes, a hard timeout (the child is killed, Python `subprocess.run(timeout=)`),
//! and optional stdout and stderr caps.

use std::io::{self, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use hub_core::pathsafe::{PathError, resolve_under};
use hub_core::py::fnmatchcase;
use hub_core::py::os::{error_str, expanduser, realpath};
use hub_core::py::text::take_chars;
use hub_core::value::PyValue;
use indexmap::IndexMap;

use crate::adapters::base::{CommandRunner, FileSystem};

pub const MAX_READ: u64 = 8 * 1024 * 1024;
pub const SECRET_NAMES: [&str; 7] = [".env", ".env.*", "*.pem", "*.key", "id_rsa*", "credentials*", "*.p12"];
pub const SECRET_DIRS: [&str; 3] = [".ssh", ".aws", ".gnupg"];

// ---- subprocess primitive ------------------------------------------------------------------------------------------

/// What a finished (or killed) child produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    /// Python `returncode`: the exit code, or minus the signal number
    pub rc: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// the timeout expired and the child was killed
    pub timed_out: bool,
    /// stdout went over `out_cap` (what was read is truncated to the cap)
    pub over_cap: bool,
    /// stderr went over `err_cap` (truncated likewise)
    pub err_over_cap: bool,
}

/// Python's `returncode` for an exit status.
pub fn returncode(st: ExitStatus) -> i32 {
    st.code().or_else(|| st.signal().map(|s| -s)).unwrap_or(-1)
}

fn read_all(mut r: impl Read, cap: Option<usize>) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 65536];
    let mut over = false;
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if over {
                    continue; // keep draining so the child never blocks on a full pipe; nothing more is kept
                }
                match cap {
                    Some(c) if buf.len() + n > c => {
                        over = true;
                        let room = c.saturating_sub(buf.len());
                        buf.extend_from_slice(&chunk[..room]);
                    }
                    _ => buf.extend_from_slice(&chunk[..n]),
                }
            }
        }
    }
    (buf, over)
}

fn deadline_after(timeout: f64) -> Instant {
    let secs = if timeout.is_finite() && timeout > 0.0 {
        timeout
    } else {
        0.0
    };
    Instant::now() + Duration::from_secs_f64(secs.min(1.0e7))
}

/// Wait for `child` until `deadline`; on expiry kill it (SIGKILL to the child, as `subprocess.run`) and reap it.
fn wait_deadline(child: &mut Child, deadline: Instant) -> io::Result<(ExitStatus, bool)> {
    let mut nap = Duration::from_micros(500);
    loop {
        if let Some(st) = child.try_wait()? {
            return Ok((st, false));
        }
        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            return Ok((child.wait()?, true));
        }
        std::thread::sleep(nap.min(deadline - now));
        nap = (nap * 2).min(Duration::from_millis(20));
    }
}

/// `subprocess.run(argv, cwd=cwd, env=env, input=stdin, capture_output=True, timeout=timeout)`: argv only (never a
/// shell), the environment is EXACTLY `env`. `Err` where Python's `Popen` raised (FileNotFoundError for a missing binary
/// or cwd, ...). Without `stdin` the child inherits ours (as `subprocess.run` without `input=`). With `out_cap` /
/// `err_cap`, output beyond the cap is drained but not kept (`over_cap` / `err_over_cap` is set), so memory stays
/// bounded whatever the child prints (audit F4: Python kept everything).
pub fn capture(
    argv: &[String],
    cwd: Option<&Path>,
    env: &[(&str, String)],
    stdin: Option<Vec<u8>>,
    timeout: f64,
    out_cap: Option<usize>,
    err_cap: Option<usize>,
) -> io::Result<Captured> {
    let Some((prog, args)) = argv.split_first() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
    };
    let mut cmd = Command::new(prog);
    cmd.args(args).env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::inherit()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let writer = match (stdin, child.stdin.take()) {
        (Some(data), Some(mut pipe)) => Some(std::thread::spawn(move || {
            let _ = pipe.write_all(&data);
        })),
        _ => None,
    };
    let out_r = child
        .stdout
        .take()
        .map(|o| std::thread::spawn(move || read_all(o, out_cap)));
    let err_r = child
        .stderr
        .take()
        .map(|e| std::thread::spawn(move || read_all(e, err_cap)));
    let (st, timed_out) = wait_deadline(&mut child, deadline_after(timeout))?;
    let (stdout, over_cap) = out_r.and_then(|t| t.join().ok()).unwrap_or_default();
    let (stderr, err_over_cap) = err_r.and_then(|t| t.join().ok()).unwrap_or_default();
    if let Some(w) = writer {
        let _ = w.join();
    }
    Ok(Captured {
        rc: returncode(st),
        stdout,
        stderr,
        timed_out,
        over_cap,
        err_over_cap,
    })
}

/// `os.environ.get("PATH", "/usr/bin:/bin")`.
pub fn path_env() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
}

/// `{"PATH": ..., "HOME": ..., "LC_ALL": "C"}` (`_minimal_env`).
fn minimal_env() -> Vec<(&'static str, String)> {
    vec![
        ("PATH", path_env()),
        ("HOME", std::env::var("HOME").unwrap_or_else(|_| "/".into())),
        ("LC_ALL", "C".into()),
    ]
}

/// `bytes.decode("utf-8", "replace")[:n]` (n in code points).
fn decode_cap(b: &[u8], n: usize) -> String {
    let s = String::from_utf8_lossy(b);
    take_chars(&s, n).to_string()
}

// ---- the one reader for paths someone else can write ---------------------------------------------------------------

/// Open `path` for reading without ever blocking: O_NONBLOCK, so a FIFO (or a device) planted at the path cannot stall
/// the open, and O_NOFOLLOW unless `follow`. The type is checked with fstat on the OPENED descriptor, so a swap after
/// any earlier stat cannot slip through: a directory is EISDIR (Python's IsADirectoryError, rendered the same), any
/// other non-regular file is refused as "not a regular file". Errors from the open keep their errno, so
/// [`py_os_error`] renders them as Python would. Deviation D-B2-FIFO (PORTING.md A-14): Python opened in blocking
/// mode and hung on a FIFO, holding the hub lock.
pub fn open_regular(path: &Path, follow: bool) -> io::Result<(std::fs::File, std::fs::Metadata)> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut flags = nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC;
    if !follow {
        flags |= nix::libc::O_NOFOLLOW;
    }
    let f = std::fs::OpenOptions::new().read(true).custom_flags(flags).open(path)?;
    let md = f.metadata()?;
    if md.is_dir() {
        return Err(io::Error::from_raw_os_error(nix::libc::EISDIR));
    }
    if !md.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    Ok((f, md))
}

/// [`open_regular`], then read at most `cap` bytes: "file too large" when the file is (or grows) over the cap, checked
/// on the descriptor's size before reading and again while reading.
pub fn read_regular(path: &Path, follow: bool, cap: u64) -> io::Result<Vec<u8>> {
    let (f, md) = open_regular(path, follow)?;
    if md.len() > cap {
        return Err(io::Error::other("file too large"));
    }
    let mut buf = Vec::new();
    f.take(cap.saturating_add(1)).read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        return Err(io::Error::other("file too large"));
    }
    Ok(buf)
}

/// [`py_os_error`] for errors that carry an errno; the reader's own messages ("not a regular file", "file too large")
/// are kept as they are.
fn os_error_at(e: io::Error, path: &Path) -> io::Error {
    if e.raw_os_error().is_some() {
        py_os_error(&e, path)
    } else {
        e
    }
}

// ---- RealFs --------------------------------------------------------------------------------------------------------

/// `is_secret_path(p)`: defence in depth for discovery: never read credential-looking files, even inside an allowed dir.
pub fn is_secret_path(p: &Path) -> bool {
    let s = p.to_string_lossy();
    if s.split('/').any(|part| SECRET_DIRS.contains(&part)) {
        return true;
    }
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    SECRET_NAMES.iter().any(|pat| fnmatchcase(&name, pat))
}

fn perm(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, msg.to_string())
}

/// An OSError as Python renders it (`str(exc)` = `[Errno N] text: 'path'`), kind preserved.
pub fn py_os_error(e: &io::Error, path: &Path) -> io::Error {
    io::Error::new(e.kind(), error_str(e, Some(&path.to_string_lossy())))
}

/// pathlib's `_ignore_error`: ENOENT, ENOTDIR, EBADF, ELOOP make `exists()`/`is_file()`/`is_dir()` False.
fn ignorable(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(nix::libc::ENOENT | nix::libc::ENOTDIR | nix::libc::EBADF | nix::libc::ELOOP)
    )
}

pub struct RealFs {
    /// realpath'd allowed roots
    pub roots: Vec<PathBuf>,
    pub deny_secrets: bool,
}

impl RealFs {
    pub fn new(roots: &[PathBuf], deny_secrets: bool) -> Self {
        Self {
            roots: roots.iter().map(|r| realpath(r)).collect(),
            deny_secrets,
        }
    }

    /// `_ok(path)`: PermissionError outside the roots, FileNotFoundError (invisible) for a secret-looking path.
    fn ok(&self, path: &str) -> io::Result<PathBuf> {
        if path.contains('\0') {
            return Err(perm("bad path"));
        }
        let real = realpath(Path::new(path));
        if !self.roots.iter().any(|r| real.starts_with(r)) {
            return Err(perm("path is outside the configured roots"));
        }
        if self.deny_secrets && is_secret_path(&real) {
            return Err(io::Error::new(io::ErrorKind::NotFound, path.to_string()));
        }
        Ok(real)
    }
}

impl FileSystem for RealFs {
    fn read_bytes(&self, path: &str) -> io::Result<Vec<u8>> {
        let p = self.ok(path)?;
        // `p` is already resolved, so a symlink at the last component now can only be a swap: refused (ELOOP)
        read_regular(&p, false, MAX_READ).map_err(|e| os_error_at(e, &p))
    }

    fn exists(&self, path: &str) -> bool {
        match self.ok(path) {
            Ok(p) => std::fs::metadata(p).is_ok(),
            Err(_) => false,
        }
    }

    fn listdir(&self, path: &str) -> io::Result<Vec<String>> {
        let base = self.ok(path)?;
        let mut out = Vec::new();
        for e in std::fs::read_dir(&base).map_err(|e| py_os_error(&e, &base))? {
            let e = e.map_err(|e| py_os_error(&e, &base))?;
            let name = e.file_name().to_string_lossy().to_string();
            if self.deny_secrets && is_secret_path(&base.join(&name)) {
                continue;
            }
            out.push(name);
        }
        out.sort();
        Ok(out)
    }

    fn is_dir(&self, path: &str) -> bool {
        match self.ok(path) {
            Ok(p) => p.is_dir(),
            Err(_) => false,
        }
    }

    /// stat only (same allowed-path checks), so callers can cap BEFORE reading.
    fn size(&self, path: &str) -> Option<io::Result<u64>> {
        Some(
            self.ok(path)
                .and_then(|p| std::fs::metadata(&p).map(|m| m.len()).map_err(|e| py_os_error(&e, &p))),
        )
    }
}

// ---- runners -------------------------------------------------------------------------------------------------------

/// The CommandRunner adapters use for verify (Python `RealRunner`): argv only, `min(timeout, max_timeout)`, minimal
/// environment, stdout capped at `max_stdout` characters and stderr at 20 000. The output is also capped while it is
/// read (4 bytes per character plus one character: the decoded, truncated text is the same), so a child that prints
/// without end, such as `docker exec` into a container the hub does not trust, cannot exhaust memory (audit F4).
const STDERR_CHARS: usize = 20_000;

pub struct RealRunner {
    pub max_timeout: f64,
    pub max_stdout: usize,
}

impl Default for RealRunner {
    fn default() -> Self {
        Self::new(60.0, 100_000)
    }
}

impl RealRunner {
    pub fn new(max_timeout: f64, max_stdout: usize) -> Self {
        Self {
            max_timeout,
            max_stdout,
        }
    }
}

impl CommandRunner for RealRunner {
    fn run(&self, argv: &[String], timeout: f64) -> io::Result<(i32, String, String)> {
        if argv.is_empty() || argv.iter().any(|a| a.contains('\0')) {
            return Ok((127, String::new(), "invalid argv".into()));
        }
        let env = minimal_env();
        let byte_cap = |chars: usize| chars.saturating_mul(4).saturating_add(4);
        let caps = (Some(byte_cap(self.max_stdout)), Some(byte_cap(STDERR_CHARS)));
        match capture(argv, None, &env, None, timeout.min(self.max_timeout), caps.0, caps.1) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((127, String::new(), "command not found".into())),
            Err(e) => Err(e),
            Ok(c) if c.timed_out => Ok((124, String::new(), "timed out".into())),
            Ok(c) => Ok((
                c.rc,
                decode_cap(&c.stdout, self.max_stdout),
                decode_cap(&c.stderr, STDERR_CHARS),
            )),
        }
    }
}

/// CommandRunner for output that must be treated as hostile (`observe` execs a program inside an agent's container):
/// the child gets its own process group, stdout and stderr are read in chunks into buffers that can never exceed their
/// caps, and the first byte over a cap kills the whole group and returns (125, "", "output exceeded cap"). Memory use
/// is bounded by max_stdout + max_stderr whatever the child prints. No shell, minimal environment, a hard timeout.
pub struct StreamingRunner {
    pub max_timeout: f64,
    pub max_stdout: usize,
    pub max_stderr: usize,
}

impl Default for StreamingRunner {
    fn default() -> Self {
        Self::new(60.0, 100_000, 1024 * 1024)
    }
}

impl StreamingRunner {
    pub fn new(max_timeout: f64, max_stdout: usize, max_stderr: usize) -> Self {
        Self {
            max_timeout,
            max_stdout,
            max_stderr,
        }
    }
}

fn killpg(pgid: u32) {
    if let Ok(p) = i32::try_from(pgid) {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(p), nix::sys::signal::Signal::SIGKILL);
    }
}

/// `pump(stream, buf, cap)`: chunked reads; the first chunk that would pass the cap sets `over` and kills the group.
fn pump(mut r: impl Read, cap: usize, pgid: u32, over: &AtomicBool) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 65536];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => return buf,
            Ok(n) => {
                if buf.len() + n > cap {
                    over.store(true, Ordering::SeqCst);
                    killpg(pgid);
                    return buf;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
        }
    }
}

impl CommandRunner for StreamingRunner {
    fn run(&self, argv: &[String], timeout: f64) -> io::Result<(i32, String, String)> {
        let Some((prog, args)) = argv.split_first() else {
            return Ok((127, String::new(), "invalid argv".into()));
        };
        if argv.iter().any(|a| a.contains('\0')) {
            return Ok((127, String::new(), "invalid argv".into()));
        }
        let mut cmd = Command::new(prog);
        cmd.args(args)
            .env_clear()
            .envs(minimal_env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Python: start_new_session=True. The kill relies on the new process group (pgid = pid); see
            // PORTING.md deviation D-B2-SESSION.
            .process_group(0);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok((127, String::new(), "command not found".into()));
            }
            Err(e) => {
                return Ok((
                    126,
                    String::new(),
                    format!("cannot start: {}", hub_core::py::os::error_name(&e)),
                ));
            }
        };
        let pgid = child.id();
        let over = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<(u8, Vec<u8>)>();
        if let Some(o) = child.stdout.take() {
            let (tx, over, cap) = (tx.clone(), over.clone(), self.max_stdout);
            std::thread::spawn(move || {
                let b = pump(o, cap, pgid, &over);
                let _ = tx.send((1, b));
            });
        }
        if let Some(e) = child.stderr.take() {
            let (tx, over, cap) = (tx.clone(), over.clone(), self.max_stderr);
            std::thread::spawn(move || {
                let b = pump(e, cap, pgid, &over);
                let _ = tx.send((2, b));
            });
        }
        drop(tx);
        let deadline = deadline_after(timeout.min(self.max_timeout));
        let mut timed_out = false;
        let mut status = None;
        let mut nap = Duration::from_micros(500);
        loop {
            if let Some(st) = child.try_wait()? {
                status = Some(st);
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                timed_out = true;
                killpg(pgid);
                let until = Instant::now() + Duration::from_secs(10); // proc.wait(timeout=10)
                while Instant::now() < until {
                    if let Some(st) = child.try_wait()? {
                        status = Some(st);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                break;
            }
            std::thread::sleep(nap.min(deadline - now));
            nap = (nap * 2).min(Duration::from_millis(20));
        }
        killpg(pgid); // also reaps grandchildren that kept the group alive
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let join_by = Instant::now() + Duration::from_secs(5);
        for _ in 0..2 {
            match rx.recv_timeout(join_by.saturating_duration_since(Instant::now())) {
                Ok((1, b)) => out = b,
                Ok((_, b)) => err = b,
                Err(_) => break,
            }
        }
        if over.load(Ordering::SeqCst) {
            return Ok((125, String::new(), "output exceeded cap".into()));
        }
        if timed_out {
            return Ok((124, String::new(), "timed out".into()));
        }
        let rc = status.map_or(-9, returncode);
        Ok((
            rc,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        ))
    }
}

// ---- LiveFiles -----------------------------------------------------------------------------------------------------

/// A PathError carried through an `io::Error` (kind `InvalidInput`; callers render `str(exc)`, the message for both).
pub fn path_io_error(e: &PathError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, e.0.clone())
}

/// pathlib `exists()`/`is_file()` semantics: ignorable errors read as "absent", anything else is raised.
fn stat_follow(p: &Path) -> io::Result<Option<std::fs::Metadata>> {
    match std::fs::metadata(p) {
        Ok(m) => Ok(Some(m)),
        Err(e) if ignorable(&e) => Ok(None),
        Err(e) => Err(py_os_error(&e, p)),
    }
}

/// Read/locate files under a client's named roots with the path-safety rules of ARCHITECTURE section 4.
pub struct LiveFiles {
    pub roots: IndexMap<String, String>,
    pub max_bytes: u64,
}

impl LiveFiles {
    pub fn new(roots: &IndexMap<String, String>, max_bytes: u64) -> Self {
        Self {
            roots: roots.clone(),
            max_bytes,
        }
    }

    /// `target(root, rel)`: PathError for an unknown root or an unsafe/symlinked path.
    pub fn target(&self, root: &str, rel: &str) -> Result<PathBuf, PathError> {
        let Some(base) = self.roots.get(root) else {
            return Err(PathError(format!(
                "root {} is not configured",
                hub_core::py::repr(root)
            )));
        };
        resolve_under(Path::new(base), rel)
    }

    /// `read(root, rel)`: None when the file does not exist. PathError (as `InvalidInput`) / OSError for unsafe or
    /// non-regular targets; the error's Display is Python's `str(exc)`.
    pub fn read(&self, root: &str, rel: &str) -> io::Result<Option<Vec<u8>>> {
        let p = self.target(root, rel).map_err(|e| path_io_error(&e))?;
        let Some(md) = stat_follow(&p)? else {
            return Ok(None);
        };
        if !md.is_file() {
            return Err(io::Error::other("not a regular file"));
        }
        if md.len() > self.max_bytes {
            return Err(io::Error::other("file too large"));
        }
        // the stat above gives Python's answers; the read itself re-checks on the opened descriptor (a FIFO or a link
        // swapped in since the stat is refused instead of blocking under the hub lock)
        read_regular(&p, false, self.max_bytes)
            .map(Some)
            .map_err(|e| os_error_at(e, &p))
    }
}

/// `discovery_fs(roots, params, extra)`: the READ-ONLY discovery view: exactly the client's roots, the adapter's
/// declared `params.source_dirs` (relative to `params.source_root`, no `..`, no symlink escape) and operator-configured
/// extras. Secret-looking files are invisible.
pub fn discovery_fs(roots: &IndexMap<String, String>, params: &PyValue, extra: &[String]) -> RealFs {
    let mut allowed: Vec<PathBuf> = roots.values().map(PathBuf::from).collect();
    allowed.extend(extra.iter().map(PathBuf::from));
    // the first of source_root / base_dir / lab_dir (legacy name) that is a str, even an empty one
    let base = ["source_root", "base_dir", "lab_dir"]
        .iter()
        .find_map(|k| params.get(k).and_then(PyValue::as_str));
    let srcs = params.get("source_dirs").and_then(PyValue::as_list);
    if let (Some(base), Some(srcs)) = (base, srcs)
        && !base.is_empty()
        && !base.contains('\0')
    {
        let home = std::env::var("HOME").unwrap_or_default();
        let base_real = realpath(Path::new(&expanduser(base, &home)));
        for entry in srcs {
            // adapters declare {kind, path} or a bare path
            let rel = if entry.as_dict().is_some() {
                entry.get("path")
            } else {
                Some(entry)
            };
            let Some(rel) = rel.and_then(PyValue::as_str) else {
                continue;
            };
            if rel.is_empty() || rel.contains('\0') || rel.split('/').any(|p| p == "..") {
                continue;
            }
            let cand = PathBuf::from(expanduser(rel, &home));
            let cand = if cand.is_absolute() { cand } else { base_real.join(cand) };
            let real = realpath(&cand);
            if real.starts_with(&base_real) {
                // a link that leaves the base dir is refused
                allowed.push(real);
            }
        }
    }
    RealFs::new(&allowed, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_paths() {
        for s in [
            "/a/.ssh/x",
            "/a/.env",
            "/a/.env.local",
            "/a/b.pem",
            "/k.key",
            "/id_rsa_x",
            "/credentials.json",
            "/c.p12",
            "/x/.gnupg",
        ] {
            assert!(is_secret_path(Path::new(s)), "{s}");
        }
        for s in ["/a/env", "/a/x.keys", "/a/sshd/x", "/"] {
            assert!(!is_secret_path(Path::new(s)), "{s}");
        }
    }

    #[test]
    fn capture_basics() -> io::Result<()> {
        let env = [("PATH", path_env())];
        let c = capture(&["cat".into()], None, &env, Some(b"hello".to_vec()), 10.0, None, None)?;
        assert_eq!((c.rc, c.stdout.as_slice(), c.timed_out), (0, &b"hello"[..], false));
        let c = capture(&["sleep".into(), "5".into()], None, &env, None, 0.2, None, None)?;
        assert!(c.timed_out && c.rc == -9);
        let argv: Vec<String> = ["head", "-c", "100000", "/dev/zero"].map(String::from).to_vec();
        let c = capture(&argv, None, &env, None, 10.0, Some(10), None)?;
        assert!(c.over_cap && c.stdout.len() == 10 && c.rc == 0);
        let e = capture(&["/nonexistent/x".into()], None, &env, None, 1.0, None, None).err();
        assert_eq!(e.map(|e| e.kind()), Some(io::ErrorKind::NotFound));
        Ok(())
    }

    type R = Result<(), Box<dyn std::error::Error>>;

    fn p(path: &Path) -> String {
        path.to_string_lossy().to_string()
    }

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    /// tests/core/test_fs_merge.py::test_realfs_is_limited_to_roots
    #[test]
    fn test_realfs_is_limited_to_roots() -> R {
        let d = tempfile::tempdir()?;
        let (root, outside) = (d.path().join("root"), d.path().join("outside"));
        std::fs::create_dir(&root)?;
        std::fs::create_dir(&outside)?;
        std::fs::write(root.join("a.txt"), "a")?;
        std::fs::write(outside.join("s.txt"), "secret")?;
        std::os::unix::fs::symlink(&outside, root.join("link"))?;
        let fs = RealFs::new(std::slice::from_ref(&root), false);
        assert_eq!(fs.read_bytes(&p(&root.join("a.txt")))?, b"a");
        assert!(fs.exists(&p(&root.join("a.txt"))) && fs.is_dir(&p(&root)));
        assert_eq!(fs.listdir(&p(&root))?, vec!["a.txt", "link"]);
        for bad in [
            p(&outside.join("s.txt")),
            p(&root.join("link/s.txt")),
            p(&root.join("../outside/s.txt")),
            "/etc/hostname".to_string(),
        ] {
            let e = fs.read_bytes(&bad).err().ok_or("read outside the roots")?;
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{bad}");
            assert_eq!(e.to_string(), "path is outside the configured roots");
            assert!(!fs.exists(&bad) && !fs.is_dir(&bad));
        }
        assert_eq!(
            fs.listdir("/etc").err().map(|e| e.kind()),
            Some(io::ErrorKind::PermissionDenied)
        );
        let e = fs.read_bytes(&p(&root.join("a\0b"))).err().ok_or("read a NUL path")?;
        assert_eq!(
            (e.kind(), e.to_string()),
            (io::ErrorKind::PermissionDenied, "bad path".to_string())
        );
        Ok(())
    }

    /// tests/core/test_fs_merge.py::test_realrunner_no_shell_timeout_and_limits
    #[test]
    fn test_realrunner_no_shell_timeout_and_limits() -> io::Result<()> {
        let r = RealRunner::new(1.0, 100_000);
        let (rc, out, _) = r.run(&argv(&["echo", "$HOME; echo pwned"]), 5.0)?;
        assert_eq!((rc, out.trim()), (0, "$HOME; echo pwned")); // no shell expansion
        assert_eq!(r.run(&argv(&["definitely-not-a-command-xyz"]), 5.0)?.0, 127);
        assert_eq!(r.run(&[], 5.0)?, (127, String::new(), "invalid argv".into()));
        assert_eq!(r.run(&argv(&["a\0b"]), 5.0)?.0, 127);
        let t0 = Instant::now();
        assert_eq!(
            r.run(&argv(&["sleep", "5"]), 30.0)?,
            (124, String::new(), "timed out".into())
        ); // capped
        assert!(t0.elapsed() < Duration::from_secs(4));
        // stdout capped in characters, environment minimal
        let small = RealRunner::new(10.0, 3);
        assert_eq!(small.run(&argv(&["printf", "héllo"]), 5.0)?.1, "hél");
        let (_, env_out, _) = r.run(&argv(&["env"]), 5.0)?;
        let keys: Vec<&str> = env_out.lines().filter_map(|l| l.split_once('=').map(|x| x.0)).collect();
        assert_eq!(keys.len(), 3, "{env_out}");
        assert!(["PATH", "HOME", "LC_ALL"].iter().all(|k| keys.contains(k)));
        Ok(())
    }

    /// tests/core/test_fs_merge.py::test_livefiles
    #[test]
    fn test_livefiles() -> R {
        let d = tempfile::tempdir()?;
        let lf = LiveFiles::new(&IndexMap::from([("home".to_string(), p(d.path()))]), 10);
        assert_eq!(lf.read("home", "missing")?, None);
        std::fs::write(d.path().join("f"), "hi")?;
        assert_eq!(lf.read("home", "f")?, Some(b"hi".to_vec()));
        std::fs::write(d.path().join("big"), "x".repeat(100))?;
        std::fs::create_dir(d.path().join("d"))?;
        assert_eq!(
            lf.read("home", "big").err().map(|e| e.to_string()),
            Some("file too large".into())
        );
        assert_eq!(
            lf.read("home", "d").err().map(|e| e.to_string()),
            Some("not a regular file".into())
        );
        let e = lf.read("nope", "f").err().ok_or("read an unknown root")?;
        assert_eq!(
            (e.kind(), e.to_string()),
            (io::ErrorKind::InvalidInput, "root 'nope' is not configured".into())
        );
        assert_eq!(
            lf.target("nope", "f").err().map(|e| e.0),
            Some("root 'nope' is not configured".into())
        );
        Ok(())
    }

    /// tests/core/test_fs_merge.py::test_realfs_size_uses_stat_with_the_same_boundaries
    #[test]
    fn test_realfs_size_uses_stat_with_the_same_boundaries() -> R {
        let d = tempfile::tempdir()?;
        let (root, outside) = (d.path().join("root"), d.path().join("outside"));
        std::fs::create_dir(&root)?;
        std::fs::create_dir(&outside)?;
        std::fs::write(root.join("a"), "12345")?;
        std::fs::write(outside.join("b"), "x")?;
        let fs = RealFs::new(std::slice::from_ref(&root), false);
        assert_eq!(fs.size(&p(&root.join("a"))).ok_or("no size")??, 5);
        let e = fs.size(&p(&outside.join("b"))).ok_or("no size")?.err().ok_or("sized")?;
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        let missing = root.join("missing");
        let e = fs.size(&p(&missing)).ok_or("no size")?.err().ok_or("sized")?;
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            e.to_string(),
            format!(
                "[Errno 2] No such file or directory: {}",
                hub_core::py::repr(&p(&realpath(&missing)))
            )
        );
        Ok(())
    }

    /// tests/core/test_discovery_fs.py::test_discovery_fs_source_dirs_denylist_and_escapes
    #[test]
    fn test_discovery_fs_source_dirs_denylist_and_escapes() -> R {
        let t = tempfile::tempdir()?;
        let (base, stage, other) = (t.path().join("base"), t.path().join("stage"), t.path().join("other"));
        for d in [
            base.join("clients/x/skills/s1"),
            base.join(".ssh"),
            stage.clone(),
            other.clone(),
        ] {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(base.join("clients/x/skills/s1/SKILL.md"), "skill")?;
        let secrets = [
            ".env",
            ".env.local",
            "a.pem",
            "b.key",
            "id_rsa_x",
            "credentials.json",
            "c.p12",
        ];
        for n in secrets {
            std::fs::write(base.join("clients/x/skills").join(n), "s")?;
        }
        std::fs::create_dir(base.join("clients/x/skills/.ssh"))?;
        std::fs::write(base.join("clients/x/skills/.ssh/config"), "s")?;
        std::fs::write(base.join("clients/x/AGENTS.md"), "agents")?;
        std::fs::create_dir(base.join("clients/y"))?;
        std::fs::write(base.join("clients/y/private.md"), "no")?;
        std::fs::write(other.join("leak.txt"), "leak")?;
        std::os::unix::fs::symlink(&other, base.join("clients/x/skills/escape"))?;
        std::os::unix::fs::symlink(&other, base.join("linkdir"))?;
        let params = PyValue::Dict(vec![
            (PyValue::str("source_root"), PyValue::str(p(&base))),
            (
                PyValue::str("source_dirs"),
                PyValue::List(vec![
                    PyValue::Dict(vec![
                        (PyValue::str("kind"), PyValue::str("skills")),
                        (PyValue::str("path"), PyValue::str("clients/x/skills")),
                    ]),
                    PyValue::str("clients/x/AGENTS.md"),
                    PyValue::str("../other"),
                    PyValue::str("linkdir"),
                    PyValue::str("/etc"),
                ]),
            ),
        ]);
        let fs = discovery_fs(&IndexMap::from([("stage".to_string(), p(&stage))]), &params, &[]);
        let skills = p(&base.join("clients/x/skills"));
        // (a) source_dirs outside roots are discoverable
        assert_eq!(fs.read_bytes(&format!("{skills}/s1/SKILL.md"))?, b"skill");
        assert_eq!(fs.read_bytes(&p(&base.join("clients/x/AGENTS.md")))?, b"agents");
        assert_eq!(fs.listdir(&skills)?, vec!["escape", "s1"]); // every secret-looking name is invisible
        for n in secrets.iter().copied().chain([".ssh/config"]) {
            let f = format!("{skills}/{n}");
            assert!(!fs.exists(&f));
            assert_eq!(
                fs.read_bytes(&f).err().map(|e| e.kind()),
                Some(io::ErrorKind::NotFound),
                "{n}"
            );
        }
        assert!(!fs.exists(&p(&base.join(".ssh"))));
        // (b) unlisted sibling, `..`, symlink escapes, and out-of-base source_dirs are refused
        for bad in [
            p(&base.join("clients/y/private.md")),
            p(&other.join("leak.txt")),
            format!("{skills}/escape/leak.txt"),
            p(&base.join("linkdir/leak.txt")),
            format!("{skills}/../../y/private.md"),
            "/etc/hostname".to_string(),
        ] {
            assert_eq!(
                fs.read_bytes(&bad).err().map(|e| e.kind()),
                Some(io::ErrorKind::PermissionDenied),
                "{bad}"
            );
        }
        assert!(fs.exists(&p(&stage)) && !fs.exists(&p(&base.join("clients/y"))));
        Ok(())
    }

    /// Run `f` on a thread; fail (instead of hanging the suite) when it has not returned within 5 s. A reader stuck in
    /// a blocking open of `fifo` is released by opening the FIFO for writing.
    fn within_5s<T: Send + 'static>(fifo: &Path, f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(v) => Ok(v),
            Err(_) => {
                let _ = std::fs::OpenOptions::new().write(true).open(fifo); // unblocks the stuck reader
                Err(format!("blocked on the FIFO at {}", fifo.display()))
            }
        }
    }

    fn mkfifo(p: &Path) -> R {
        nix::unistd::mkfifo(p, nix::sys::stat::Mode::from_bits_truncate(0o600))?;
        Ok(())
    }

    /// Audit F1 (0.3.0): a FIFO, a device, a directory or a swapped-in link at a path the hub reads never blocks the
    /// read: the type is checked on the opened descriptor.
    #[test]
    fn read_regular_never_blocks_on_special_files() -> R {
        let d = tempfile::tempdir()?;
        let fifo = d.path().join("fifo");
        mkfifo(&fifo)?;
        let f2 = fifo.clone();
        let r = within_5s(&fifo, move || read_regular(&f2, false, 100).map_err(|e| e.to_string()))?;
        assert_eq!(r, Err("not a regular file".to_string()));
        assert_eq!(
            read_regular(Path::new("/dev/zero"), true, 100)
                .err()
                .map(|e| e.to_string()),
            Some("not a regular file".into())
        );
        let e = read_regular(d.path(), true, 100).err().ok_or("read a directory")?;
        assert_eq!(e.raw_os_error(), Some(nix::libc::EISDIR));
        std::fs::write(d.path().join("f"), "12345")?;
        std::os::unix::fs::symlink(d.path().join("f"), d.path().join("link"))?;
        assert_eq!(read_regular(&d.path().join("link"), true, 100)?, b"12345");
        let e = read_regular(&d.path().join("link"), false, 100)
            .err()
            .ok_or("followed a link")?;
        assert_eq!(e.raw_os_error(), Some(nix::libc::ELOOP));
        assert_eq!(
            read_regular(&d.path().join("f"), false, 4).err().map(|e| e.to_string()),
            Some("file too large".into())
        );
        assert_eq!(read_regular(&d.path().join("f"), false, 5)?, b"12345");
        Ok(())
    }

    /// Audit F1: RealFs (discovery and every adapter's verify) and LiveFiles (plan, rollback) refuse a FIFO promptly.
    #[test]
    fn realfs_and_livefiles_refuse_a_fifo_promptly() -> R {
        let d = tempfile::tempdir()?;
        let fifo = d.path().join("SKILL.md");
        mkfifo(&fifo)?;
        let fs = RealFs::new(&[d.path().to_path_buf()], false);
        let path = p(&fifo);
        let r = within_5s(&fifo, move || fs.read_bytes(&path).map_err(|e| e.to_string()))?;
        assert_eq!(r, Err("not a regular file".to_string()));
        let lf = LiveFiles::new(&IndexMap::from([("home".to_string(), p(d.path()))]), 100);
        let r = within_5s(&fifo, move || lf.read("home", "SKILL.md").map_err(|e| e.to_string()))?;
        assert_eq!(r, Err("not a regular file".to_string()));
        // the Python-shaped errors are unchanged
        let fs = RealFs::new(&[d.path().to_path_buf()], false);
        let missing = d.path().join("missing");
        assert_eq!(
            fs.read_bytes(&p(&missing)).err().map(|e| e.to_string()),
            Some(format!(
                "[Errno 2] No such file or directory: {}",
                hub_core::py::repr(&p(&realpath(&missing)))
            ))
        );
        assert_eq!(
            fs.read_bytes(&p(d.path())).err().map(|e| e.to_string()),
            Some(format!(
                "[Errno 21] Is a directory: {}",
                hub_core::py::repr(&p(&realpath(d.path())))
            ))
        );
        Ok(())
    }

    /// Audit F4: stderr is capped like stdout (drained, not kept), so a child that floods it cannot exhaust memory.
    #[test]
    fn capture_caps_stderr_too() -> R {
        let env = [("PATH", path_env())];
        let flood = argv(&["/bin/sh", "-c", "head -c 3000000 /dev/zero >&2; echo done"]);
        let c = capture(&flood, None, &env, None, 30.0, Some(100), Some(1000))?;
        assert_eq!((c.rc, c.stdout.as_slice(), c.over_cap), (0, &b"done\n"[..], false));
        assert_eq!((c.stderr.len(), c.err_over_cap), (1000, true));
        let c = capture(&flood, None, &env, None, 30.0, None, Some(3_000_000))?;
        assert_eq!((c.stderr.len(), c.err_over_cap), (3_000_000, false)); // exactly at the cap is kept whole
        Ok(())
    }

    /// Audit F4: RealRunner caps both streams while reading; what it returns is still Python's `decode[:n]` of all of it.
    #[test]
    fn realrunner_caps_while_reading_and_returns_the_same_text() -> R {
        // 3-byte characters, then a flood: the first n characters are kept exactly, invalid bytes become U+FFFD
        let script = "printf '\\342\\202\\254%.0s' $(seq 1 50); printf '\\377'; head -c 2000000 /dev/zero; \
                      printf 'e\\342\\202\\254' >&2; head -c 2000000 /dev/zero >&2";
        let (rc, out, err) = RealRunner::new(30.0, 60).run(&argv(&["/bin/sh", "-c", script]), 30.0)?;
        assert_eq!(rc, 0);
        let want_out: String = "\u{20ac}".repeat(50) + "\u{fffd}" + &"\0".repeat(9);
        assert_eq!(out, want_out);
        assert_eq!(err.chars().count(), STDERR_CHARS);
        assert!(err.starts_with("e\u{20ac}\0"));
        Ok(())
    }

    /// `_gone(pid)`: the process no longer exists (or is a zombie) within 5 s.
    fn gone(pid: i32) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
                return true;
            }
            if std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|s| s.split_whitespace().nth(2) == Some("Z"))
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// tests/core/test_audit_fixes.py::test_streaming_runner_kills_a_child_that_floods_stdout
    #[test]
    fn test_streaming_runner_kills_a_child_that_floods_stdout() -> R {
        let d = tempfile::tempdir()?;
        let pidfile = d.path().join("pid");
        // a child that records its pid, then a grandchild of the same group floods stdout forever
        let script = format!("echo $$ > '{}'; yes xxxxxxxxxxxxxxxx; sleep 60", p(&pidfile));
        let t0 = Instant::now();
        let r = StreamingRunner::new(60.0, 10_000, 1024 * 1024).run(&argv(&["/bin/sh", "-c", &script]), 30.0)?;
        assert_eq!(r, (125, String::new(), "output exceeded cap".into()));
        assert!(t0.elapsed() < Duration::from_secs(20));
        let pid: i32 = std::fs::read_to_string(&pidfile)?.trim().parse()?;
        assert!(gone(pid), "the whole group was killed");
        Ok(())
    }

    /// tests/core/test_audit_fixes.py::test_streaming_runner_caps_stderr_too_and_keeps_normal_semantics
    #[test]
    fn test_streaming_runner_caps_stderr_too_and_keeps_normal_semantics() -> io::Result<()> {
        let flood = argv(&["/bin/sh", "-c", "yes eeeeeeeeeeeeeeee >&2"]);
        assert_eq!(StreamingRunner::new(60.0, 100_000, 5_000).run(&flood, 30.0)?.0, 125);
        let def = StreamingRunner::default();
        assert_eq!(
            def.run(&argv(&["/bin/sh", "-c", "echo hi"]), 30.0)?,
            (0, "hi\n".into(), String::new())
        );
        assert_eq!(
            def.run(&argv(&["/bin/sh", "-c", "printf bad >&2; exit 3"]), 30.0)?,
            (3, String::new(), "bad".into())
        );
        let (rc, _, err) = StreamingRunner::new(1.0, 100_000, 1024 * 1024).run(&argv(&["sleep", "60"]), 50.0)?;
        assert_eq!((rc, err.as_str()), (124, "timed out")); // min(timeout, max_timeout)
        assert_eq!(def.run(&argv(&["/nonexistent/prog"]), 1.0)?.0, 127);
        assert_eq!(def.run(&[], 1.0)?.0, 127);
        // exactly at the cap is fine, one byte more is not
        let ok = StreamingRunner::new(60.0, 100, 1024).run(&argv(&["head", "-c", "100", "/dev/zero"]), 30.0)?;
        assert_eq!((ok.0, ok.1.len()), (0, 100));
        let over = StreamingRunner::new(60.0, 100, 1024).run(&argv(&["head", "-c", "101", "/dev/zero"]), 30.0)?;
        assert_eq!(over.0, 125);
        Ok(())
    }
}
