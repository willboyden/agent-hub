//! Port of services/skilleval.py: NVIDIA SkillEvaluator (and SkillSpector) integration. ADVISORY ONLY: nothing here
//! feeds plan, apply or the policy floor, and a failing evaluation blocks nothing (the import gate reads `gate()`).
//!
//! Containment (S-EVAL-ARGV, S-EVAL-COPY; PORTING.md §11 E-1..E-5):
//! * Launcher: the first of LAUNCHERS found on PATH plus ~/.local/bin. Bare names only, argv lists, no shell, a timeout
//!   per tier, its own process group (killed on timeout/cancel/shutdown), stdout/stderr capped (tails).
//! * Child environment: PATH, HOME, LC_ALL=C plus SKILL_EVAL_* / SKILLEVAL_* from the hub's environment, for these
//!   binaries only.
//! * The skill is COPIED (regular files only, never through a symlink, the hub.yaml sidecar, `.git` and compiled
//!   bytecode excluded, file-count and byte caps) into a private work dir; SkillEvaluator runs there and gets the copy.
//! * EVERY tier1 argv carries `--checks SAFE_CHECKS` (never `dependency`: pip-audit would install packages a skill
//!   chose). Every evaluation runs a static pass first; the LLM pass can only make the gate stricter.
//! * One evaluation at a time: a thread lock plus an flock on <state>/skilleval/.lock (hubctl and the server exclude
//!   each other). One FIFO worker thread drains the in-memory queue. Finished runs are indexed by content hash.
//! * Reports stay under <state>/skilleval/<skill>/<run-id>/ (dirs 0700, files 0600), newest KEEP_RUNS per subject.
//!
//! Owner: WS-C.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use hub_core::common::{atomic_write, scrub};
use hub_core::ids::valid_id;
use hub_core::pathsafe::check_relpath;
use hub_core::py::json::{Opts, dumps};
use hub_core::py::text::{strip, take_chars};
use hub_core::value::PyValue;
use hub_core::{HubResult, Problem};
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value, json};

use super::hub::Hub;
use crate::adapters::base::Files;

pub const LAUNCHERS: [&str; 2] = ["skillevaluator-local", "skillevaluator"];
pub const TIERS: [&str; 3] = ["static", "tier1", "tier1+2"];
pub const INSTALL_HINT: &str =
    "uv tool install --python 3.13 \"skillevaluator[all] @ git+https://github.com/NVIDIA/SkillEvaluator.git\"";
pub const SAFE_CHECKS: &str = "schema,version,security,pii,license,code-integrity,unicode,quality,lint";
pub const KEEP_RUNS: usize = 20;
pub const EVAL_REV: i64 = 2;

pub const ENV_PREFIXES: [&str; 2] = ["SKILL_EVAL_", "SKILLEVAL_"];
pub const RUN_TIMEOUT_S: f64 = 540.0;
pub const VERSION_TIMEOUT_S: f64 = 20.0;
pub const VERSION_TTL_S: f64 = 300.0;
pub const MAX_FILES: usize = 200;
pub const OUTPUT_CAP: usize = 64 * 1024;
pub const REPORT_CAP: u64 = 8 * 1024 * 1024;
pub const RECORD_CAP: u64 = 1024 * 1024;
pub const MARKDOWN_CAP: usize = 512 * 1024;
pub const ERROR_TAIL: usize = 1500;
pub const TOP_FINDINGS: usize = 12;
pub const VERDICTS: [&str; 4] = ["passed", "failed", "incomplete", "error"];
pub const SEVERITIES: [&str; 4] = ["critical", "high", "medium", "low"];
pub const T1_JSON: &str = "skillevaluator-tier1.json";
pub const T1_MD: &str = "skillevaluator-tier1.md";
pub const T2_JSON: &str = "skillevaluator-tier2.json";
pub const RECORD: &str = "run.json";
pub const SKILLSPECTOR: &str = "skillspector";
pub const SKILLSPECTOR_TIMEOUT_S: f64 = 300.0;
pub const SKILLSPECTOR_REPORT: &str = "skillspector.json";
pub const PARTIAL_ERROR: &str =
    "skillspector JSON field 'analysis_completeness' reports incomplete analysis (status 'partial')";
pub const UNRESOLVED_REFERENCES: &str = "unresolved_references";
pub const SIDECAR: &str = "hub.yaml";
pub const COPY_MARK: &str = "<skill-copy>";
pub const WORK_MARK: &str = "<work-dir>";
pub const STATE_MARK: &str = "<state-dir>";
pub const NON_SECURITY_VALIDATORS: [&str; 6] = [
    "Schema & Repository Governance",
    "Semantic Version Validation",
    "License Compliance",
    "QUALITY",
    "SCRIPT_LINT",
    "RUBRIC_EVAL",
];
pub const EXPECTED_SECURITY_VALIDATORS: [&str; 6] = [
    "Security Scan",
    "PII Scan",
    "Code Risk Analysis",
    "Secrets Detection",
    "Code Integrity & Hygiene",
    "Unicode Smuggling Detection",
];
pub const GATE_STATUSES: [&str; 4] = ["missing", "blocked", "warn", "ok"];
pub const STATIC_DIR: &str = "static";
pub const BY_HASH: &str = "@by-hash";
pub const OBSERVED_DIR: &str = "@observed";
pub const QUEUE_CAP: usize = 5000;
pub const QUEUE_SHOWN: usize = 200;
pub const INDEX_CAP: u64 = 32 * 1024 * 1024;
pub const BYTECODE_DIR: &str = "__pycache__";
pub const BYTECODE_SUFFIXES: [&str; 2] = [".pyc", ".pyo"];

/// `TIER_HELP`.
pub fn tier_help(t: &str) -> &'static str {
    match t {
        "static" => "Tier 1 static checks only (--no-llm: no model calls, about 2 s)",
        "tier1" => "Tier 1 with the LLM judge when a provider is configured (rubric and LLM security review)",
        _ => "Tier 1 with the LLM judge, then Tier 2 deduplication (needs an embeddings model)",
    }
}

fn tier_rank(t: &str) -> Option<i64> {
    match t {
        "static" => Some(0),
        "tier1" => Some(1),
        "tier1+2" => Some(2),
        _ => None,
    }
}

fn verdict_rank(v: &str) -> Option<i64> {
    match v {
        "passed" => Some(0),
        "incomplete" => Some(1),
        "failed" => Some(2),
        _ => None,
    }
}

pub(crate) static RUN_ID_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"\A\d{8}T\d{9}Z-[0-9a-f]{8}\z"));
pub(crate) static SUBJECT_RE: LazyLock<Regex> =
    LazyLock::new(|| hub_core::re(r"\A([a-z0-9][a-z0-9._-]{0,63}):([a-z0-9][a-z0-9._-]{0,80})\z"));
pub(crate) static KEY_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"\A[a-z0-9][a-z0-9._-]{0,80}\z"));
pub(crate) static HASH_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"\A[0-9a-f]{64}\z"));
static FINDING_LINE: LazyLock<Regex> =
    LazyLock::new(|| hub_core::re(r"\A\[SECURITY-(CRITICAL|HIGH|MEDIUM|LOW|INFO)\] "));
static ANSI: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"\x1b\[[0-9;?]*[ -/]*[@-~]"));
static VERSION: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"version\s+([0-9A-Za-z.+_-]{1,40})"));
/// `(?![\w.-])` after a scrubbed path: Python's `\w` on str is letters, digits and `_`.
static PATH_CONT: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"\A[\p{L}\p{N}_.\-]"));

/// `KEY_RE.fullmatch(key)` (observed-skill keys).
pub fn valid_key(key: &str) -> bool {
    KEY_RE.is_match(key)
}

/// `RUN_ID_RE.fullmatch(run_id)`.
pub fn valid_run_id(run_id: &str) -> bool {
    RUN_ID_RE.is_match(run_id)
}

// ---- small helpers over JSON values (Python's _dict/_list/_num/_int and str()) -------------------------------------

static EMPTY_MAP: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);

fn dict(v: Option<&Value>) -> &Map<String, Value> {
    v.and_then(Value::as_object).unwrap_or(&EMPTY_MAP)
}

fn list(v: Option<&Value>) -> &[Value] {
    v.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// `_num(v)`: float(v) for an int/float (never a bool).
fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

/// `_int(v)`: int(v) for an int/float (never a bool; floats truncate toward zero).
#[allow(clippy::cast_possible_truncation)]
fn int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|u| i64::try_from(u).unwrap_or(i64::MAX)))
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        _ => None,
    }
}

fn int_value(v: Option<&Value>) -> Value {
    int(v).map_or(Value::Null, Value::from)
}

/// `str(x)`.
fn pystr(v: &Value) -> String {
    PyValue::from_json(v).py_str()
}

/// `str(m.get(k, default))`.
fn get_str(m: &Map<String, Value>, k: &str, default: &str) -> String {
    m.get(k).map_or_else(|| default.to_string(), pystr)
}

/// Python truthiness.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(m)) => !m.is_empty(),
    }
}

/// `str(a or b or c or "")`.
fn first_truthy(m: &Map<String, Value>, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| m.get(*k).filter(|v| truthy(Some(v))))
        .map(pystr)
        .unwrap_or_default()
}

fn is_int(v: Option<&Value>) -> bool {
    v.is_some_and(|v| v.is_i64() || v.is_u64())
}

fn is_str(v: Option<&Value>, s: &str) -> bool {
    v.and_then(Value::as_str) == Some(s)
}

// ---- the runner port -----------------------------------------------------------------------------------------------

/// A finished child process (`EvalResult`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EvalResult {
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// The runner port (fakes in tests): argv list, cwd, env, timeout; `cancel()` kills every live child group.
pub trait EvalRunner: Send + Sync {
    fn run(&self, argv: &[String], cwd: &Path, env: &[(String, String)], timeout: f64) -> EvalResult;
    fn cancel(&self);
}

/// `_Tail`: the last `cap` bytes of a stream.
struct Tail {
    cap: usize,
    buf: Vec<u8>,
    dropped: usize,
}

impl Tail {
    fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > self.cap {
            let extra = self.buf.len() - self.cap;
            self.dropped += extra;
            self.buf.drain(..extra);
        }
    }
    fn text(&self) -> String {
        format!(
            "{}{}",
            if self.dropped > 0 { "[...] " } else { "" },
            String::from_utf8_lossy(&self.buf)
        )
    }
}

fn drain(mut stream: impl Read, tail: &Mutex<Tail>) {
    let mut chunk = vec![0u8; 65536];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => tail
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .feed(&chunk[..n]),
        }
    }
}

fn kill_group(pid: i32) {
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
}

/// `RealEvalRunner`: argv list, no shell, the given cwd and env, a hard timeout. The child gets its own process group so a
/// timeout or a hub shutdown kills everything it started; stdout/stderr are drained by threads that keep a bounded tail.
pub struct RealEvalRunner {
    pub output_cap: usize,
    /// every live (not yet reaped) child: a version probe must not hide a running evaluation
    procs: Mutex<HashMap<u32, ()>>,
}

impl Default for RealEvalRunner {
    fn default() -> Self {
        Self::new(OUTPUT_CAP)
    }
}

impl RealEvalRunner {
    pub fn new(output_cap: usize) -> Self {
        Self {
            output_cap,
            procs: Mutex::new(HashMap::new()),
        }
    }

    /// How many children are alive (Python's tests read `len(r._procs)`).
    pub fn live_children(&self) -> usize {
        self.procs().len()
    }

    fn procs(&self) -> std::sync::MutexGuard<'_, HashMap<u32, ()>> {
        self.procs.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Poll the child under the procs lock, so `cancel()` never signals a group whose leader was already reaped.
    fn try_reap(&self, child: &mut Child, pid: u32) -> Option<i32> {
        let mut g = self.procs();
        match child.try_wait() {
            Ok(Some(st)) => {
                g.remove(&pid);
                Some(st.code().unwrap_or_else(|| -st.signal().unwrap_or(9)))
            }
            Ok(None) => None,
            Err(_) => {
                g.remove(&pid);
                Some(-9)
            }
        }
    }
}

impl EvalRunner for RealEvalRunner {
    fn run(&self, argv: &[String], cwd: &Path, env: &[(String, String)], timeout: f64) -> EvalResult {
        if argv.is_empty() || argv.iter().any(|a| a.is_empty() || a.contains('\0')) {
            return EvalResult {
                returncode: 127,
                stdout: String::new(),
                stderr: "invalid argv".into(),
                timed_out: false,
            };
        }
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = {
            let mut g = self.procs();
            match cmd.spawn() {
                Ok(c) => {
                    g.insert(c.id(), ());
                    c
                }
                Err(e) => {
                    let (rc, msg) = if e.kind() == io::ErrorKind::NotFound {
                        (127, "command not found".to_string())
                    } else {
                        (126, format!("cannot start: {}", hub_core::py::os::error_name(&e)))
                    };
                    return EvalResult {
                        returncode: rc,
                        stdout: String::new(),
                        stderr: msg,
                        timed_out: false,
                    };
                }
            }
        };
        let pid = child.id();
        let pid_i = i32::try_from(pid).unwrap_or(0);
        let out = Arc::new(Mutex::new(Tail {
            cap: self.output_cap,
            buf: Vec::new(),
            dropped: 0,
        }));
        let err = Arc::new(Mutex::new(Tail {
            cap: self.output_cap,
            buf: Vec::new(),
            dropped: 0,
        }));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let mut readers = 0;
        if let Some(s) = child.stdout.take() {
            let (t, tx) = (out.clone(), tx.clone());
            readers += 1;
            std::thread::spawn(move || {
                drain(s, &t);
                let _ = tx.send(());
            });
        }
        if let Some(s) = child.stderr.take() {
            let (t, tx) = (err.clone(), tx.clone());
            readers += 1;
            std::thread::spawn(move || {
                drain(s, &t);
                let _ = tx.send(());
            });
        }
        drop(tx);
        let start = Instant::now();
        let limit = Duration::from_secs_f64(timeout.max(0.0));
        let mut timed_out = false;
        let rc = loop {
            if let Some(rc) = self.try_reap(&mut child, pid) {
                break rc;
            }
            if start.elapsed() >= limit {
                timed_out = true;
                {
                    let g = self.procs();
                    if g.contains_key(&pid) {
                        kill_group(pid_i);
                    }
                }
                let deadline = Instant::now() + Duration::from_secs(10);
                let rc = loop {
                    if let Some(rc) = self.try_reap(&mut child, pid) {
                        break Some(rc);
                    }
                    if Instant::now() >= deadline {
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                };
                self.procs().remove(&pid);
                break rc.unwrap_or(-9);
            }
            let left = limit.saturating_sub(start.elapsed());
            std::thread::sleep(left.min(Duration::from_millis(20)));
        };
        // a grandchild that left the group could hold a pipe open: bounded wait only
        let deadline = Instant::now() + Duration::from_secs(5);
        for _ in 0..readers {
            let left = deadline.saturating_duration_since(Instant::now());
            if rx.recv_timeout(left).is_err() {
                break;
            }
        }
        let text = |t: &Arc<Mutex<Tail>>| t.lock().unwrap_or_else(std::sync::PoisonError::into_inner).text();
        EvalResult {
            returncode: rc,
            stdout: text(&out),
            stderr: text(&err),
            timed_out,
        }
    }

    fn cancel(&self) {
        let g = self.procs();
        for pid in g.keys() {
            kill_group(i32::try_from(*pid).unwrap_or(0));
        }
    }
}

// ---- reading a skill -----------------------------------------------------------------------------------------------

/// What will be evaluated (`Snapshot`).
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub skill: String,
    pub files: Files,
    pub modes: HashMap<String, u32>,
    pub subject: String,
    /// where the skill lives (hub: its name; observed: the path under the client's skills root)
    pub path: String,
    pub origin: String,
}

impl Snapshot {
    pub fn content_hash(&self) -> String {
        content_hash(&self.files)
    }
    pub fn total_bytes(&self) -> usize {
        self.files.values().map(Vec::len).sum()
    }
}

/// `content_hash(files)`: sha256 over the (relative path, bytes) pairs sorted by path, both length-prefixed (8 bytes,
/// big endian), so no two different file sets produce the same byte stream.
pub fn content_hash(files: &Files) -> String {
    use sha2::{Digest, Sha256};
    let mut keys: Vec<&String> = files.keys().collect();
    keys.sort();
    let mut h = Sha256::new();
    for rel in keys {
        let p = rel.as_bytes();
        let data = &files[rel];
        h.update((p.len() as u64).to_be_bytes());
        h.update(p);
        h.update((data.len() as u64).to_be_bytes());
        h.update(data);
    }
    hex::encode(h.finalize())
}

/// `is_bytecode(rel)`: compiled Python bytecode is generated, not authored, and unreadable to the scanners; it is never
/// evaluated, observed or imported, so what was evaluated is what gets delivered (c419f1e).
pub fn is_bytecode(rel: &str) -> bool {
    rel.split('/').any(|p| p == BYTECODE_DIR) || BYTECODE_SUFFIXES.iter().any(|s| rel.ends_with(s))
}

/// `_read_regular` failures that are not "skip this entry".
#[derive(Debug)]
pub(crate) enum ReadErr {
    /// `_TooBig`
    TooBig,
    Os(io::Error),
}

/// `_read_regular(path, limit)`: (bytes, mode) of a REGULAR file opened without following a symlink, `Ok(None)` for
/// anything else (symlink, FIFO, socket, device, vanished); `TooBig` past `limit` bytes.
pub(crate) fn read_regular(path: &Path, limit: u64) -> Result<Option<(Vec<u8>, u32)>, ReadErr> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path);
    let mut f = match f {
        Ok(f) => f,
        Err(e) => {
            let skip = matches!(
                e.raw_os_error(),
                Some(
                    nix::libc::ELOOP
                        | nix::libc::ENOENT
                        | nix::libc::ENXIO
                        | nix::libc::EACCES
                        | nix::libc::ENOTDIR
                        | nix::libc::EISDIR
                )
            );
            return if skip { Ok(None) } else { Err(ReadErr::Os(e)) };
        }
    };
    let st = f.metadata().map_err(ReadErr::Os)?;
    if !st.file_type().is_file() {
        return Ok(None);
    }
    if st.len() > limit {
        return Err(ReadErr::TooBig);
    }
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 65536];
    loop {
        let n = match f.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(ReadErr::Os(e)),
        };
        if n == 0 {
            break;
        }
        if (out.len() + n) as u64 > limit {
            return Err(ReadErr::TooBig);
        }
        out.extend_from_slice(&chunk[..n]);
    }
    Ok(Some((out, st.mode() & 0o7777)))
}

/// `json.loads(bytes)` into a value (a UTF-8 BOM is accepted, like Python's encoding detection).
pub(crate) fn json_loads(bytes: &[u8]) -> Option<Value> {
    let b = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    serde_json::from_slice(b).ok()
}

fn is_symlink(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

/// `os.walk(top, followlinks=False)` names of one directory: (dirnames, filenames), each sorted. A symlink to a
/// directory is listed as a directory (scandir's is_dir follows links), like os.walk.
fn list_dir(d: &Path) -> (Vec<String>, Vec<String>) {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let Ok(rd) = fs::read_dir(d) else {
        return (dirs, files);
    };
    for e in rd.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if fs::metadata(e.path()).is_ok_and(|m| m.is_dir()) {
            dirs.push(name);
        } else {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();
    (dirs, files)
}

/// `_walk_skill(skill_dir, skip_sidecar)`: (relative posix path, absolute path) of every candidate file, top-down.
/// Symlinked directories, `.git` and `__pycache__` are not entered, bytecode files are skipped, the top-level hub.yaml
/// sidecar is skipped (not for an observed skill), and unsafe relative paths are dropped.
pub(crate) fn walk_skill(skill_dir: &Path, skip_sidecar: bool) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    walk_into(skill_dir, skill_dir, skip_sidecar, &mut out);
    out
}

fn walk_into(base: &Path, d: &Path, skip_sidecar: bool, out: &mut Vec<(String, PathBuf)>) {
    let (dirs, files) = list_dir(d);
    for name in files {
        let full = d.join(&name);
        let Some(rel) = full.strip_prefix(base).ok().and_then(Path::to_str).map(str::to_string) else {
            continue;
        };
        if skip_sidecar && rel == SIDECAR {
            continue;
        }
        if is_bytecode(&rel) || check_relpath(&rel).is_err() {
            continue;
        }
        out.push((rel, full));
    }
    for name in dirs {
        if name == ".git" || name == BYTECODE_DIR {
            continue;
        }
        let sub = d.join(&name);
        if is_symlink(&sub) {
            continue;
        }
        walk_into(base, &sub, skip_sidecar, out);
    }
}

/// `shutil.rmtree(path, ignore_errors=True)`: never through a symlink (Python refuses to rmtree a link).
pub(crate) fn rmtree(p: &Path) {
    if is_symlink(p) {
        return;
    }
    let _ = fs::remove_dir_all(p);
}

/// `_mkdir_private(d)`: mkdir -p where every directory this call creates is 0700, then `d` itself is chmod 0700.
pub(crate) fn mkdir_private(d: &Path) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut p = d.to_path_buf();
    while !p.exists() {
        match p.parent() {
            Some(parent) if parent != p => {
                missing.push(p.clone());
                p = parent.to_path_buf();
            }
            _ => break,
        }
    }
    for m in missing.iter().rev() {
        match fs::DirBuilder::new().mode(0o700).create(m) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    let md = fs::symlink_metadata(d)?;
    if md.file_type().is_symlink() || !md.is_dir() {
        return Err(io::Error::from_raw_os_error(nix::libc::ENOTDIR));
    }
    fs::set_permissions(d, fs::Permissions::from_mode(0o700))
}

/// `os.open(path, O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC, mode)` + write.
pub(crate) fn write_new(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    f.write_all(data)
}

/// `shutil.which(name, path=path)`: the first `<dir>/<name>` that exists, is executable for us and is not a directory.
pub fn which(name: &str, path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let mut seen = BTreeSet::new();
    for dir in path.split(':') {
        if !seen.insert(dir.to_string()) {
            continue;
        }
        let cand = if dir.is_empty() {
            name.to_string()
        } else {
            hub_core::py::os::join(dir, name)
        };
        let p = Path::new(&cand);
        if p.exists() && !p.is_dir() && nix::unistd::access(p, nix::unistd::AccessFlags::X_OK).is_ok() {
            return Some(cand);
        }
    }
    None
}

// ---- report summaries ----------------------------------------------------------------------------------------------

/// `summarize_tier1(d)`: the fields the UI shows, from a tier1 JSON report. The judge score counts only when the judge
/// actually ran.
pub fn summarize_tier1(d: &Value) -> Value {
    let q = list(d.get("quality_summary"));
    let q0 = dict(q.first());
    let rub = dict(d.get("rubric_eval"));
    let sev = dict(d.get("severity_counts"));
    let status = d.get("overall_status").and_then(Value::as_str);
    let judge_status = rub.get("execution_status");
    let grade = q0.get("grade").and_then(Value::as_str);
    let mut severity = Map::new();
    for k in SEVERITIES {
        severity.insert(k.into(), int_value(sev.get(k)));
    }
    let incomplete = match d.get("incomplete_scans") {
        Some(Value::Array(a)) => a
            .iter()
            .take(10)
            .map(|x| Value::String(take_chars(&pystr(x), 80).to_string()))
            .collect(),
        _ => Vec::new(),
    };
    json!({
        "status": status.filter(|s| ["passed", "failed", "incomplete"].contains(s)),
        "quality_score": num(q0.get("overall_score")),
        "grade": grade.map(|g| take_chars(g, 4).to_string()),
        "judge_status": judge_status.and_then(Value::as_str).map(|s| take_chars(s, 40).to_string()),
        "judge_score": if is_str(judge_status, "succeeded") { num(rub.get("overall_score")) } else { None },
        "errors": int_value(d.get("total_errors")),
        "warnings": int_value(d.get("total_warnings")),
        "severity": severity,
        "incomplete": incomplete,
    })
}

fn sev_order(sev: &str) -> Option<usize> {
    match sev {
        "critical" => Some(0),
        "high" => Some(1),
        "medium" => Some(2),
        _ => None,
    }
}

/// `str(f.get("severity", "")).lower()`.
fn severity_of(f: &Map<String, Value>) -> String {
    get_str(f, "severity", "").to_lowercase()
}

/// `top_findings(d, limit)`: (medium-and-above findings, most severe first, at most `limit`; their total count).
pub fn top_findings(d: &Value, limit: usize) -> (Vec<Value>, usize) {
    let mut rows: Vec<(usize, Value)> = Vec::new();
    for r in list(d.get("results")) {
        let r = dict(Some(r));
        for f in list(r.get("findings")) {
            let f = dict(Some(f));
            let sev = severity_of(f);
            let Some(o) = sev_order(&sev) else { continue };
            let msg = first_truthy(f, &["message", "title", "check"]);
            let validator = {
                let v = first_truthy(r, &["validator"]);
                if v.is_empty() { "?".to_string() } else { v }
            };
            rows.push((
                o,
                json!({"severity": sev, "validator": take_chars(&validator, 100), "message": take_chars(&msg, 300)}),
            ));
        }
    }
    let total = rows.len();
    rows.sort_by_key(|(o, _)| *o);
    (rows.into_iter().take(limit).map(|(_, v)| v).collect(), total)
}

/// `is_security_validator(name)`: every validator that is not a publication/quality one (unknown ones included).
pub fn is_security_validator(name: &str) -> bool {
    !NON_SECURITY_VALIDATORS.contains(&name)
}

fn validator_name(r: &Map<String, Value>) -> String {
    let v = first_truthy(r, &["validator"]);
    take_chars(if v.is_empty() { "?" } else { &v }, 100).to_string()
}

/// `classify_partial_scan(report)`: SkillSpector's static JSON report -> `{reason: unresolved_references, ...}` when its
/// analysis is `partial` ONLY because of local references the skill does not bundle; None for anything else.
pub fn classify_partial_scan(report: &Value) -> Option<Value> {
    let ac = dict(report.get("analysis_completeness"));
    let meta = dict(report.get("metadata"));
    let exceptions = ac.get("ledger_exceptions");
    let analyzers = ac.get("analyzer_statuses");
    let counts: Vec<Option<&Value>> = [
        "total_components",
        "scanned_components",
        "fully_inspected_files",
        "partially_inspected_files",
        "entirely_uninspected_files",
    ]
    .iter()
    .map(|k| ac.get(*k))
    .collect();
    let all_int = counts.iter().all(|c| is_int(*c));
    let c = |i: usize| counts[i].and_then(Value::as_i64);
    let coverage_100 = ac
        .get("coverage_percent")
        .is_some_and(|v| v.is_number() && v.as_f64() == Some(100.0));
    let ok = report.get("execution_successful") == Some(&Value::Bool(true))
        && ac.get("execution_successful") == Some(&Value::Bool(true))
        && is_str(ac.get("status"), "partial")
        && ac.get("is_complete") == Some(&Value::Bool(false))
        && all_int
        && c(0) == c(1)
        && c(1) == c(2)
        && c(3) == Some(0)
        && c(4) == Some(0)
        && coverage_100
        && ac.get("limitations") == Some(&Value::Array(Vec::new()))
        && exceptions.and_then(Value::as_array).is_some_and(|a| !a.is_empty())
        && analyzers.and_then(Value::as_array).is_some_and(|a| !a.is_empty());
    if !ok {
        return None;
    }
    let mut refs = Vec::new();
    for e in list(exceptions) {
        let e = dict(Some(e));
        if e.get("fatal") != Some(&Value::Bool(false))
            || !is_str(e.get("phase"), "reference_resolution")
            || !is_str(e.get("reason_code"), "reference_missing")
            || !is_str(e.get("outcome"), "partial")
        {
            return None;
        }
        let path = first_truthy(e, &["path"]);
        refs.push(json!({"path": take_chars(if path.is_empty() { "?" } else { &path }, 200), "line": int_value(e.get("start_line"))}));
    }
    let llm_ids: BTreeSet<String> = list(dict(meta.get("llm_provenance")).get("analyzers"))
        .iter()
        .map(|x| get_str(dict(Some(x)), "analyzer_id", "None"))
        .collect();
    for a in list(analyzers) {
        let a = dict(Some(a));
        let status = a.get("status");
        let llm_off = is_str(status, "disabled")
            && is_str(a.get("reason_code"), "disabled_by_configuration")
            && meta.get("llm_requested") == Some(&Value::Bool(false))
            && llm_ids.contains(&get_str(a, "analyzer_id", "None"));
        if !(is_str(status, "completed") || is_str(status, "not_applicable")) && !llm_off {
            return None;
        }
    }
    let version = meta
        .get("skillspector_version")
        .and_then(Value::as_str)
        .map(|v| take_chars(v, 20).to_string());
    let count = refs.len();
    refs.truncate(20);
    Some(json!({"reason": UNRESOLVED_REFERENCES, "count": count, "references": refs, "skillspector_version": version}))
}

/// `validator_summaries(d)`: per validator {validator, status, severity counts}.
pub fn validator_summaries(d: &Value) -> Value {
    let mut out = Vec::new();
    for r in list(d.get("results")).iter().take(64) {
        let r = dict(Some(r));
        let mut counts: IndexMap<&str, i64> = SEVERITIES.iter().map(|s| (*s, 0)).collect();
        for f in list(r.get("findings")) {
            let sev = severity_of(dict(Some(f)));
            if let Some(n) = counts.get_mut(sev.as_str()) {
                *n += 1;
            }
        }
        let st = r
            .get("status")
            .and_then(Value::as_str)
            .map_or_else(|| "?".to_string(), |s| take_chars(s, 20).to_string());
        let severity: Map<String, Value> = counts
            .into_iter()
            .map(|(k, v)| (k.to_string(), Value::from(v)))
            .collect();
        out.push(json!({"validator": validator_name(r), "status": st, "severity": severity}));
    }
    Value::Array(out)
}

/// `security_findings(d, which, limit=30)`: critical/high/medium findings of SECURITY validators (most severe first).
pub fn security_findings(d: &Value, which: &str) -> Value {
    let mut rows: Vec<(usize, Value)> = Vec::new();
    for r in list(d.get("results")) {
        let r = dict(Some(r));
        let name = validator_name(r);
        if !is_security_validator(&name) {
            continue;
        }
        for f in list(r.get("findings")) {
            let f = dict(Some(f));
            let sev = severity_of(f);
            if let Some(o) = sev_order(&sev) {
                let msg = first_truthy(f, &["message", "title", "check"]);
                rows.push((
                    o,
                    json!({"pass": which, "validator": name, "severity": sev, "message": take_chars(&msg, 200)}),
                ));
            }
        }
    }
    rows.sort_by_key(|(o, _)| *o);
    Value::Array(rows.into_iter().take(30).map(|(_, v)| v).collect())
}

/// `security_severity(rec)`: critical/high counts over SECURITY validators of a run record (static and LLM pass, per
/// validator the larger of the two); None for records without per-validator summaries.
pub fn security_severity(rec: &Value) -> Option<Value> {
    if !rec.get("validators").is_some_and(Value::is_array) {
        return None;
    }
    let mut best: IndexMap<String, (i64, i64)> = IndexMap::new();
    for key in ["validators", "validators_llm"] {
        for v in list(rec.get(key)) {
            let v = dict(Some(v));
            let name = {
                let n = first_truthy(v, &["validator"]);
                if n.is_empty() { "?".to_string() } else { n }
            };
            if !is_security_validator(&name) {
                continue;
            }
            let sev = dict(v.get("severity"));
            let cur = best.entry(name).or_insert((0, 0));
            cur.0 = cur.0.max(int(sev.get("critical")).unwrap_or(0));
            cur.1 = cur.1.max(int(sev.get("high")).unwrap_or(0));
        }
    }
    let crit: i64 = best.values().map(|b| b.0).sum();
    let high: i64 = best.values().map(|b| b.1).sum();
    Some(json!({"critical": crit, "high": high}))
}

/// `parse_subject(subject)`: ("hub", skill) or (client, key); fullmatch only; a hub skill must be a valid content id.
pub fn parse_subject(subject: &str) -> HubResult<(String, String)> {
    let bad =
        || Problem::bad_request("invalid subject: expected hub:<skill> or <client>:<key>").code("invalid_subject");
    let caps = SUBJECT_RE.captures(subject).ok_or_else(bad)?;
    let src = caps.get(1).map_or("", |m| m.as_str()).to_string();
    let key = caps.get(2).map_or("", |m| m.as_str()).to_string();
    if src == "hub" && !valid_id(&key) {
        return Err(Problem::bad_request("invalid skill name").code("invalid_subject"));
    }
    Ok((src, key))
}

/// `normalize_subject(value)`: a bare skill name means `hub:<name>`; anything with a colon is a full subject.
pub fn normalize_subject(value: &str) -> HubResult<String> {
    if value.contains(':') {
        let (src, key) = parse_subject(value)?;
        return Ok(format!("{src}:{key}"));
    }
    if !valid_id(value) {
        return Err(Problem::bad_request("invalid skill name").code("invalid_id"));
    }
    Ok(format!("hub:{value}"))
}

/// `combine_verdicts(tier1, tier2)`: Tier 2 can only make a result worse; a tier1 error stays an error; a tier2 without
/// a report counts as incomplete.
pub fn combine_verdicts(tier1: &str, tier2: &str) -> String {
    let Some(r1) = verdict_rank(tier1) else {
        return "error".into();
    };
    let t2 = if verdict_rank(tier2).is_some() {
        tier2
    } else {
        "incomplete"
    };
    let r2 = verdict_rank(t2).unwrap_or(1);
    if r2 > r1 { t2.into() } else { tier1.into() }
}

// ---- the global one-at-a-time lock ---------------------------------------------------------------------------------

/// `_EvalLock`: a thread lock (this process) plus an flock on a file (other processes, e.g. `hubctl` next to the server).
struct EvalLock {
    path: PathBuf,
    held: Mutex<bool>,
    cv: Condvar,
    flock: Mutex<Option<nix::fcntl::Flock<File>>>,
}

impl EvalLock {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            held: Mutex::new(false),
            cv: Condvar::new(),
            flock: Mutex::new(None),
        }
    }

    fn take_thread_lock(&self, wait: Option<Duration>) -> bool {
        let g = self.held.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut g = match wait {
            None => g,
            Some(d) => {
                self.cv
                    .wait_timeout_while(g, d, |held| *held)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0
            }
        };
        if *g {
            return false;
        }
        *g = true;
        true
    }

    fn release_thread_lock(&self) {
        *self.held.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = false;
        self.cv.notify_one();
    }

    /// A short wait absorbs a concurrent status probe; a real evaluation holds the lock far longer.
    fn try_acquire(&self) -> io::Result<bool> {
        if !self.take_thread_lock(Some(Duration::from_millis(500))) {
            return Ok(false);
        }
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&self.path);
        let mut f = match f {
            Ok(f) => f,
            Err(e) => {
                self.release_thread_lock();
                return Err(e);
            }
        };
        for attempt in 0..10 {
            match nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusiveNonblock) {
                Ok(l) => {
                    *self.flock.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(l);
                    return Ok(true);
                }
                Err((back, _)) => {
                    f = back;
                    if attempt == 9 {
                        drop(f);
                        self.release_thread_lock();
                        return Ok(false);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        self.release_thread_lock();
        Ok(false)
    }

    fn release(&self) {
        if let Some(l) = self
            .flock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = l.unlock();
        }
        self.release_thread_lock();
    }

    /// True when another PROCESS holds the file lock (a probe that never keeps it). Taking the thread lock first means a
    /// probe cannot make an evaluation in this process fail; while this process evaluates, the answer is False.
    fn held_elsewhere(&self) -> bool {
        if !self.take_thread_lock(None) {
            return false;
        }
        let res = (|| {
            let Ok(f) = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(&self.path)
            else {
                return false;
            };
            // dropping the Flock (or the returned file) releases a lock the probe obtained
            nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusiveNonblock).is_err()
        })();
        self.release_thread_lock();
        res
    }
}

// ---- jobs and failures ---------------------------------------------------------------------------------------------

/// One queued evaluation (`Job`).
#[derive(Debug)]
pub struct Job {
    pub subject: String,
    pub tiers: String,
    pub force: bool,
    pub actor: String,
    pub run_id: String,
    pub cancelled: AtomicBool,
}

/// `_Prepared`.
struct Prepared {
    subject: String,
    source: String,
    /// the skill name / observed key: the directory name of the private copy
    name: String,
    tiers: String,
    run_id: String,
    launcher: String,
    exe: String,
    snap: Snapshot,
    hash: String,
    run_dir: PathBuf,
    started_at: f64,
    version: Option<String>,
    job: Option<Arc<Job>>,
}

/// What can go wrong inside the service: a ProblemError, an OSError (Python's exception escapes; the type name is kept
/// for the queue's error list) or a cancellation.
#[derive(Debug)]
enum Fail {
    P(Problem),
    Os(io::Error),
    Cancelled,
}

impl From<Problem> for Fail {
    fn from(p: Problem) -> Self {
        Fail::P(p)
    }
}

impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Self {
        Fail::Os(e)
    }
}

impl From<ReadErr> for Fail {
    fn from(e: ReadErr) -> Self {
        match e {
            ReadErr::Os(e) => Fail::Os(e),
            ReadErr::TooBig => Fail::Os(io::Error::other("too big")),
        }
    }
}

impl Fail {
    /// At a public boundary: a ProblemError as is, anything else as Python's unhandled exception (500).
    fn into_problem(self) -> Problem {
        match self {
            Fail::P(p) => p,
            Fail::Os(_) | Fail::Cancelled => Problem::internal(),
        }
    }
    fn type_name(&self) -> String {
        match self {
            Fail::P(_) => "ProblemError".into(),
            Fail::Os(e) => hub_core::py::os::error_name(e).into(),
            Fail::Cancelled => "_Cancelled".into(),
        }
    }
}

fn busy() -> Problem {
    Problem::conflict("another skill evaluation is running; wait for it to finish, then try again")
        .code("evaluation_running")
}

fn bad_tiers() -> Problem {
    Problem::unprocessable(format!("tiers must be one of {}", TIERS.join(", "))).code("bad_tiers")
}

fn not_installed() -> Problem {
    Problem::unavailable(
        "SkillEvaluator is not installed (neither skillevaluator-local nor skillevaluator is on the hub's PATH or in ~/.local/bin)",
    )
    .code("evaluator_missing")
    .with("fix", Value::String(INSTALL_HINT.into()))
}

// ---- the service ---------------------------------------------------------------------------------------------------

/// `which(name, path)` seam.
pub type WhichFn = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;
/// `clock()` seam.
pub type ClockFn = Arc<dyn Fn() -> f64 + Send + Sync>;
/// `on_event(actor, action, params)` seam.
pub type EventFn = Arc<dyn Fn(&str, &str, &Value) + Send + Sync>;

/// What Python's tests reassign on a live service (`s.runner = ...`, `s.which = ...`, `s.keep = 3`, ...).
struct Tunables {
    runner: Arc<dyn EvalRunner>,
    which: Option<WhichFn>,
    environ: Option<Vec<(String, String)>>,
    home: Option<PathBuf>,
    max_files: usize,
    max_bytes: u64,
    keep: usize,
    timeout: f64,
    clock: Option<ClockFn>,
    eval_rev: i64,
    on_event: Option<EventFn>,
    gate_min_tiers: Option<String>,
}

#[derive(Default)]
struct State {
    running: Option<Value>,
    queue: VecDeque<Arc<Job>>,
    queued: HashMap<String, Arc<Job>>,
    current: Option<Arc<Job>>,
    worker: Option<std::thread::JoinHandle<()>>,
    stop: bool,
    finished: i64,
    errors: i64,
    last_errors: VecDeque<Value>,
}

type IndexEntry = ((i128, u64, u64), Value, HashMap<String, Value>);
type SigRow = (String, u64, i128, u64, i128);
type HashCache = HashMap<String, (Vec<SigRow>, Option<String>)>;

struct Inner {
    root: PathBuf,
    content_dir: PathBuf,
    observed_root: Option<PathBuf>,
    hub: Weak<Hub>,
    tun: RwLock<Tunables>,
    lock: EvalLock,
    st: Mutex<State>,
    cv: Condvar,
    recovered: AtomicBool,
    version: Mutex<(f64, Option<String>, Option<String>)>,
    hashes: Mutex<HashCache>,
    index_cache: Mutex<HashMap<String, IndexEntry>>,
}

pub struct SkillEvalService {
    pub root: PathBuf,
    pub content_dir: PathBuf,
    pub hub: Weak<Hub>,
    pub runner: Option<Arc<dyn EvalRunner>>,
    pub max_files: usize,
    pub max_bytes: u64,
    pub observed_root: PathBuf,
    inner: Arc<Inner>,
}

fn ns_time(secs: i64, nsec: i64) -> i128 {
    i128::from(secs) * 1_000_000_000 + i128::from(nsec)
}

impl SkillEvalService {
    /// `SkillEvalService(root, content_dir, runner=, guard=hub.mutating, skill_names=, on_event=audit.event,
    /// max_files=, max_bytes=, observed_root=, gate_min_tiers=)`: the callbacks are reached through `hub` (a dead or
    /// empty Weak gives Python's standalone defaults: no guard, skill dirs, no events, gate tiers "static").
    pub fn new(
        root: &Path,
        content_dir: &Path,
        hub: Weak<Hub>,
        runner: Option<Arc<dyn EvalRunner>>,
        max_files: usize,
        max_bytes: u64,
        observed_root: &Path,
    ) -> Self {
        let active: Arc<dyn EvalRunner> = runner.clone().unwrap_or_else(|| Arc::new(RealEvalRunner::default()));
        let inner = Arc::new(Inner {
            root: root.to_path_buf(),
            content_dir: content_dir.to_path_buf(),
            observed_root: Some(observed_root.to_path_buf()),
            hub: hub.clone(),
            tun: RwLock::new(Tunables {
                runner: active,
                which: None,
                environ: None,
                home: None,
                max_files,
                max_bytes,
                keep: KEEP_RUNS,
                timeout: RUN_TIMEOUT_S,
                clock: None,
                eval_rev: EVAL_REV,
                on_event: None,
                gate_min_tiers: None,
            }),
            lock: EvalLock::new(root.join(".lock")),
            st: Mutex::new(State::default()),
            cv: Condvar::new(),
            recovered: AtomicBool::new(false),
            version: Mutex::new((0.0, None, None)),
            hashes: Mutex::new(HashMap::new()),
            index_cache: Mutex::new(HashMap::new()),
        });
        Self {
            root: root.to_path_buf(),
            content_dir: content_dir.to_path_buf(),
            hub,
            runner,
            max_files,
            max_bytes,
            observed_root: observed_root.to_path_buf(),
            inner,
        }
    }

    // -- test seams (Python reassigns these attributes) ----------------------------------------------------------------
    fn tun_mut(&self) -> std::sync::RwLockWriteGuard<'_, Tunables> {
        self.inner
            .tun
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn set_runner(&self, r: Arc<dyn EvalRunner>) {
        self.tun_mut().runner = r;
    }
    /// `which(name, path)`; None = `shutil.which`.
    pub fn set_which(&self, w: Option<WhichFn>) {
        self.tun_mut().which = w;
    }
    /// The environment the service reads (PATH, HOME, SKILL_EVAL_*); None = the process environment.
    pub fn set_environ(&self, e: Option<Vec<(String, String)>>) {
        self.tun_mut().environ = e;
    }
    pub fn set_home(&self, h: Option<PathBuf>) {
        self.tun_mut().home = h;
    }
    pub fn set_keep(&self, keep: usize) {
        self.tun_mut().keep = keep;
    }
    pub fn set_limits(&self, max_files: usize, max_bytes: u64) {
        let mut t = self.tun_mut();
        t.max_files = max_files;
        t.max_bytes = max_bytes;
    }
    pub fn set_timeout(&self, timeout: f64) {
        self.tun_mut().timeout = timeout;
    }
    pub fn set_clock(&self, c: Option<ClockFn>) {
        self.tun_mut().clock = c;
    }
    /// `se.EVAL_REV` (monkeypatched in Python's tests).
    pub fn set_eval_rev(&self, rev: i64) {
        self.tun_mut().eval_rev = rev;
    }
    /// `on_event`; None = the hub's audit log (when there is a hub).
    pub fn set_on_event(&self, f: Option<EventFn>) {
        self.tun_mut().on_event = f;
    }
    /// `gate_min_tiers`; None = the hub setting `eval_gate_min_tiers` (else "static").
    pub fn set_gate_min_tiers(&self, t: Option<String>) {
        self.tun_mut().gate_min_tiers = t;
    }
    /// `self._version = (0.0, None, None)`.
    pub fn reset_version_cache(&self) {
        *self
            .inner
            .version
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (0.0, None, None);
    }

    // -- environment and launcher ----------------------------------------------------------------------------------------
    pub fn search_path(&self) -> String {
        self.inner.search_path()
    }
    pub fn child_env(&self) -> Vec<(String, String)> {
        self.inner.child_env()
    }
    pub fn env_names(&self) -> Vec<String> {
        self.inner.env_names()
    }
    /// (bare name, resolved path) of the first launcher found.
    pub fn find_launcher(&self) -> Option<(String, String)> {
        self.inner.find_launcher()
    }

    /// GET /skill-evaluator: launcher and version (the version probe is cached). No secret values.
    pub fn status(&self) -> HubResult<Value> {
        self.inner.status().map_err(Fail::into_problem)
    }

    /// For doctor: launcher lookup only (never a subprocess); the version only if a status call already probed it.
    pub fn doctor_info(&self) -> HubResult<Value> {
        let i = &self.inner;
        let found = i.find_launcher();
        let (_, exe, ver) = i
            .version
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let version = match &found {
            Some((_, p)) if exe.as_deref() == Some(p.as_str()) => ver,
            _ => None,
        };
        Ok(json!({
            "installed": found.is_some(),
            "launcher": found.map(|f| f.0),
            "version": version,
            "env_names": i.env_names(),
        }))
    }

    /// `snapshot(subject)`: exactly the files that will be evaluated.
    pub fn snapshot(&self, subject: &str) -> HubResult<Snapshot> {
        self.inner.snapshot(subject).map_err(Fail::into_problem)
    }

    /// The content hash of a subject's current files (None if absent/unreadable).
    pub fn current_hash(&self, subject: &str) -> Option<String> {
        self.inner.current_hash(subject)
    }

    pub fn observed_entry(&self, client: &str, key: &str) -> Option<Value> {
        self.inner.observed_entry(client, key)
    }

    pub fn subject_exists(&self, subject: &str) -> HubResult<bool> {
        self.inner.subject_exists(subject)
    }

    /// Stored run records of a subject, newest first.
    pub fn records(&self, subject: &str, limit: usize) -> Vec<Value> {
        self.inner.records(subject, limit)
    }

    /// `_freshness(latest, content_hash)`: none | current | stale.
    pub fn freshness(&self, latest: Option<&Value>, content_hash: Option<&str>) -> String {
        freshness(latest, content_hash).into()
    }

    pub fn running(&self) -> Option<Value> {
        self.inner.running()
    }

    pub fn queued_subjects(&self) -> BTreeSet<String> {
        self.inner.state().queued.keys().cloned().collect()
    }

    /// Drop stored runs of observed skills that are no longer in the client's observation (never the running one).
    pub fn prune_observed(&self, client: &str, keep_keys: &BTreeSet<String>) {
        if !valid_id(client) {
            return;
        }
        let base = self.inner.root.join(OBSERVED_DIR).join(client);
        let cur = self.inner.state().current.as_ref().map(|j| j.subject.clone());
        let Ok(rd) = fs::read_dir(&base) else { return };
        for e in rd.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let p = e.path();
            if KEY_RE.is_match(&name)
                && !keep_keys.contains(&name)
                && p.is_dir()
                && !is_symlink(&p)
                && cur.as_deref() != Some(format!("{client}:{name}").as_str())
            {
                rmtree(&p);
            }
        }
    }

    /// `list_latest()`: latest evaluation per skill of the working tree (library badges). Never starts a process.
    pub fn list_latest(&self) -> Vec<Value> {
        let i = &self.inner;
        let run = i.running();
        let mut out = Vec::new();
        for skill in i.skill_names() {
            if !valid_id(&skill) {
                continue;
            }
            let subject = format!("hub:{skill}");
            let latest = i.records(&subject, 1).into_iter().next();
            let current = if latest.is_some() {
                i.current_hash(&subject)
            } else {
                None
            };
            out.push(json!({
                "skill": skill,
                "subject": subject,
                "state": freshness(latest.as_ref(), current.as_deref()),
                "latest": latest,
                "running": run.as_ref().is_some_and(|r| r.get("subject").and_then(Value::as_str) == Some(subject.as_str())),
            }));
        }
        out
    }

    pub fn detail(&self, subject: &str, limit: usize) -> HubResult<Value> {
        self.inner.detail(subject, limit).map_err(Fail::into_problem)
    }

    /// The Markdown report of one stored run, secret-shaped text redacted; a reused run shows its source's report.
    pub fn report(&self, subject: &str, run_id: &str) -> HubResult<Value> {
        self.inner.report(subject, run_id).map_err(Fail::into_problem)
    }

    /// `gate(content_hash)`: `{"status": missing|blocked|warn|ok, ...}` (fail closed; see the Python docstring).
    pub fn gate(&self, content_hash: &str) -> Value {
        self.inner.gate(content_hash)
    }

    /// Synchronous (`hubctl evaluate-skill`): one explicit skill is always re-run. 409 `evaluation_running` when another
    /// evaluation holds the lock.
    pub fn evaluate(&self, skill: &str, tiers: &str, actor: &str) -> HubResult<Value> {
        self.evaluate_force(skill, tiers, actor, true)
    }

    /// `evaluate(skill, tiers, actor, force)`.
    pub fn evaluate_force(&self, skill: &str, tiers: &str, actor: &str, force: bool) -> HubResult<Value> {
        let i = &self.inner;
        i.acquire().map_err(Fail::into_problem)?;
        let res = i
            .prepare(skill, tiers, None, None)
            .and_then(|p| i.run(&p, actor, force));
        i.lock.release();
        res.map_err(Fail::into_problem)
    }

    /// `hubctl evaluate --wait`: one lock for the whole batch, so a UI run cannot slip in between two subjects.
    pub fn evaluate_subjects(
        &self,
        subjects: &[String],
        tiers: &str,
        actor: &str,
        force: bool,
    ) -> HubResult<Vec<Value>> {
        if !TIERS.contains(&tiers) {
            return Err(bad_tiers());
        }
        let i = &self.inner;
        i.acquire().map_err(Fail::into_problem)?;
        let mut out = Vec::new();
        let mut res = Ok(());
        for s in subjects {
            match i.prepare(s, tiers, None, None).and_then(|p| i.run(&p, actor, force)) {
                Ok(rec) => out.push(rec),
                Err(Fail::P(p)) => out.push(json!({"skill": s, "subject": s, "tiers": tiers, "verdict": "error",
                                                    "error": p.detail, "run_id": null})),
                Err(e) => {
                    res = Err(e.into_problem());
                    break;
                }
            }
        }
        i.lock.release();
        res.map(|()| out)
    }

    /// `hubctl evaluate-skills`: every named hub skill, always re-run.
    pub fn evaluate_many(&self, skills: &[String], tiers: &str, actor: &str) -> HubResult<Vec<Value>> {
        let subjects: Vec<String> = skills
            .iter()
            .map(|s| {
                if valid_id(s) {
                    normalize_subject(s).unwrap_or_else(|_| s.clone())
                } else {
                    s.clone()
                }
            })
            .collect();
        self.evaluate_subjects(&subjects, tiers, actor, true)
    }

    /// Queue evaluations (FIFO, one at a time): `{"queued", "skipped", "run_ids"}`. Subjects already queued or running are
    /// skipped; a malformed subject refuses the whole request (400); one that does not exist is skipped `not_found`.
    pub fn enqueue(&self, subjects: &[String], tiers: &str, force: bool, actor: &str) -> HubResult<Value> {
        Inner::enqueue(&self.inner, subjects, tiers, force, actor)
    }

    /// POST /skills/{name}/evaluate (202): validate now, then enqueue ONE forced run; answers `queued` or `running`.
    pub fn start(&self, skill: &str, tiers: &str, actor: &str) -> HubResult<Value> {
        let i = &self.inner;
        let subject = normalize_subject(skill)?;
        if !i.subject_exists(&subject)? {
            return Err(Problem::not_found(format!("no such skill: {subject}")));
        }
        if !TIERS.contains(&tiers) {
            return Err(bad_tiers());
        }
        let (src, key) = parse_subject(&subject)?;
        if i.find_launcher().is_some() {
            if src == "hub" {
                let h = i.hub.upgrade();
                let _g = match &h {
                    Some(h) => Some(h.mutating()?),
                    None => None,
                };
                i.snapshot(&subject).map_err(Fail::into_problem)?;
            } else {
                i.snapshot(&subject).map_err(Fail::into_problem)?;
            }
        }
        let idle = {
            let st = i.state();
            st.current.is_none() && st.queue.is_empty()
        };
        let res = Inner::enqueue(i, std::slice::from_ref(&subject), tiers, true, actor)?;
        let reason = res
            .get("skipped")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|s| s.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let status = if reason.as_deref() == Some("running") || (reason.is_none() && idle) {
            "running"
        } else {
            "queued"
        };
        Ok(
            json!({"skill": key, "subject": subject, "tiers": tiers, "status": status,
                  "run_id": res.get("run_ids").and_then(|r| r.get(&subject)).cloned().unwrap_or(Value::Null),
                  "started_at": null}),
        )
    }

    pub fn queue(&self) -> HubResult<Value> {
        let i = &self.inner;
        let run = i.running();
        let st = i.state();
        let queued: Vec<Value> = st
            .queue
            .iter()
            .take(QUEUE_SHOWN)
            .map(|j| json!({"subject": j.subject, "tiers": j.tiers}))
            .collect();
        let running = run.map(|r| {
            let mut m = Map::new();
            for k in ["subject", "run_id", "tiers", "started_at", "elsewhere"] {
                m.insert(k.into(), r.get(k).cloned().unwrap_or(Value::Null));
            }
            Value::Object(m)
        });
        Ok(
            json!({"running": running, "queued": queued, "queued_total": st.queue.len(),
                  "finished_since_start": st.finished, "errors_since_start": st.errors,
                  "last_errors": st.last_errors.iter().cloned().collect::<Vec<_>>()}),
        )
    }

    /// Clear the queue and kill the running evaluation's process group.
    pub fn cancel(&self) -> HubResult<Value> {
        let i = &self.inner;
        let (n, cur) = {
            let mut st = i.state();
            let n = st.queue.len();
            st.queue.clear();
            st.queued.clear();
            let cur = st.current.clone();
            if let Some(c) = &cur {
                c.cancelled.store(true, Ordering::SeqCst);
            }
            i.cv.notify_all();
            (n, cur)
        };
        if cur.is_some() {
            i.runner().cancel();
        }
        Ok(json!({"cancelled_queued": n, "killed_running": cur.is_some()}))
    }

    /// Wait until the queue is empty and nothing is running (tests, shutdown).
    pub fn join(&self, timeout: Option<f64>) {
        self.inner.join(timeout);
    }

    /// Hub stopping: drop the queue, kill a running SkillEvaluator process group, give the worker a moment to record.
    pub fn shutdown(&self) {
        let i = &self.inner;
        {
            let mut st = i.state();
            st.stop = true;
            st.queue.clear();
            st.queued.clear();
            if let Some(c) = &st.current {
                c.cancelled.store(true, Ordering::SeqCst);
            }
            i.cv.notify_all();
        }
        i.runner().cancel();
        i.join(Some(5.0));
    }
}

/// `_freshness(rec, current)`.
fn freshness(rec: Option<&Value>, current: Option<&str>) -> &'static str {
    match rec {
        None => "none",
        Some(r) => {
            if current.is_some_and(|c| r.get("content_hash").and_then(Value::as_str) == Some(c)) {
                "current"
            } else {
                "stale"
            }
        }
    }
}

impl Inner {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn tun(&self) -> std::sync::RwLockReadGuard<'_, Tunables> {
        self.tun.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn runner(&self) -> Arc<dyn EvalRunner> {
        self.tun().runner.clone()
    }
    fn clock(&self) -> f64 {
        let c = self.tun().clock.clone();
        c.map_or_else(hub_core::common::now, |c| c())
    }
    fn environ(&self) -> Vec<(String, String)> {
        self.tun().environ.clone().unwrap_or_else(|| std::env::vars().collect())
    }
    fn env_get(environ: &[(String, String)], k: &str) -> Option<String> {
        environ.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }
    fn home(&self) -> PathBuf {
        self.tun().home.clone().unwrap_or_else(crate::config::home)
    }

    fn search_path(&self) -> String {
        let environ = self.environ();
        let path = Self::env_get(&environ, "PATH")
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "/usr/bin:/bin".into());
        let mut parts: Vec<String> = path.split(':').filter(|p| !p.is_empty()).map(str::to_string).collect();
        let local_bin = self.home().join(".local").join("bin").to_string_lossy().into_owned();
        if !parts.contains(&local_bin) {
            parts.push(local_bin);
        }
        parts.join(":")
    }

    /// The hub's minimal subprocess environment plus SKILL_EVAL_* / SKILLEVAL_* (for SkillEvaluator only).
    fn child_env(&self) -> Vec<(String, String)> {
        let environ = self.environ();
        let home = Self::env_get(&environ, "HOME")
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| self.home().to_string_lossy().into_owned());
        let mut env = vec![
            ("PATH".to_string(), self.search_path()),
            ("HOME".to_string(), home),
            ("LC_ALL".to_string(), "C".to_string()),
        ];
        for (k, v) in environ {
            if ENV_PREFIXES.iter().any(|p| k.starts_with(p)) {
                env.retain(|(n, _)| *n != k);
                env.push((k, v));
            }
        }
        env
    }

    fn env_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .environ()
            .into_iter()
            .map(|(k, _)| k)
            .filter(|k| ENV_PREFIXES.iter().any(|p| k.starts_with(p)))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    fn which(&self, name: &str, path: &str) -> Option<String> {
        let w = self.tun().which.clone();
        match w {
            Some(f) => f(name, path),
            None => which(name, path),
        }
    }

    fn find_launcher(&self) -> Option<(String, String)> {
        let path = self.search_path();
        LAUNCHERS
            .iter()
            .find_map(|name| self.which(name, &path).map(|exe| ((*name).to_string(), exe)))
    }

    fn ensure_root(&self) -> io::Result<()> {
        mkdir_private(&self.root)
    }

    fn probe_version(&self, exe: &str) -> Result<Option<String>, Fail> {
        self.ensure_root()?;
        let r = self.runner().run(
            &[exe.to_string(), "--version".to_string()],
            &self.root,
            &self.child_env(),
            VERSION_TIMEOUT_S,
        );
        if r.returncode != 0 {
            return Ok(None);
        }
        Ok(VERSION
            .captures(&r.stdout)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string()))
    }

    fn version_for(&self, found: &(String, String)) -> Result<Option<String>, Fail> {
        let (ts, exe, ver) = self
            .version
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if exe.as_deref() != Some(found.1.as_str()) || self.clock() - ts > VERSION_TTL_S {
            let v = self.probe_version(&found.1)?;
            *self.version.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                (self.clock(), Some(found.1.clone()), v.clone());
            return Ok(v);
        }
        Ok(ver)
    }

    fn status(&self) -> Result<Value, Fail> {
        let found = self.find_launcher();
        let version = match &found {
            Some(f) => self.version_for(f)?,
            None => None,
        };
        let (keep, timeout) = {
            let t = self.tun();
            (t.keep, t.timeout)
        };
        let tiers: Vec<Value> = TIERS.iter().map(|t| json!({"id": t, "help": tier_help(t)})).collect();
        Ok(json!({
            "installed": found.is_some(),
            "launcher": found.as_ref().map(|f| f.0.clone()),
            "version": version,
            "wrapper": found.as_ref().is_some_and(|f| f.0 == "skillevaluator-local"),
            "env_names": self.env_names(),
            "install_hint": INSTALL_HINT,
            "tiers": tiers,
            "keep_runs": keep,
            "timeout_s": timeout,
            "running": self.running(),
            "checks": SAFE_CHECKS.split(',').collect::<Vec<_>>(),
        }))
    }

    // -- subjects: hub skills ------------------------------------------------------------------------------------------
    fn skills_root(&self) -> PathBuf {
        self.content_dir.join("skills")
    }

    fn skill_dirs(&self) -> Vec<String> {
        let d = self.skills_root();
        if !d.is_dir() || is_symlink(&d) {
            return Vec::new();
        }
        let Ok(rd) = fs::read_dir(&d) else {
            return Vec::new();
        };
        let mut out: Vec<String> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_string();
                let p = e.path();
                (valid_id(&name) && p.is_dir() && !is_symlink(&p)).then_some(name)
            })
            .collect();
        out.sort();
        out
    }

    /// `skill_names()`: the hub's `work.names("skill")`, else the skill directories.
    fn skill_names(&self) -> Vec<String> {
        if let Some(h) = self.hub.upgrade()
            && let Ok(names) = h.work.names("skill")
        {
            return names;
        }
        self.skill_dirs()
    }

    fn gate_min_tiers(&self) -> String {
        if let Some(t) = self.tun().gate_min_tiers.clone() {
            return t;
        }
        match self.hub.upgrade() {
            Some(h) => h.setting_str("eval_gate_min_tiers"),
            None => "static".into(),
        }
    }

    fn skill_dir(&self, skill: &str) -> HubResult<PathBuf> {
        if !valid_id(skill) {
            return Err(Problem::bad_request("invalid skill name").code("invalid_id"));
        }
        let root = self.skills_root();
        let d = root.join(skill);
        let nf = || Problem::not_found(format!("no such skill: {skill}"));
        if is_symlink(&root) {
            return Err(nf());
        }
        match fs::symlink_metadata(&d) {
            Ok(m) if m.is_dir() => {}
            _ => return Err(nf()),
        }
        let real = hub_core::py::os::realpath(&d);
        if real.parent() != Some(hub_core::py::os::realpath(&root).as_path()) {
            return Err(nf());
        }
        Ok(d)
    }

    fn read_tree(&self, base: &Path, label: &str, mut snap: Snapshot, skip_sidecar: bool) -> Result<Snapshot, Fail> {
        let (max_files, max_bytes) = {
            let t = self.tun();
            (t.max_files, t.max_bytes)
        };
        let mut total: u64 = 0;
        for (rel, full) in walk_skill(base, skip_sidecar) {
            let got = match read_regular(&full, max_bytes.saturating_sub(total)) {
                Ok(g) => g,
                Err(ReadErr::TooBig) => {
                    return Err(Fail::P(
                        Problem::unprocessable(format!(
                            "skill {label} is larger than {max_bytes} bytes; not evaluated"
                        ))
                        .code("skill_too_large"),
                    ));
                }
                Err(ReadErr::Os(e)) => return Err(Fail::Os(e)),
            };
            let Some((data, mode)) = got else { continue };
            if snap.files.len() >= max_files {
                return Err(Fail::P(
                    Problem::unprocessable(format!("skill {label} has more than {max_files} files; not evaluated"))
                        .code("skill_too_large"),
                ));
            }
            total += data.len() as u64;
            snap.modes.insert(rel.clone(), mode);
            snap.files.insert(rel, data);
        }
        if !snap.files.contains_key("SKILL.md") {
            return Err(Fail::P(
                Problem::unprocessable(format!("skill {label} has no SKILL.md")).code("skill_invalid"),
            ));
        }
        Ok(snap)
    }

    fn snapshot(&self, subject: &str) -> Result<Snapshot, Fail> {
        let subject = normalize_subject(subject)?;
        let (src, key) = parse_subject(&subject)?;
        if src == "hub" {
            let base = self.skill_dir(&key)?;
            let snap = Snapshot {
                skill: key.clone(),
                subject,
                path: key.clone(),
                origin: "hub".into(),
                ..Snapshot::default()
            };
            return self.read_tree(&base, &key, snap, true);
        }
        let nf = || Fail::P(Problem::not_found(format!("no such observed skill: {subject}")));
        let ent = self.observed_entry(&src, &key).ok_or_else(nf)?;
        let Some(oroot) = &self.observed_root else {
            return Err(nf());
        };
        let base = oroot.join(&src).join("skills").join(&key);
        match fs::symlink_metadata(&base) {
            Ok(m) if m.is_dir() => {}
            _ => return Err(nf()),
        }
        let path = ent
            .get("path")
            .filter(|v| truthy(Some(v)))
            .map_or_else(|| key.clone(), pystr);
        let origin = ent
            .get("origin")
            .filter(|v| truthy(Some(v)))
            .map_or_else(|| "client".to_string(), pystr);
        let snap = Snapshot {
            skill: key,
            subject: subject.clone(),
            path,
            origin,
            ..Snapshot::default()
        };
        self.read_tree(&base, &subject, snap, false)
    }

    fn current_hash(&self, skill: &str) -> Option<String> {
        let subject = normalize_subject(skill).ok()?;
        let (src, key) = parse_subject(&subject).ok()?;
        if src != "hub" {
            let ent = self.observed_entry(&src, &key)?;
            return ent
                .get("content_hash")
                .and_then(Value::as_str)
                .filter(|h| HASH_RE.is_match(h))
                .map(str::to_string);
        }
        let base = self.skill_dir(&key).ok()?;
        let mut sig: Vec<SigRow> = Vec::new();
        for (rel, full) in walk_skill(&base, true) {
            let st = fs::symlink_metadata(&full).ok()?;
            if st.file_type().is_file() {
                sig.push((
                    rel,
                    st.size(),
                    ns_time(st.mtime(), st.mtime_nsec()),
                    st.ino(),
                    ns_time(st.ctime(), st.ctime_nsec()),
                ));
            }
        }
        if let Some((s, h)) = self
            .hashes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            && *s == sig
        {
            return h.clone();
        }
        let h2 = self.snapshot(&subject).ok().map(|s| s.content_hash());
        self.hashes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, (sig, h2.clone()));
        h2
    }

    // -- subjects: observed skills (read-only view of observe.rs's store) -------------------------------------------------
    fn index_map(&self, client: &str) -> Option<(Value, HashMap<String, Value>)> {
        let oroot = self.observed_root.as_ref()?;
        if !valid_id(client) {
            return None;
        }
        let path = oroot.join(client).join("index.json");
        let st = fs::symlink_metadata(&path).ok()?;
        let sig = (ns_time(st.mtime(), st.mtime_nsec()), st.size(), st.ino());
        if let Some((s, idx, by_key)) = self
            .index_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(client)
            && *s == sig
        {
            return Some((idx.clone(), by_key.clone()));
        }
        let got = read_regular(&path, INDEX_CAP).ok()??;
        let idx = json_loads(&got.0)?;
        if !idx.is_object() {
            return None;
        }
        let mut by_key = HashMap::new();
        for s in list(idx.get("skills")) {
            if let Some(k) = s.get("key").and_then(Value::as_str)
                && s.is_object()
            {
                by_key.insert(k.to_string(), s.clone());
            }
        }
        self.index_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(client.to_string(), (sig, idx.clone(), by_key.clone()));
        Some((idx, by_key))
    }

    fn observed_entry(&self, client: &str, key: &str) -> Option<Value> {
        if !KEY_RE.is_match(key) {
            return None;
        }
        self.index_map(client).and_then(|(_, m)| m.get(key).cloned())
    }

    fn subject_exists(&self, subject: &str) -> HubResult<bool> {
        let (src, key) = parse_subject(subject)?;
        if src == "hub" {
            return Ok(self.skill_dir(&key).is_ok());
        }
        Ok(self.observed_entry(&src, &key).is_some())
    }

    // -- stored runs -------------------------------------------------------------------------------------------------------
    fn runs_dir(&self, subject: &str) -> HubResult<PathBuf> {
        let (src, key) = parse_subject(subject)?;
        Ok(if src == "hub" {
            self.root.join(key)
        } else {
            self.root.join(OBSERVED_DIR).join(src).join(key)
        })
    }

    fn run_dirs(&self, subject: &str) -> Vec<PathBuf> {
        let Ok(d) = self.runs_dir(subject) else {
            return Vec::new();
        };
        if !d.is_dir() || is_symlink(&d) {
            return Vec::new();
        }
        let Ok(rd) = fs::read_dir(&d) else {
            return Vec::new();
        };
        let mut dirs: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| RUN_ID_RE.is_match(n))
                    && p.is_dir()
                    && !is_symlink(p)
            })
            .collect();
        dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
        dirs
    }

    fn load_record(run_dir: &Path) -> Option<Value> {
        let got = read_regular(&run_dir.join(RECORD), RECORD_CAP).ok()??;
        let rec = json_loads(&got.0)?;
        let name = run_dir.file_name()?.to_str()?;
        if !rec.is_object() || rec.get("run_id").and_then(Value::as_str) != Some(name) {
            return None;
        }
        Some(rec)
    }

    fn records(&self, skill: &str, limit: usize) -> Vec<Value> {
        let Ok(subject) = normalize_subject(skill) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for d in self.run_dirs(&subject) {
            if let Some(rec) = Self::load_record(&d) {
                out.push(rec);
                if out.len() >= limit {
                    break;
                }
            }
        }
        out
    }

    fn running(&self) -> Option<Value> {
        {
            let st = self.state();
            if let Some(r) = &st.running {
                return Some(r.clone());
            }
            if let Some(cur) = &st.current {
                let skill = parse_subject(&cur.subject).map(|p| p.1).unwrap_or_default();
                return Some(
                    json!({"subject": cur.subject, "skill": skill, "run_id": cur.run_id, "tiers": cur.tiers,
                                   "started_at": null, "elsewhere": false}),
                );
            }
        }
        if self.lock.held_elsewhere() {
            return Some(
                json!({"subject": null, "skill": null, "run_id": null, "tiers": null, "started_at": null,
                               "elsewhere": true}),
            );
        }
        None
    }

    fn detail(&self, skill: &str, limit: usize) -> Result<Value, Fail> {
        let subject = normalize_subject(skill)?;
        let (src, key) = parse_subject(&subject)?;
        if !self.subject_exists(&subject)? {
            return Err(Fail::P(Problem::not_found(format!("no such skill: {subject}"))));
        }
        let current = self.current_hash(&subject);
        let history: Vec<Value> = self
            .records(&subject, limit)
            .into_iter()
            .map(|mut r| {
                let state = freshness(Some(&r), current.as_deref());
                if let Some(m) = r.as_object_mut() {
                    m.insert("state".into(), Value::String(state.into()));
                }
                r
            })
            .collect();
        let run = self.running();
        let run = run.filter(|r| {
            let s = r.get("subject");
            s.is_none_or(Value::is_null) || s.and_then(Value::as_str) == Some(subject.as_str())
        });
        let state = history
            .first()
            .and_then(|h| h.get("state"))
            .cloned()
            .unwrap_or_else(|| json!("none"));
        Ok(json!({
            "skill": key, "subject": subject, "source": src, "content_hash": current,
            "state": state, "latest": history.first().cloned(), "history": history, "running": run,
            "gate": current.as_deref().map(|h| self.gate(h)),
        }))
    }

    fn report(&self, skill: &str, run_id: &str) -> Result<Value, Fail> {
        let subject = normalize_subject(skill)?;
        let key = parse_subject(&subject)?.1;
        if !RUN_ID_RE.is_match(run_id) {
            return Err(Fail::P(Problem::bad_request("invalid run id").code("invalid_id")));
        }
        let runs = self.runs_dir(&subject)?;
        let run_dir = runs.join(run_id);
        let rec = if is_symlink(&runs) || is_symlink(&run_dir) {
            None
        } else {
            Self::load_record(&run_dir)
        };
        let Some(rec) = rec else {
            return Err(Fail::P(Problem::not_found("no such evaluation run")));
        };
        let base = |md: Value, truncated: bool| json!({"skill": key, "subject": subject, "run_id": run_id, "markdown": md, "truncated": truncated});
        let mut src_dir = run_dir.clone();
        if let Some(r) = rec.get("reused_from").and_then(Value::as_object)
            && let (Some(rs), Some(rr)) = (
                r.get("subject").and_then(Value::as_str),
                r.get("run_id").and_then(Value::as_str),
            )
        {
            let cand = normalize_subject(rs)
                .and_then(|s| self.runs_dir(&s))
                .map_or_else(|_| run_dir.clone(), |d| d.join(rr));
            if RUN_ID_RE.is_match(rr) && !is_symlink(&cand) && Self::load_record(&cand).is_some() {
                src_dir = cand;
            }
        }
        for md in [src_dir.join(T1_MD), src_dir.join(STATIC_DIR).join(T1_MD)] {
            match read_regular(&md, REPORT_CAP) {
                Err(ReadErr::TooBig) => return Ok(base(Value::Null, true)),
                Err(ReadErr::Os(e)) => return Err(Fail::Os(e)),
                Ok(Some((bytes, _))) => {
                    let text = String::from_utf8_lossy(&bytes);
                    let n = text.chars().count();
                    return Ok(base(
                        Value::String(scrub(take_chars(&text, MARKDOWN_CAP))),
                        n > MARKDOWN_CAP,
                    ));
                }
                Ok(None) => {}
            }
        }
        Ok(base(Value::Null, false))
    }

    // -- the by-hash index, reuse and the gate ------------------------------------------------------------------------------
    fn pointer_path(&self, h: &str) -> PathBuf {
        self.root.join(BY_HASH).join(format!("{h}.json"))
    }

    fn best_run(&self, h: &str) -> Option<(Value, Value)> {
        if !HASH_RE.is_match(h) {
            return None;
        }
        let got = read_regular(&self.pointer_path(h), 64 * 1024).ok()??;
        let ptr = json_loads(&got.0)?;
        let subject = ptr.get("subject").and_then(Value::as_str)?;
        let run_id = ptr.get("run_id").and_then(Value::as_str)?;
        if !ptr.is_object() || !RUN_ID_RE.is_match(run_id) {
            return None;
        }
        let d = self.runs_dir(&normalize_subject(subject).ok()?).ok()?.join(run_id);
        let rec = if is_symlink(&d) { None } else { Self::load_record(&d) }?;
        if rec.get("content_hash").and_then(Value::as_str) != Some(h) {
            return None;
        }
        Some((ptr, rec))
    }

    fn update_pointer(&self, rec: &Value) {
        let Some(h) = rec.get("content_hash").and_then(Value::as_str) else {
            return;
        };
        let Some(tiers) = rec.get("tiers").and_then(Value::as_str) else {
            return;
        };
        let Some(rank) = tier_rank(tiers) else { return };
        if !HASH_RE.is_match(h) || is_str(rec.get("verdict"), "error") {
            return;
        }
        if let Some((_, old)) = self.best_run(h)
            && old
                .get("tiers")
                .and_then(Value::as_str)
                .and_then(tier_rank)
                .unwrap_or(-1)
                > rank
        {
            return;
        }
        let g = |k: &str| rec.get(k).cloned().unwrap_or(Value::Null);
        let ptr = json!({"subject": g("subject"), "run_id": g("run_id"), "tiers": tiers, "verdict": g("verdict"),
                         "evaluator_version": g("evaluator_version"), "finished_at": g("finished_at")});
        let text = dumps(
            &ptr,
            &Opts {
                sort_keys: true,
                ..Opts::default()
            },
        );
        let _ = atomic_write(&self.pointer_path(h), text.as_bytes(), 0o600, 0o700);
    }

    fn reusable(&self, p: &Prepared) -> Option<(Value, Value)> {
        let (ptr, rec) = self.best_run(&p.hash)?;
        let rank = rec
            .get("tiers")
            .and_then(Value::as_str)
            .and_then(tier_rank)
            .unwrap_or(-1);
        let verdict = rec.get("verdict");
        if rank < tier_rank(&p.tiers).unwrap_or(0)
            || verdict.is_none_or(Value::is_null)
            || is_str(verdict, "error")
            || rec.get("validators").is_none()
            || ptr.get("evaluator_version").cloned().unwrap_or(Value::Null)
                != p.version.clone().map_or(Value::Null, Value::String)
        {
            return None;
        }
        let rev = int(rec.get("eval_rev")).filter(|r| *r != 0).unwrap_or(1);
        let incomplete_scan = list(rec.get("validators")).iter().any(|v| {
            let v = dict(Some(v));
            is_str(v.get("validator"), "Security Scan") && is_str(v.get("status"), "incomplete")
        });
        if rev < self.tun().eval_rev && incomplete_scan {
            return None; // recorded before the hub explained partial scans: evaluate again, once
        }
        Some((ptr, rec))
    }

    /// F7, fail closed: missing | blocked | warn | ok from the stored reports, never a model.
    fn gate(&self, content_hash: &str) -> Value {
        let none = json!({"status": "missing", "verdict": null, "run_id": null, "tiers": null, "subject": null, "reasons": []});
        let Some((_, rec)) = self.best_run(content_hash) else {
            return none;
        };
        let need = self.gate_min_tiers();
        let validators: Vec<&Map<String, Value>> = list(rec.get("validators")).iter().map(|v| dict(Some(v))).collect();
        let rank = rec
            .get("tiers")
            .and_then(Value::as_str)
            .and_then(tier_rank)
            .unwrap_or(-1);
        let verdict = rec.get("verdict");
        if rank < tier_rank(&need).unwrap_or(0)
            || verdict.is_none_or(Value::is_null)
            || is_str(verdict, "error")
            || validators.is_empty()
        {
            return none;
        }
        let g = |k: &str| rec.get(k).cloned().unwrap_or(Value::Null);
        let mut blocked: Vec<String> = Vec::new();
        let mut warn: Vec<String> = Vec::new();
        let shown: Vec<&Map<String, Value>> = list(rec.get("security_findings"))
            .iter()
            .filter_map(Value::as_object)
            .collect();
        let crit_high =
            |f: &Map<String, Value>| is_str(f.get("severity"), "critical") || is_str(f.get("severity"), "high");
        for f in &shown {
            if crit_high(f) {
                blocked.push(format!(
                    "{}: {}",
                    get_str(f, "validator", "None"),
                    get_str(f, "message", "None")
                ));
            }
        }
        let present: BTreeSet<String> = validators
            .iter()
            .map(|v| {
                let n = first_truthy(v, &["validator"]);
                if n.is_empty() { "?".into() } else { n }
            })
            .collect();
        for name in EXPECTED_SECURITY_VALIDATORS {
            if !present.contains(name) {
                blocked.push(format!("{name}: validator missing from the report"));
            }
        }
        for (which, key) in [("static", "validators"), ("llm", "validators_llm")] {
            for v in list(rec.get(key)) {
                let v = dict(Some(v));
                let name = {
                    let n = first_truthy(v, &["validator"]);
                    if n.is_empty() { "?".to_string() } else { n }
                };
                if !is_security_validator(&name) {
                    continue;
                }
                let sev = dict(v.get("severity"));
                let bad = int(sev.get("critical")).unwrap_or(0) + int(sev.get("high")).unwrap_or(0);
                let seen = shown
                    .iter()
                    .any(|f| is_str(f.get("validator"), &name) && crit_high(f) && is_str(f.get("pass"), which));
                if bad != 0 && !seen {
                    blocked.push(format!("{name}: {bad} critical/high finding(s)"));
                }
                if which == "static" {
                    let part = dict(dict(rec.get("scan_partial")).get(&name));
                    if is_str(v.get("status"), "incomplete") && is_str(part.get("reason"), UNRESOLVED_REFERENCES) {
                        let where_ = list(part.get("references"))
                            .iter()
                            .take(3)
                            .map(|x| {
                                let x = dict(Some(x));
                                format!("{}:{}", get_str(x, "path", "None"), get_str(x, "line", "None"))
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        let count = int(part.get("count")).map_or_else(|| "None".to_string(), |c| c.to_string());
                        warn.push(format!(
                            "{name}: {count} local reference(s) not bundled with the skill ({where_}); every bundled file was scanned"
                        ));
                    } else if !(is_str(v.get("status"), "passed") || is_str(v.get("status"), "failed")) {
                        let st = v.get("status").map_or_else(|| "None".to_string(), pystr);
                        blocked.push(format!(
                            "{name}: status {} (the scan did not finish)",
                            take_chars(&st, 20)
                        ));
                    }
                    if let Some(m) = int(sev.get("medium")).filter(|m| *m != 0) {
                        warn.push(format!("{name}: {m} medium finding(s)"));
                    }
                }
            }
        }
        let (status, reasons): (&str, Vec<String>) = if !blocked.is_empty() {
            ("blocked", blocked.into_iter().take(8).collect())
        } else if !warn.is_empty() {
            ("warn", warn.into_iter().take(5).collect())
        } else {
            ("ok", Vec::new())
        };
        json!({"verdict": g("verdict"), "run_id": g("run_id"), "tiers": g("tiers"), "subject": g("subject"),
               "status": status, "reasons": reasons})
    }

    // -- evaluation --------------------------------------------------------------------------------------------------------
    /// `_new_run_id()`: UTC time with milliseconds, then 32 random bits.
    #[allow(clippy::cast_possible_truncation)]
    fn new_run_id(&self) -> String {
        let ts = self.clock();
        let secs = ts.floor() as i64;
        let stamp = time::OffsetDateTime::from_unix_timestamp(secs)
            .ok()
            .and_then(|d| {
                d.format(&time::macros::format_description!(
                    "[year][month][day]T[hour][minute][second]"
                ))
                .ok()
            })
            .unwrap_or_else(|| "19700101T000000".into());
        let ms = ((ts * 1000.0).trunc() as i64).rem_euclid(1000);
        format!("{stamp}{ms:03}Z-{}", hub_core::common::token_hex(4))
    }

    fn acquire(&self) -> Result<(), Fail> {
        self.ensure_root()?;
        if !self.lock.try_acquire()? {
            return Err(Fail::P(busy()));
        }
        Ok(())
    }

    fn prepare(&self, skill: &str, tiers: &str, run_id: Option<&str>, job: Option<Arc<Job>>) -> Result<Prepared, Fail> {
        if !TIERS.contains(&tiers) {
            return Err(Fail::P(bad_tiers()));
        }
        let subject = normalize_subject(skill)?;
        let (src, key) = parse_subject(&subject)?;
        let found = self.find_launcher().ok_or_else(|| Fail::P(not_installed()))?;
        let snap = if src == "hub" {
            // the content lock: a consistent copy, never mid-edit
            let h = self.hub.upgrade();
            let _g = match &h {
                Some(h) => Some(h.mutating()?),
                None => None,
            };
            self.snapshot(&subject)?
        } else {
            self.snapshot(&subject)?
        };
        let version = self.version_for(&found)?;
        let rid = run_id.map_or_else(|| self.new_run_id(), str::to_string);
        let run_dir = self.runs_dir(&subject)?.join(&rid);
        if let Some(parent) = run_dir.parent() {
            mkdir_private(parent)?;
        }
        fs::DirBuilder::new().mode(0o700).create(&run_dir)?;
        fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))?;
        let hash = snap.content_hash();
        Ok(Prepared {
            subject,
            source: src,
            name: key,
            tiers: tiers.to_string(),
            run_id: rid,
            launcher: found.0,
            exe: found.1,
            snap,
            hash,
            run_dir,
            started_at: self.clock(),
            version,
            job,
        })
    }

    /// `_do`: reuse a finished run of the same content when allowed (no process), else run SkillEvaluator.
    fn run(&self, p: &Prepared, actor: &str, force: bool) -> Result<Value, Fail> {
        if !force && let Some((ptr, rec)) = self.reusable(p) {
            return self.write_reused(p, actor, &ptr, &rec);
        }
        self.execute(p, actor)
    }

    fn enqueue(this: &Arc<Inner>, subjects: &[String], tiers: &str, force: bool, actor: &str) -> HubResult<Value> {
        if !TIERS.contains(&tiers) {
            return Err(bad_tiers());
        }
        if this.find_launcher().is_none() {
            return Err(not_installed());
        }
        let wanted: Vec<String> = subjects
            .iter()
            .map(|s| normalize_subject(s))
            .collect::<HubResult<_>>()?;
        let mut exists: HashMap<String, bool> = HashMap::new();
        for s in &wanted {
            if !exists.contains_key(s) {
                let e = this.subject_exists(s)?;
                exists.insert(s.clone(), e);
            }
        }
        let mut queued = Vec::new();
        let mut skipped = Vec::new();
        let mut run_ids = Map::new();
        let mut st = this.state();
        if st.stop {
            return Err(Problem::unavailable("the hub is shutting down").code("shutting_down"));
        }
        for s in wanted {
            if !exists.get(&s).copied().unwrap_or(false) {
                skipped.push(json!({"subject": s, "reason": "not_found"}));
            } else if let Some(cur) = st.current.as_ref().filter(|c| c.subject == s) {
                run_ids.insert(s.clone(), Value::String(cur.run_id.clone()));
                skipped.push(json!({"subject": s, "reason": "running"}));
            } else if let Some(j) = st.queued.get(&s) {
                run_ids.insert(s.clone(), Value::String(j.run_id.clone()));
                skipped.push(json!({"subject": s, "reason": "queued"}));
            } else if st.queue.len() >= QUEUE_CAP {
                skipped.push(json!({"subject": s, "reason": "queue_full"}));
            } else {
                let job = Arc::new(Job {
                    subject: s.clone(),
                    tiers: tiers.to_string(),
                    force,
                    actor: take_chars(actor, 64).to_string(),
                    run_id: this.new_run_id(),
                    cancelled: AtomicBool::new(false),
                });
                run_ids.insert(s.clone(), Value::String(job.run_id.clone()));
                st.queue.push_back(job.clone());
                st.queued.insert(s.clone(), job);
                queued.push(s);
            }
        }
        if !queued.is_empty() && st.worker.as_ref().is_none_or(std::thread::JoinHandle::is_finished) {
            let w = this.clone();
            st.worker = std::thread::Builder::new()
                .name("skilleval-queue".into())
                .spawn(move || w.work())
                .ok();
        }
        this.cv.notify_all();
        Ok(json!({"queued": queued, "skipped": skipped, "run_ids": run_ids}))
    }

    fn join(&self, timeout: Option<f64>) {
        let st = self.state();
        let cond = |s: &mut State| !s.queue.is_empty() || s.current.is_some();
        match timeout {
            None => {
                let _g = self
                    .cv
                    .wait_while(st, cond)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            Some(t) => {
                let _g = self
                    .cv
                    .wait_timeout_while(st, Duration::from_secs_f64(t.max(0.0)), cond)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
    }

    fn note_error(&self, job: &Job, text: &str) {
        let mut st = self.state();
        st.errors += 1;
        st.last_errors
            .push_back(json!({"subject": job.subject, "error": take_chars(&scrub(text), 300)}));
        while st.last_errors.len() > 20 {
            st.last_errors.pop_front();
        }
    }

    fn work(&self) {
        loop {
            let job = {
                let mut st = self.state();
                while st.queue.is_empty() && !st.stop {
                    st = self.cv.wait(st).unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                if st.stop {
                    return;
                }
                let Some(job) = st.queue.pop_front() else { continue };
                st.queued.remove(&job.subject);
                st.current = Some(job.clone());
                job
            };
            if let Err(f) = self.run_job(&job) {
                self.note_error(&job, &f.type_name());
            }
            let mut st = self.state();
            st.current = None;
            self.cv.notify_all();
        }
    }

    fn run_job(&self, job: &Arc<Job>) -> Result<(), Fail> {
        self.ensure_root()?;
        loop {
            if job.cancelled.load(Ordering::SeqCst) || self.state().stop {
                return Ok(());
            }
            if self.lock.try_acquire()? {
                break;
            }
            std::thread::sleep(Duration::from_secs(1)); // another process (hubctl) holds the evaluation lock
        }
        let res = (|| -> Result<(), Fail> {
            self.recover_interrupted();
            if job.cancelled.load(Ordering::SeqCst) {
                return Ok(());
            }
            let rec = match self
                .prepare(&job.subject, &job.tiers, Some(&job.run_id), Some(job.clone()))
                .and_then(|p| self.run(&p, &job.actor, job.force))
            {
                Ok(r) => r,
                Err(Fail::P(p)) => {
                    self.note_error(job, &p.detail);
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            let mut st = self.state();
            st.finished += 1;
            if is_str(rec.get("verdict"), "error") {
                st.errors += 1;
                let err = rec
                    .get("error")
                    .filter(|v| truthy(Some(v)))
                    .map_or_else(|| "error".to_string(), pystr);
                st.last_errors
                    .push_back(json!({"subject": job.subject, "error": take_chars(&err, 300)}));
                while st.last_errors.len() > 20 {
                    st.last_errors.pop_front();
                }
            }
            Ok(())
        })();
        self.lock.release();
        res
    }

    /// Once per process, under the lock: a run directory without a record is a run the previous hub process died in.
    fn recover_interrupted(&self) {
        if self.recovered.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut dirs: Vec<(String, PathBuf)> = Vec::new();
        let names = |d: &Path| -> Vec<(String, PathBuf)> {
            fs::read_dir(d)
                .map(|rd| {
                    rd.flatten()
                        .filter_map(|e| Some((e.file_name().to_str()?.to_string(), e.path())))
                        .collect()
                })
                .unwrap_or_default()
        };
        for (n, p) in names(&self.root) {
            if valid_id(&n) && p.is_dir() && !is_symlink(&p) {
                dirs.push((format!("hub:{n}"), p));
            }
        }
        for (c, cp) in names(&self.root.join(OBSERVED_DIR)) {
            if valid_id(&c) && cp.is_dir() && !is_symlink(&cp) {
                for (k, kp) in names(&cp) {
                    if KEY_RE.is_match(&k) && kp.is_dir() && !is_symlink(&kp) {
                        dirs.push((format!("{c}:{k}"), kp));
                    }
                }
            }
        }
        for (subject, d) in dirs {
            let Ok((src, key)) = parse_subject(&subject) else {
                continue;
            };
            for (rn, rd) in names(&d) {
                if RUN_ID_RE.is_match(&rn) && rd.is_dir() && !is_symlink(&rd) && !rd.join(RECORD).exists() {
                    let rec = json!({"run_id": rn, "subject": subject, "skill": key, "source": src, "tiers": "static",
                                     "verdict": "error", "error": "interrupted", "finished_at": self.clock(),
                                     "content_hash": null, "findings": []});
                    if Self::write_record(&rd, &rec).is_err() {
                        break;
                    }
                }
            }
        }
    }

    fn tier1_argv(p: &Prepared, copy: &Path, out: &Path, llm: bool) -> Vec<String> {
        let mut v = vec![p.exe.clone(), "tier1".into(), copy.to_string_lossy().into_owned()];
        if !llm {
            v.push("--no-llm".into());
        }
        v.extend(
            ["--checks", SAFE_CHECKS, "-r", "json,markdown", "-o"]
                .iter()
                .map(|s| (*s).to_string()),
        );
        v.push(out.to_string_lossy().into_owned());
        v
    }

    fn check_cancel(p: &Prepared) -> Result<(), Fail> {
        if p.job.as_ref().is_some_and(|j| j.cancelled.load(Ordering::SeqCst)) {
            return Err(Fail::Cancelled);
        }
        Ok(())
    }

    fn base_record(&self, p: &Prepared, actor: &str) -> Map<String, Value> {
        let v = json!({
            "run_id": p.run_id, "skill": p.name, "subject": p.subject, "source": p.source, "path": p.snap.path,
            "origin": p.snap.origin, "tiers": p.tiers, "launcher": p.launcher, "evaluator_version": p.version,
            "started_at": p.started_at, "content_hash": p.hash, "files": p.snap.files.len(),
            "bytes": p.snap.total_bytes(), "actor": take_chars(actor, 64), "model_calls": p.tiers != "static",
            "static": null, "tier1": null, "tier2": null, "validators": null, "validators_llm": null,
            "security_findings": [], "findings": [], "findings_total": 0, "error": null, "llm_error": null,
            "reused_from": null, "eval_rev": self.tun().eval_rev, "scan_partial": {},
        });
        match v {
            Value::Object(m) => m,
            _ => Map::new(),
        }
    }

    fn emit(&self, actor: &str, p: &Prepared, rec: &Map<String, Value>) {
        let g = |k: &str| rec.get(k).cloned().unwrap_or(Value::Null);
        let params = json!({"skill": p.name, "subject": p.subject, "run_id": p.run_id, "tiers": g("tiers"),
                            "verdict": g("verdict"), "launcher": p.launcher, "content_hash": take_chars(&p.hash, 12),
                            "reused_from": g("reused_from")});
        let f = self.tun().on_event.clone();
        match f {
            Some(f) => f(actor, "skill.evaluate", &params),
            None => {
                if let Some(h) = self.hub.upgrade() {
                    let _ = h.audit_svc.event(actor, "skill.evaluate", &params);
                }
            }
        }
    }

    /// Same content, already evaluated (tiers at least as high, same evaluator version): a record that copies that run's
    /// summary and points at it. No process is started.
    fn write_reused(&self, p: &Prepared, actor: &str, ptr: &Value, src: &Value) -> Result<Value, Fail> {
        let mut rec = src.as_object().cloned().unwrap_or_default();
        for (k, v) in self.base_record(p, actor) {
            rec.insert(k, v);
        }
        for k in [
            "tiers",
            "launcher",
            "model_calls",
            "static",
            "tier1",
            "tier2",
            "validators",
            "validators_llm",
            "security_findings",
            "findings",
            "findings_total",
            "error",
            "llm_error",
            "evaluator_version",
        ] {
            rec.insert(k.into(), src.get(k).cloned().unwrap_or(Value::Null));
        }
        rec.insert("verdict".into(), src.get("verdict").cloned().unwrap_or(Value::Null));
        rec.insert("finished_at".into(), json!(self.clock()));
        rec.insert("duration_ms".into(), json!(0));
        rec.insert("reports".into(), json!([]));
        rec.insert(
            "reused_from".into(),
            json!({"subject": ptr.get("subject").cloned().unwrap_or(Value::Null),
                   "run_id": ptr.get("run_id").cloned().unwrap_or(Value::Null)}),
        );
        let v = Value::Object(rec.clone());
        Self::write_record(&p.run_dir, &v)?;
        self.prune(&p.subject);
        self.emit(actor, p, &rec);
        Ok(v)
    }

    #[allow(clippy::too_many_lines)]
    fn execute(&self, p: &Prepared, actor: &str) -> Result<Value, Fail> {
        let work = self.root.join(".work").join(&p.run_id);
        let mut rec = self.base_record(p, actor);
        self.state().running = Some(
            json!({"subject": p.subject, "skill": p.name, "run_id": p.run_id, "tiers": p.tiers,
                                            "started_at": p.started_at, "elsewhere": false}),
        );
        let t0 = Instant::now();
        let copy = work.join(&p.name);
        let static_dir = p.run_dir.join(STATIC_DIR);
        let timeout = self.tun().timeout;
        let runner = self.runner();
        let res: Result<String, Fail> = (|| {
            Self::check_cancel(p)?;
            self.clean_work()?;
            mkdir_private(&work)?;
            Self::write_copy(&copy, &p.snap)?;
            mkdir_private(&static_dir)?;
            let env = self.child_env();
            let r1 = runner.run(&Self::tier1_argv(p, &copy, &static_dir, false), &work, &env, timeout);
            Self::check_cancel(p)?;
            let mut verdict;
            match Self::load_report(&static_dir.join(T1_JSON)) {
                None => {
                    verdict = "error".to_string();
                    rec.insert("error".into(), Value::String(self.error_text(&r1, &copy, timeout)));
                }
                Some(ts) => {
                    let st = self.clean(&summarize_tier1(&ts), &copy);
                    rec.insert("static".into(), st.clone());
                    rec.insert("tier1".into(), st.clone());
                    rec.insert("validators".into(), self.clean(&validator_summaries(&ts), &copy));
                    if let Some(partial) = self.explain_partial_security(&ts, &copy, &work, &env) {
                        rec.insert(
                            "scan_partial".into(),
                            json!({"Security Scan": self.clean(&partial, &copy)}),
                        );
                    }
                    rec.insert(
                        "security_findings".into(),
                        self.clean(&security_findings(&ts, "static"), &copy),
                    );
                    let (mut findings, mut total) = top_findings(&ts, TOP_FINDINGS);
                    verdict = st.get("status").and_then(Value::as_str).unwrap_or("error").to_string();
                    if verdict == "error" {
                        rec.insert("error".into(), json!("the tier1 report has no overall_status"));
                    } else if p.tiers != "static" {
                        // second pass with the LLM judge; its security findings can only ADD to the gate
                        let r2 = runner.run(&Self::tier1_argv(p, &copy, &p.run_dir, true), &work, &env, timeout);
                        Self::check_cancel(p)?;
                        match Self::load_report(&p.run_dir.join(T1_JSON)) {
                            None => {
                                rec.insert("llm_error".into(), Value::String(self.error_text(&r2, &copy, timeout)));
                                verdict = combine_verdicts(&verdict, "error");
                            }
                            Some(tl) => {
                                let t1 = self.clean(&summarize_tier1(&tl), &copy);
                                rec.insert("tier1".into(), t1.clone());
                                rec.insert("validators_llm".into(), self.clean(&validator_summaries(&tl), &copy));
                                let extra: Vec<Value> = list(Some(&self.clean(&security_findings(&tl, "llm"), &copy)))
                                    .iter()
                                    .filter(|f| {
                                        is_str(f.get("severity"), "critical") || is_str(f.get("severity"), "high")
                                    })
                                    .cloned()
                                    .collect();
                                if let Some(Value::Array(a)) = rec.get_mut("security_findings") {
                                    a.extend(extra);
                                }
                                (findings, total) = top_findings(&tl, TOP_FINDINGS);
                                let s2 = t1.get("status").and_then(Value::as_str).unwrap_or("error").to_string();
                                verdict = combine_verdicts(&verdict, &s2);
                            }
                        }
                    }
                    rec.insert("findings".into(), self.clean(&Value::Array(findings), &copy));
                    rec.insert("findings_total".into(), json!(total));
                }
            }
            if p.tiers == "tier1+2" {
                Self::check_cancel(p)?;
                let argv = vec![
                    p.exe.clone(),
                    "tier2".into(),
                    copy.to_string_lossy().into_owned(),
                    "-r".into(),
                    "json".into(),
                    "-o".into(),
                    p.run_dir.to_string_lossy().into_owned(),
                ];
                let r3 = runner.run(&argv, &work, &env, timeout);
                let t2 = Self::load_report(&p.run_dir.join(T2_JSON));
                let st2 = t2
                    .as_ref()
                    .and_then(|t| t.get("overall_status"))
                    .and_then(Value::as_str)
                    .filter(|s| ["passed", "failed", "incomplete"].contains(s))
                    .unwrap_or("error")
                    .to_string();
                let err = if st2 == "error" {
                    Value::String(self.error_text(&r3, &copy, timeout))
                } else {
                    Value::Null
                };
                rec.insert(
                    "tier2".into(),
                    json!({"status": st2,
                           "errors": t2.as_ref().map_or(Value::Null, |t| int_value(t.get("total_errors"))),
                           "warnings": t2.as_ref().map_or(Value::Null, |t| int_value(t.get("total_warnings"))),
                           "error": err}),
                );
                verdict = combine_verdicts(&verdict, &st2);
            }
            Ok(verdict)
        })();
        match res {
            Ok(v) => {
                rec.insert("verdict".into(), Value::String(v));
            }
            Err(Fail::Cancelled) => {
                rec.insert("verdict".into(), json!("error"));
                rec.insert("error".into(), json!("cancelled"));
            }
            Err(e) => {
                rec.insert("verdict".into(), json!("error"));
                rec.insert(
                    "error".into(),
                    Value::String(format!("the evaluation could not run ({})", e.type_name())),
                );
            }
        }
        // finally:
        rmtree(&work);
        let ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
        rec.insert("duration_ms".into(), json!(ms));
        rec.insert("finished_at".into(), json!(self.clock()));
        self.finalize_reports(&p.run_dir, &copy);
        let mut reports: Vec<String> = [T1_JSON, T1_MD, T2_JSON]
            .iter()
            .filter(|n| p.run_dir.join(n).is_file())
            .map(|n| (*n).to_string())
            .chain(
                [T1_JSON, T1_MD]
                    .iter()
                    .filter(|n| static_dir.join(n).is_file())
                    .map(|n| format!("{STATIC_DIR}/{n}")),
            )
            .collect();
        reports.sort();
        rec.insert("reports".into(), json!(reports));
        if !rec.contains_key("verdict") {
            rec.insert("verdict".into(), json!("error"));
        }
        let v = Value::Object(rec.clone());
        // Python: an OSError from _write_record leaves the `finally` block early, so the pointer, the pruning and the
        // `running` reset are skipped too (a stale `running` entry; recorded as a Python bug, behaviour kept).
        Self::write_record(&p.run_dir, &v)?;
        self.update_pointer(&v);
        self.prune(&p.subject);
        if p.source == "hub" {
            self.hashes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&p.name);
        }
        self.state().running = None;
        self.emit(actor, p, &rec);
        Ok(v)
    }

    // -- helpers -------------------------------------------------------------------------------------------------------------
    /// For a static Security Scan that is `incomplete` ONLY because SkillSpector called its analysis partial, run
    /// SkillSpector's static scan on the same copy and classify why. None whenever anything differs or cannot be read.
    fn explain_partial_security(
        &self,
        ts: &Value,
        copy: &Path,
        work: &Path,
        env: &[(String, String)],
    ) -> Option<Value> {
        let sec = list(ts.get("results"))
            .iter()
            .map(|r| dict(Some(r)))
            .find(|r| is_str(r.get("validator"), "Security Scan"))
            .unwrap_or(&EMPTY_MAP);
        let errors: Vec<String> = list(dict(sec.get("legacy")).get("errors"))
            .iter()
            .map(pystr)
            .filter(|e| !FINDING_LINE.is_match(e))
            .collect();
        if !is_str(sec.get("status"), "incomplete")
            || sec.get("incomplete_scans") != Some(&json!([SKILLSPECTOR]))
            || errors.len() != 1
            || !errors[0].starts_with(PARTIAL_ERROR)
        {
            return None;
        }
        let exe = self.which(SKILLSPECTOR, &self.search_path())?;
        let out = work.join(SKILLSPECTOR_REPORT);
        let argv: Vec<String> = vec![
            exe,
            "scan".into(),
            copy.to_string_lossy().into_owned(),
            "--format".into(),
            "json".into(),
            "--no-llm".into(),
            "--output".into(),
            out.to_string_lossy().into_owned(),
        ];
        let r = self.runner().run(&argv, work, env, SKILLSPECTOR_TIMEOUT_S);
        if r.timed_out {
            return None;
        }
        Self::load_report(&out).and_then(|rep| classify_partial_scan(&rep))
    }

    /// Leftovers of an interrupted run. Only called while holding the global lock.
    fn clean_work(&self) -> io::Result<()> {
        let w = self.root.join(".work");
        if w.is_dir() && !is_symlink(&w) {
            for e in fs::read_dir(&w)? {
                let p = e?.path();
                if is_symlink(&p) {
                    fs::remove_file(&p)?;
                } else {
                    rmtree(&p);
                }
            }
        }
        Ok(())
    }

    fn write_copy(dest: &Path, snap: &Snapshot) -> io::Result<()> {
        fs::DirBuilder::new().mode(0o700).create(dest)?;
        let mut rels: Vec<&String> = snap.files.keys().collect();
        rels.sort();
        for rel in rels {
            let target = dest.join(rel);
            if let Some(parent) = target.parent()
                && parent != dest
            {
                mkdir_private(parent)?;
            }
            // keep "is a script" for the script checks
            let mode = if snap.modes.get(rel).copied().unwrap_or(0) & 0o111 != 0 {
                0o700
            } else {
                0o600
            };
            write_new(&target, &snap.files[rel], mode)?;
        }
        Ok(())
    }

    /// The temporary copy's path, its work dir and the state dir replaced by markers; whole path prefixes only.
    fn scrub_paths(&self, text: &str, copy: &Path) -> String {
        let work = copy.parent().map(Path::to_path_buf).unwrap_or_default();
        let state = self.root.parent().map(Path::to_path_buf).unwrap_or_default();
        let real = |p: &Path| hub_core::py::os::realpath(p).to_string_lossy().into_owned();
        let pairs = [
            (copy.to_string_lossy().into_owned(), COPY_MARK),
            (real(copy), COPY_MARK),
            (work.to_string_lossy().into_owned(), WORK_MARK),
            (real(&work), WORK_MARK),
            (state.to_string_lossy().into_owned(), STATE_MARK),
            (real(&state), STATE_MARK),
        ];
        let mut text = text.to_string();
        for (p, token) in pairs {
            if p.chars().count() > 1 && text.contains(&p) {
                text = replace_path(&text, &p, token);
            }
        }
        text
    }

    /// `_clean(obj, copy)`: strings with the temp-copy and state paths replaced, secret-shaped values redacted.
    fn clean(&self, obj: &Value, copy: &Path) -> Value {
        match obj {
            Value::String(s) => Value::String(scrub(&self.scrub_paths(s, copy))),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.clean(x, copy)).collect()),
            Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), self.clean(v, copy))).collect()),
            other => other.clone(),
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn error_text(&self, r: &EvalResult, copy: &Path, timeout: f64) -> String {
        if r.timed_out {
            return format!("SkillEvaluator timed out after {} s", timeout.trunc() as i64);
        }
        let err = strip(&r.stderr);
        let raw = if err.is_empty() { strip(&r.stdout) } else { err };
        let text = ANSI.replace_all(raw, "");
        let text = scrub(&self.scrub_paths(&text, copy));
        let n = text.chars().count();
        let tail: String = if n > ERROR_TAIL {
            text.chars().skip(n - ERROR_TAIL).collect()
        } else {
            text
        };
        if tail.is_empty() {
            format!("exit {}, no report", r.returncode)
        } else {
            format!("exit {}, no report: {tail}", r.returncode)
        }
    }

    fn load_report(path: &Path) -> Option<Value> {
        let got = read_regular(path, REPORT_CAP).ok()??;
        json_loads(&got.0).filter(Value::is_object)
    }

    /// Make what SkillEvaluator wrote private (0600 / 0700), drop symlinks and special files it may have left, and
    /// replace the temporary copy's absolute path in text reports.
    fn finalize_reports(&self, run_dir: &Path, copy: &Path) {
        let mut stack = vec![run_dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let _ = fs::set_permissions(&d, fs::Permissions::from_mode(0o700));
            let (dirs, files) = list_dir(&d);
            let mut subdirs = Vec::new();
            for n in dirs {
                let p = d.join(&n);
                if is_symlink(&p) {
                    let _ = fs::remove_file(&p);
                } else {
                    subdirs.push(p);
                }
            }
            for n in files {
                let full = d.join(&n);
                let _ = (|| -> io::Result<()> {
                    let st = fs::symlink_metadata(&full)?;
                    if !st.file_type().is_file() {
                        return fs::remove_file(&full); // symlinks, FIFOs, sockets: nothing a report needs
                    }
                    if matches!(py_suffix(&n), ".json" | ".md") {
                        match read_regular(&full, REPORT_CAP) {
                            Ok(Some((bytes, _))) => {
                                let text = String::from_utf8_lossy(&bytes).into_owned();
                                let new = self.scrub_paths(&text, copy);
                                if new != text {
                                    let tmp = d.join(format!(".{n}.tmp"));
                                    write_new(&tmp, new.as_bytes(), 0o600)?;
                                    fs::rename(&tmp, &full)?;
                                }
                            }
                            Ok(None) => {}
                            Err(_) => return Ok(()), // too big or unreadable: left as it is (Python `continue`)
                        }
                    }
                    fs::set_permissions(&full, fs::Permissions::from_mode(0o600))
                })();
            }
            // os.walk is top-down in sorted order; push in reverse so the stack pops them in order
            for p in subdirs.into_iter().rev() {
                stack.push(p);
            }
        }
    }

    /// `_write_record(run_dir, rec)`: `json.dump(rec, indent=1, sort_keys=True)` to a 0600 temp file, fsync, rename.
    fn write_record(run_dir: &Path, rec: &Value) -> io::Result<()> {
        let tmp = run_dir.join(format!(".{RECORD}.tmp"));
        match fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let text = dumps(
            rec,
            &Opts {
                indent: Some(1),
                sort_keys: true,
                ..Opts::default()
            },
        );
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.flush()?;
        f.sync_all()?;
        fs::rename(&tmp, run_dir.join(RECORD))
    }

    /// Keep the newest `keep` run directories of this subject (unfinished ones count, so a crash loop cannot grow it).
    fn prune(&self, subject: &str) {
        let keep = self.tun().keep;
        for d in self.run_dirs(subject).into_iter().skip(keep) {
            rmtree(&d);
        }
    }
}

/// `PurePath(name).suffix`: the text from the last dot, unless the dot is the first or the last character.
fn py_suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => &name[i..],
        _ => "",
    }
}

/// `re.sub(re.escape(p) + r"(?![\w.-])", token, text)`: replace every occurrence of `p` that is not followed by a word
/// character, `.` or `-` (an occurrence that fails the look-ahead lets the scan continue one character later).
fn replace_path(text: &str, p: &str, token: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i <= text.len() {
        let Some(off) = text[i..].find(p) else { break };
        let j = i + off;
        let after = &text[j + p.len()..];
        let blocked = after.chars().next().is_some_and(|c| {
            let mut b = [0u8; 4];
            PATH_CONT.is_match(c.encode_utf8(&mut b))
        });
        if blocked {
            let step = text[j..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&text[i..j + step]);
            i = j + step;
        } else {
            out.push_str(&text[i..j]);
            out.push_str(token);
            i = j + p.len();
        }
    }
    if i <= text.len() {
        out.push_str(&text[i..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_path_honours_the_lookahead() {
        assert_eq!(
            replace_path("/a/b and /a/bc and /a/b.", "/a/b", "<x>"),
            "<x> and /a/bc and /a/b."
        );
        assert_eq!(replace_path("/a/b/c", "/a/b", "<x>"), "<x>/c");
        assert_eq!(replace_path("x/a/b", "/a/b", "<x>"), "x<x>");
        assert_eq!(replace_path("/a/a/a/", "/a/a", "T"), "T/a/");
    }

    #[test]
    fn bytecode_rule() {
        for p in [
            "__pycache__/x.cpython-313.pyc",
            "a/__pycache__/b.txt",
            "x.pyc",
            "d/y.pyo",
        ] {
            assert!(is_bytecode(p), "{p}");
        }
        for p in ["x.py", "pycache/x.txt", "x.pyc.txt", "__pycache__x/y"] {
            assert!(!is_bytecode(p), "{p}");
        }
    }

    #[test]
    fn subjects() {
        assert_eq!(parse_subject("hub:alpha").ok(), Some(("hub".into(), "alpha".into())));
        assert!(parse_subject("hub:x\n").is_err());
        assert_eq!(normalize_subject("alpha").ok().as_deref(), Some("hub:alpha"));
        assert_eq!(normalize_subject("A").err().map(|p| p.code), Some("invalid_id".into()));
        assert_eq!(
            normalize_subject("a:b c").err().map(|p| p.code),
            Some("invalid_subject".into())
        );
    }
}
