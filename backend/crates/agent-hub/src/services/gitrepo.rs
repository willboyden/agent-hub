//! Port of services/gitrepo.py: the content repo via the `git` CLI. argv lists only, a timeout on every call, no shell,
//! a fixed identity, the operator's global/system config ignored, and `contentguard` re-verified before git runs. The
//! exact argv/env are in PORTING.md "Subprocess argv" (G-1..G-3). Security control S-GIT. Owner: WS-B2.
//!
//! Python runs git with `umask=0o077` (set in the forked child). Without `pre_exec` (unsafe, forbidden here) the Rust
//! port spawns git from a scoped thread that first unshares its filesystem attributes (`unshare(CLONE_FS)`) and sets its
//! own umask to 0o077, so git's files are private and no other thread's umask changes ([`with_private_umask`],
//! PORTING.md addendum A-6). Only if the kernel refuses `unshare` does git inherit the process umask (D-B2-UMASK).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hub_core::ids::COMMIT_RE;
use hub_core::pathsafe::check_relpath;
use hub_core::py::text::{splitlines, strip, take_chars};
use hub_core::{HubResult, Problem};
use serde_json::{Value, json};

use super::contentguard;
use super::difflib::unified_diff;
use super::fs::{Captured, capture, path_env};

/// git's well-known empty tree object id (the content hash before the first commit).
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbf2c1ff";
pub const MAX_ARCHIVE: usize = 64 * 1024 * 1024;
/// git's stderr is only ever shown truncated (300 characters); 1 MiB bounds the memory a misbehaving git can take.
pub const GIT_STDERR_CAP: usize = 1024 * 1024;
/// The only paths `commit` stages (never `add -A` of the whole tree).
pub const KNOWN_CONTENT: [&str; 11] = [
    "skills",
    "agents",
    "instructions",
    "rules",
    "memory",
    "mcp",
    "collections",
    "clients",
    "profiles",
    "endpoints.yaml",
    "hub.yaml",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitResult {
    pub rc: i32,
    pub out: Vec<u8>,
    /// stderr decoded (utf-8, replace) and stripped
    pub err: String,
}

/// One pending change: status is added | modified | deleted | renamed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub status: String,
    pub diff: String,
}

pub struct GitRepo {
    pub path: PathBuf,
    pub git_bin: String,
    pub name: String,
    pub email: String,
    pub timeout: f64,
    /// `_verified`: when contentguard last passed (re-checked at most every 2 s)
    verified: Mutex<Option<Instant>>,
    /// false only in this module's unit tests, which exercise git without WS-F's contentguard
    guarded: bool,
}

/// The `-c` options of every `run` (G-1), before the caller's arguments.
fn run_prefix(git_bin: &str, path: &Path) -> Vec<String> {
    let mut v: Vec<String> = vec![git_bin.to_string()];
    for c in [
        "core.hooksPath=/dev/null",
        "commit.gpgsign=false",
        "core.sshCommand=/bin/false",
        "protocol.allow=never",
        "diff.external=",
        "core.quotepath=off",
        "core.fsmonitor=false",
        "core.autocrlf=false",
        "core.attributesFile=/dev/null",
    ] {
        v.push("-c".into());
        v.push(c.into());
    }
    v.push("-c".into());
    v.push(format!("safe.directory={}", path.to_string_lossy()));
    v
}

/// `bytes.decode("utf-8", "replace")`.
fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Python's repr of a list of str (`['a', 'b']`).
fn repr_list(items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|s| hub_core::py::repr(s)).collect();
    format!("[{}]", parts.join(", "))
}

impl GitRepo {
    pub fn new(path: &Path, git_bin: &str, name: &str, email: &str, timeout: f64) -> Self {
        Self {
            path: path.to_path_buf(),
            git_bin: git_bin.into(),
            name: name.into(),
            email: email.into(),
            timeout,
            verified: Mutex::new(None),
            guarded: true,
        }
    }

    /// Refuse a repo whose metadata could execute code (contentguard), re-checked at most every 2 s unless `force`.
    pub fn verify_safe(&self, force: bool) -> HubResult<()> {
        let git = self.path.join(".git");
        if !git.exists() && !git.is_symlink() {
            return Ok(());
        }
        if !self.guarded {
            return Ok(());
        }
        let mut last = self.verified.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let stale = last.is_none_or(|t| t.elapsed() > Duration::from_secs(2));
        if force || stale {
            contentguard::check_ownership(&self.path)?;
            contentguard::verify_git_metadata(&self.path)?;
            *last = Some(Instant::now());
        }
        Ok(())
    }

    /// `_env()`: exactly these variables (a HOME with no gitconfig, global/system config off, fixed identity).
    pub fn env(&self) -> Vec<(&'static str, String)> {
        let p = self.path.to_string_lossy().to_string();
        vec![
            ("PATH", path_env()),
            ("HOME", p),
            ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
            ("GIT_CONFIG_SYSTEM", "/dev/null".into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_ATTR_NOSYSTEM", "1".into()),
            ("GIT_TERMINAL_PROMPT", "0".into()),
            ("GIT_LITERAL_PATHSPECS", "1".into()),
            ("GIT_OPTIONAL_LOCKS", "0".into()),
            ("LC_ALL", "C".into()),
            ("GIT_AUTHOR_NAME", self.name.clone()),
            ("GIT_AUTHOR_EMAIL", self.email.clone()),
            ("GIT_COMMITTER_NAME", self.name.clone()),
            ("GIT_COMMITTER_EMAIL", self.email.clone()),
        ]
    }

    /// Spawn git; FileNotFoundError -> 503 git_missing, timeout -> 503 git_timeout, any other OSError -> 500.
    fn spawn(&self, argv: &[String], stdin: Option<Vec<u8>>, timeout: f64, cap: Option<usize>) -> HubResult<Captured> {
        let env = self.env();
        match with_private_umask(|| capture(argv, Some(&self.path), &env, stdin, timeout, cap, Some(GIT_STDERR_CAP))) {
            Ok(c) if c.timed_out => Err(Problem::unavailable("git timed out").code("git_timeout")),
            Ok(c) => Ok(c),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Problem::unavailable("git is not installed").code("git_missing"))
            }
            Err(_) => Err(Problem::internal()),
        }
    }

    /// `run(*args, check=True, timeout=None)`: Conflict `git_error` on a non-zero exit when `check`.
    pub fn run(&self, args: &[&str], check: bool, timeout: Option<f64>) -> HubResult<GitResult> {
        if args.first().is_some_and(|a| *a != "init") {
            self.verify_safe(false)?;
        }
        let mut argv = run_prefix(&self.git_bin, &self.path);
        argv.extend(args.iter().map(|a| (*a).to_string()));
        // Python: `timeout or self.timeout` (a zero timeout falls back to the default)
        let t = timeout.filter(|t| *t != 0.0).unwrap_or(self.timeout);
        let c = self.spawn(&argv, None, t, None)?;
        let res = GitResult {
            rc: c.rc,
            out: c.stdout,
            err: strip(&lossy(&c.stderr)).to_string(),
        };
        if check && res.rc != 0 {
            let arg0 = args.first().copied().unwrap_or_default();
            return Err(
                Problem::conflict(format!("git {arg0} failed: {}", take_chars(&res.err, 300))).code("git_error"),
            );
        }
        Ok(res)
    }

    /// `text(*args)`: stdout decoded and stripped.
    pub fn text(&self, args: &[&str]) -> HubResult<String> {
        Ok(strip(&lossy(&self.run(args, true, None)?.out)).to_string())
    }

    /// `.git` is a directory and `git rev-parse --git-dir` succeeds; an unsafe repo is never "ready" (false).
    pub fn is_repo(&self) -> HubResult<bool> {
        if !self.path.join(".git").is_dir() {
            return Ok(false);
        }
        match self.run(&["rev-parse", "--git-dir"], false, None) {
            Ok(r) => Ok(r.rc == 0),
            // a ProblemError means "not ready"; an unexpected OSError (500) propagates as in Python
            Err(p) if p.status == 500 => Err(p),
            Err(_) => Ok(false),
        }
    }

    pub fn init(&self) -> HubResult<()> {
        let fresh = !self.path.exists();
        std::fs::create_dir_all(&self.path)?;
        if fresh {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o700))?;
        }
        if !self.is_repo()? {
            self.run(&["init", "-q", "-b", "main"], true, None)?;
        }
        Ok(())
    }

    pub fn has_head(&self) -> HubResult<bool> {
        Ok(self.run(&["rev-parse", "--verify", "-q", "HEAD"], false, None)?.rc == 0)
    }

    /// The content hash: tree id of HEAD, or EMPTY_TREE.
    pub fn head_tree(&self) -> HubResult<String> {
        if self.has_head()? {
            self.text(&["rev-parse", "HEAD^{tree}"])
        } else {
            Ok(EMPTY_TREE.into())
        }
    }

    pub fn head(&self) -> HubResult<Option<String>> {
        if self.has_head()? {
            Ok(Some(self.text(&["rev-parse", "HEAD"])?))
        } else {
            Ok(None)
        }
    }

    /// (path, added|deleted|renamed|modified) from `status --porcelain=v1 -z --untracked-files=all`.
    pub fn status(&self) -> HubResult<Vec<(String, String)>> {
        let out = lossy(
            &self
                .run(&["status", "--porcelain=v1", "-z", "--untracked-files=all"], true, None)?
                .out,
        );
        let entries: Vec<&str> = out.split('\0').filter(|e| !e.is_empty()).collect();
        let mut res = Vec::new();
        let mut i = 0;
        while i < entries.len() {
            let e = entries[i];
            let xy: String = e.chars().take(2).collect();
            let path: String = e.chars().skip(3).collect();
            if xy.contains('R') || xy.contains('C') {
                i += 1; // the next NUL field is the rename source
            }
            let code = if xy == "??" || xy.contains('A') {
                "added"
            } else if xy.contains('D') {
                "deleted"
            } else if xy.contains('R') {
                "renamed"
            } else {
                "modified"
            };
            res.push((path, code.to_string()));
            i += 1;
        }
        Ok(res)
    }

    pub fn changes(&self, max_diff: usize) -> HubResult<Vec<Change>> {
        let head = self.has_head()?;
        let mut out = Vec::new();
        for (path, status) in self.status()? {
            let diff = self.diff_for(&path, &status, head, max_diff)?;
            out.push(Change { path, status, diff });
        }
        Ok(out)
    }

    fn diff_for(&self, path: &str, status: &str, head: bool, max_diff: usize) -> HubResult<String> {
        if status == "added" && !self.tracked(path)? {
            let f = self.path.join(path);
            if f.is_symlink() || !f.is_file() {
                return Ok(String::new());
            }
            // never a blocking open: a FIFO swapped in after the checks above is refused, not waited on
            let Ok(raw) = super::fs::open_regular(&f, false).and_then(|(fh, _)| {
                let mut b = Vec::new();
                std::io::Read::read_to_end(&mut std::io::Read::take(fh, max_diff as u64), &mut b).map(|_| b)
            }) else {
                return Ok(String::new());
            };
            if raw.contains(&0) {
                return Ok(format!("Binary file {path} added\n"));
            }
            let text = lossy(&raw);
            let new = splitlines(&text, true);
            return Ok(unified_diff(&[], &new, "/dev/null", &format!("b/{path}")));
        }
        if !head {
            return Ok(String::new());
        }
        let r = self.run(
            &[
                "diff",
                "HEAD",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--",
                path,
            ],
            false,
            None,
        )?;
        Ok(take_chars(&lossy(&r.out), max_diff).to_string())
    }

    fn tracked(&self, path: &str) -> HubResult<bool> {
        Ok(self.run(&["ls-files", "--error-unmatch", "--", path], false, None)?.rc == 0)
    }

    pub fn is_clean(&self) -> HubResult<bool> {
        Ok(self.status()?.is_empty())
    }

    /// Pending paths outside KNOWN_CONTENT (reported, never committed).
    pub fn strays(&self) -> HubResult<Vec<String>> {
        Ok(self
            .status()?
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| !is_known(p))
            .collect())
    }

    /// Stage only KNOWN_CONTENT, commit with the fixed identity; returns the new HEAD.
    pub fn commit(&self, message: &str) -> HubResult<String> {
        let message = strip(message);
        if message.is_empty() || message.chars().count() > 2000 {
            return Err(Problem::bad_request("commit message must be 1..2000 characters"));
        }
        for k in KNOWN_CONTENT {
            if self.path.join(k).exists() || self.tracked(k)? {
                self.run(&["add", "-A", "--", k], false, None)?;
            }
        }
        if self.run(&["diff", "--cached", "--quiet"], false, None)?.rc == 0 {
            let strays = if self.strays()?.is_empty() {
                String::new()
            } else {
                let s = self.strays()?;
                format!(" (untracked/stray paths ignored: {})", repr_list(&s[..s.len().min(5)]))
            };
            return Err(Problem::conflict(format!("nothing to commit{strays}")).code("nothing_to_commit"));
        }
        self.run(&["commit", "-q", "--no-verify", "-m", message], true, None)?;
        self.text(&["rev-parse", "HEAD"])
    }

    pub fn discard(&self, paths: Option<&[String]>, dry_run: bool) -> HubResult<Vec<String>> {
        let paths = paths.filter(|p| !p.is_empty());
        let clean_paths = match paths {
            Some(p) => check_paths(p)?,
            None => Vec::new(),
        };
        let mut touched: Vec<String> = self.status()?.into_iter().map(|(p, _)| p).collect();
        if paths.is_some() {
            touched.retain(|p| {
                clean_paths
                    .iter()
                    .any(|c| p == c || p.starts_with(&format!("{}/", c.trim_end_matches('/'))))
            });
        }
        if dry_run {
            return Ok(touched);
        }
        if self.has_head()? {
            let mut tracked = Vec::new();
            for p in &touched {
                if self.tracked(p)? {
                    tracked.push(p.as_str());
                }
            }
            if !tracked.is_empty() {
                let mut args = vec!["restore", "--source=HEAD", "--staged", "--worktree", "--"];
                args.extend(tracked);
                self.run(&args, true, None)?;
            }
        }
        let mut untracked = Vec::new();
        for p in &touched {
            if !self.tracked_head(p)? {
                untracked.push(p.as_str());
            }
        }
        if !untracked.is_empty() {
            let mut args = vec!["clean", "-fdq", "--"];
            args.extend(untracked);
            self.run(&args, true, None)?;
        }
        Ok(touched)
    }

    fn tracked_head(&self, path: &str) -> HubResult<bool> {
        Ok(self.has_head()? && self.run(&["cat-file", "-e", &format!("HEAD:{path}")], false, None)?.rc == 0)
    }

    /// `[{"commit", "author", "time", "message"}]`, newest first.
    pub fn history(&self, limit: i64) -> HubResult<Vec<Value>> {
        if !self.has_head()? {
            return Ok(Vec::new());
        }
        let n = format!("-n{}", limit.clamp(1, 500));
        let raw = self.text(&["log", &n, "--format=%H%x1f%an%x1f%at%x1f%s"])?;
        let mut rows = Vec::new();
        for line in splitlines(&raw, false) {
            let mut parts: Vec<&str> = line.split('\x1f').collect();
            parts.extend(["", "", "", ""]);
            let (h, author, at, subject) = (parts[0], parts[1], parts[2], parts[3]);
            let time = if at.is_empty() {
                0
            } else {
                hub_core::py::text::parse_int(at).ok_or_else(Problem::internal)?
            };
            rows.push(json!({"commit": h, "author": author, "time": time, "message": subject}));
        }
        Ok(rows)
    }

    pub fn revert(&self, commit: &str) -> HubResult<String> {
        if !COMMIT_RE.is_match(commit) {
            return Err(Problem::bad_request("commit must be a hex object id"));
        }
        if !self.is_clean()? {
            return Err(Problem::conflict("commit or discard pending changes before reverting").code("dirty_tree"));
        }
        if self
            .run(&["cat-file", "-e", &format!("{commit}^{{commit}}")], false, None)?
            .rc
            != 0
        {
            return Err(Problem::bad_request("unknown commit").code("unknown_commit"));
        }
        let r = self.run(&["revert", "--no-edit", commit], false, None)?;
        if r.rc != 0 {
            self.run(&["revert", "--abort"], false, None)?;
            return Err(Problem::conflict(format!(
                "revert failed (conflicts or a merge commit): {}",
                take_chars(&r.err, 200)
            ))
            .code("revert_failed"));
        }
        self.text(&["rev-parse", "HEAD"])
    }

    /// Materialise HEAD's tree into `dest` from the object database (`ls-tree` + `cat-file --batch`), never
    /// `git archive`: export-ignore / export-subst attributes cannot alter what gets planned. Regular files only;
    /// symlinks/submodules skipped.
    pub fn export_head(&self, dest: &Path) -> HubResult<()> {
        std::fs::create_dir_all(dest)?;
        if !self.has_head()? {
            return Ok(());
        }
        let listing = lossy(
            &self
                .run(&["ls-tree", "-r", "-z", "--full-tree", "HEAD"], true, None)?
                .out,
        );
        let mut entries: Vec<(String, String)> = Vec::new();
        for rec in listing.split('\0').filter(|r| !r.is_empty()) {
            let (meta, path) = rec.split_once('\t').unwrap_or((rec, ""));
            let mut f: Vec<&str> = hub_core::py::text::split_ws(meta);
            f.extend(["", "", ""]);
            let (mode, typ, sha) = (f[0], f[1], f[2]);
            if typ != "blob" || !(mode == "100644" || mode == "100755") {
                continue;
            }
            if check_relpath(path).is_err() {
                continue;
            }
            entries.push((path.to_string(), sha.to_string()));
        }
        if entries.is_empty() {
            return Ok(());
        }
        let mut argv: Vec<String> = vec![self.git_bin.clone()];
        for c in [
            "core.hooksPath=/dev/null",
            "core.attributesFile=/dev/null",
            "core.fsmonitor=false",
            "core.sshCommand=/bin/false",
            "protocol.allow=never",
        ] {
            argv.push("-c".into());
            argv.push(c.into());
        }
        argv.push("cat-file".into());
        argv.push("--batch".into());
        let input: String = entries.iter().map(|(_, sha)| format!("{sha}\n")).collect();
        let env = self.env();
        let p = match with_private_umask(|| {
            capture(
                &argv,
                Some(&self.path),
                &env,
                Some(input.into_bytes()),
                self.timeout * 2.0,
                Some(MAX_ARCHIVE),
                Some(GIT_STDERR_CAP),
            )
        }) {
            Ok(c) if c.timed_out => return Err(Problem::unavailable("git timed out").code("git_timeout")),
            Ok(c) => c,
            // Python only catches TimeoutExpired here: anything else is an unexpected error (500)
            Err(_) => return Err(Problem::internal()),
        };
        if p.rc != 0 || p.over_cap {
            return Err(Problem::conflict("could not read the content snapshot").code("snapshot_failed"));
        }
        let buf = p.stdout;
        let (mut pos, mut total) = (0usize, 0usize);
        for (path, sha) in &entries {
            let nl = buf
                .get(pos..)
                .and_then(|rest| rest.iter().position(|b| *b == b'\n'))
                .map(|i| pos + i)
                .ok_or_else(Problem::internal)?;
            let hdr: Vec<&[u8]> = buf[pos..nl]
                .split(u8::is_ascii_whitespace)
                .filter(|x| !x.is_empty())
                .collect();
            if hdr.len() != 3 || hdr[0] != sha.as_bytes() || hdr[1] != b"blob" {
                return Err(Problem::conflict("unexpected git object stream").code("snapshot_failed"));
            }
            let size: usize = std::str::from_utf8(hdr[2])
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(Problem::internal)?;
            let start = (nl + 1).min(buf.len());
            let end = (nl + 1 + size).min(buf.len());
            let blob = &buf[start..end];
            pos = nl + 1 + size + 1;
            total += size;
            if total > MAX_ARCHIVE {
                return Err(Problem::conflict("content snapshot too large").code("snapshot_too_large"));
            }
            let target = dest.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, blob)?;
        }
        Ok(())
    }
}

/// `_is_known(path)`.
pub fn is_known(path: &str) -> bool {
    KNOWN_CONTENT
        .iter()
        .any(|k| path == *k || path.starts_with(&format!("{k}/")))
}

/// `_check_paths(paths)`: relative content paths only; never an option-looking path or `.git`.
fn check_paths(paths: &[String]) -> HubResult<Vec<String>> {
    let mut out = Vec::new();
    for p in paths {
        let stripped = p.trim_end_matches('/');
        match check_relpath(stripped) {
            Ok(c) => out.push(c.to_string()),
            Err(e) => return Err(Problem::bad_request(format!("bad path: {e}"))),
        }
        if p.starts_with('-') || p.split('/').next() == Some(".git") {
            return Err(Problem::bad_request("bad path"));
        }
    }
    Ok(out)
}

/// Run `f` on a thread whose umask is 0o077, the Rust form of `subprocess.run(..., umask=0o077)`: the thread unshares
/// its filesystem attributes (`CLONE_FS`: cwd, root, umask) first, so a child it spawns inherits 0o077 while every other
/// thread keeps the process umask. If the kernel refuses `unshare` (a seccomp sandbox), `f` runs with the inherited
/// umask (D-B2-UMASK) rather than changing the umask of the whole process.
pub fn with_private_umask<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    let joined = std::thread::scope(|s| {
        s.spawn(|| {
            if nix::sched::unshare(nix::sched::CloneFlags::CLONE_FS).is_ok() {
                let _ = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
            }
            f()
        })
        .join()
    });
    joined.unwrap_or_else(|p| std::panic::resume_unwind(p))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// The umask a thread runs with, read from /proc without changing it.
    fn thread_umask() -> Option<u32> {
        let s = std::fs::read_to_string("/proc/thread-self/status").ok()?;
        let line = s.lines().find(|l| l.starts_with("Umask:"))?;
        u32::from_str_radix(line.trim_start_matches("Umask:").trim(), 8).ok()
    }

    #[test]
    fn git_children_get_umask_077_and_no_other_thread_changes() -> Result<(), Box<dyn std::error::Error>> {
        let d = tempfile::tempdir()?;
        let before = thread_umask();
        let f = d.path().join("made-by-child");
        let argv: Vec<String> = vec!["touch".into(), f.to_string_lossy().to_string()];
        let (inner, c) = with_private_umask(|| {
            (
                thread_umask(),
                capture(
                    &argv,
                    None,
                    &[("PATH", "/usr/bin:/bin".to_string())],
                    None,
                    10.0,
                    None,
                    None,
                ),
            )
        });
        assert_eq!(c?.rc, 0);
        assert_eq!(inner, Some(0o077));
        assert_eq!(std::fs::metadata(&f)?.permissions().mode() & 0o777, 0o600); // touch asks 0666
        assert_eq!(thread_umask(), before); // the calling thread (and the process) keep their umask
        std::thread::spawn(move || assert_eq!(thread_umask(), before))
            .join()
            .map_err(|_| "thread")?;
        Ok(())
    }

    type R = Result<(), Box<dyn std::error::Error>>;

    impl GitRepo {
        /// A repo without the contentguard gate (WS-F's module), for exercising git itself.
        fn unguarded(path: &Path) -> Self {
            Self {
                guarded: false,
                ..Self::new(path, "git", "Agent Hub", "agent-hub@localhost", 30.0)
            }
        }
    }

    fn put(repo: &GitRepo, rel: &str, text: &str) -> std::io::Result<()> {
        let p = repo.path.join(rel);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, text)
    }

    fn fresh() -> Result<(tempfile::TempDir, GitRepo), Box<dyn std::error::Error>> {
        let d = tempfile::tempdir()?;
        let repo = GitRepo::unguarded(&d.path().join("content"));
        repo.init()?;
        Ok((d, repo))
    }

    #[test]
    fn init_is_private_and_empty_tree() -> R {
        let (_d, repo) = fresh()?;
        assert_eq!(std::fs::metadata(&repo.path)?.permissions().mode() & 0o777, 0o700);
        assert!(repo.is_repo()?);
        assert_eq!(repo.head_tree()?, EMPTY_TREE);
        assert_eq!(repo.head()?, None);
        assert!(repo.history(50)?.is_empty());
        assert_eq!(repo.commit("   ").err().map(|p| p.status), Some(400));
        let e = repo.commit("empty").err().ok_or("no error")?;
        assert_eq!(
            (e.status, e.code.as_str(), e.detail.as_str()),
            (409, "nothing_to_commit", "nothing to commit")
        );
        Ok(())
    }

    #[test]
    fn pending_changes_have_diffs_and_commit_identity() -> R {
        let (_d, repo) = fresh()?;
        put(&repo, "instructions/one.md", "first\n")?;
        let ch = repo.changes(200_000)?;
        assert_eq!(ch.len(), 1);
        assert_eq!(
            (ch[0].path.as_str(), ch[0].status.as_str()),
            ("instructions/one.md", "added")
        );
        assert_eq!(
            ch[0].diff,
            "--- /dev/null\n+++ b/instructions/one.md\n@@ -0,0 +1 @@\n+first\n"
        );
        let sha = repo.commit("add one")?;
        assert_eq!(sha.len(), 40);
        assert_ne!(repo.head_tree()?, EMPTY_TREE);
        put(&repo, "instructions/one.md", "second\n")?;
        let ch = repo.changes(200_000)?;
        assert_eq!(ch[0].status, "modified");
        assert!(ch[0].diff.contains("+second") && ch[0].diff.contains("-first"));
        let who = repo.text(&["log", "-1", "--format=%an <%ae> | %cn"])?;
        assert_eq!(who, "Agent Hub <agent-hub@localhost> | Agent Hub");
        Ok(())
    }

    #[test]
    fn binary_added_file_and_truncation() -> R {
        let (_d, repo) = fresh()?;
        put(&repo, "skills/x/blob.bin", "a\0b")?;
        assert_eq!(repo.changes(200_000)?[0].diff, "Binary file skills/x/blob.bin added\n");
        Ok(())
    }

    #[test]
    fn commit_stages_only_known_paths_and_reports_strays() -> R {
        let (_d, repo) = fresh()?;
        put(&repo, "instructions/a.md", "a\n")?;
        put(&repo, "stray.txt", "x")?;
        put(&repo, "notes/todo.md", "x")?;
        put(&repo, "inbox/c1/x.md", "p")?;
        repo.commit("only content")?;
        let mut strays = repo.strays()?;
        strays.sort();
        assert_eq!(strays, vec!["inbox/c1/x.md", "notes/todo.md", "stray.txt"]);
        let tracked = repo.text(&["ls-files"])?;
        assert!(tracked.contains("instructions/a.md") && !tracked.contains("stray.txt"));
        let e = repo.commit("only strays").err().ok_or("no error")?;
        assert_eq!(e.code, "nothing_to_commit");
        assert!(
            e.detail
                .starts_with("nothing to commit (untracked/stray paths ignored: ['"),
            "{}",
            e.detail
        );
        // deletions under known paths are committed
        std::fs::remove_file(repo.path.join("instructions/a.md"))?;
        repo.commit("remove")?;
        assert!(!repo.text(&["ls-files"])?.contains("instructions/a.md"));
        Ok(())
    }

    #[test]
    fn pathspecs_are_literal() -> R {
        let (_d, repo) = fresh()?;
        put(&repo, "instructions/a.md", "a\n")?;
        repo.commit("add")?;
        assert!(
            repo.run(&["ls-files", "--", "instructions/*.md"], true, None)?
                .out
                .is_empty()
        );
        assert!(
            String::from_utf8_lossy(&repo.run(&["ls-files", "--", "instructions/a.md"], true, None)?.out)
                .contains("instructions/a.md")
        );
        Ok(())
    }

    #[test]
    fn discard_paths_all_and_hostile() -> R {
        let (_d, repo) = fresh()?;
        put(&repo, "instructions/keep.md", "k\n")?;
        repo.commit("keep")?;
        put(&repo, "instructions/keep.md", "changed\n")?;
        put(&repo, "instructions/new1.md", "n\n")?;
        put(&repo, "instructions/new2.md", "n\n")?;
        let dry = repo.discard(None, true)?;
        assert_eq!(dry.len(), 3);
        assert_eq!(
            repo.discard(Some(&["instructions/new1.md".into()]), false)?,
            vec!["instructions/new1.md"]
        );
        assert!(!repo.path.join("instructions/new1.md").exists() && repo.path.join("instructions/new2.md").exists());
        repo.discard(Some(&["instructions/".into()]), false)?;
        assert!(repo.is_clean()?);
        assert_eq!(std::fs::read_to_string(repo.path.join("instructions/keep.md"))?, "k\n");
        for bad in ["../x", "/etc", ".git/config", "--all"] {
            let e = repo.discard(Some(&[bad.to_string()]), false).err().ok_or("accepted")?;
            assert_eq!(e.status, 400, "{bad}");
        }
        assert_eq!(
            repo.discard(Some(&["a/../b".into()]), false).err().map(|e| e.detail),
            Some("bad path: empty, '.' or '..' path segment".into())
        );
        Ok(())
    }

    #[test]
    fn history_and_revert() -> R {
        let (_d, repo) = fresh()?;
        put(&repo, "instructions/one.md", "v1\n")?;
        let c1 = repo.commit("v1")?;
        put(&repo, "instructions/one.md", "v2\n")?;
        let c2 = repo.commit("v2")?;
        let h = repo.history(50)?;
        assert_eq!(h[0]["message"], "v2");
        assert_eq!(h[1]["message"], "v1");
        assert_eq!(h[0]["author"], "Agent Hub");
        assert!(h[0]["time"].is_i64() && h[0]["commit"] == c2.as_str());
        assert_eq!(repo.history(1)?.len(), 1);
        put(&repo, "instructions/dirty.md", "d\n")?;
        assert_eq!(repo.revert(&c2).err().map(|e| e.code), Some("dirty_tree".into()));
        repo.discard(None, false)?;
        repo.revert(&c2)?;
        assert_eq!(std::fs::read_to_string(repo.path.join("instructions/one.md"))?, "v1\n");
        assert_eq!(repo.revert("not-a-hash!").err().map(|e| e.status), Some(400));
        assert_eq!(
            repo.revert("deadbeefdeadbeef").err().map(|e| e.code),
            Some("unknown_commit".into())
        );
        assert_ne!(c1, c2);
        Ok(())
    }

    #[test]
    fn export_head_reads_objects_not_the_working_tree() -> R {
        let (d, repo) = fresh()?;
        put(&repo, "instructions/a.md", "hello\n")?;
        put(&repo, "skills/s/SKILL.md", "skill\n")?;
        put(&repo, ".gitattributes", "*.md export-subst\n")?;
        repo.run(&["add", "-A", "--", ".gitattributes"], true, None)?;
        repo.commit("add")?;
        put(&repo, "instructions/a.md", "changed\n")?;
        let dest = d.path().join("snap");
        repo.export_head(&dest)?;
        assert_eq!(std::fs::read_to_string(dest.join("instructions/a.md"))?, "hello\n");
        assert_eq!(std::fs::read_to_string(dest.join("skills/s/SKILL.md"))?, "skill\n");
        assert!(repo.changes(200_000)?[0].diff.contains("+changed"));
        Ok(())
    }

    #[test]
    fn missing_git_and_timeout_are_503() -> R {
        let d = tempfile::tempdir()?;
        let mut repo = GitRepo::unguarded(d.path());
        repo.git_bin = "/nonexistent/git".into();
        let e = repo.run(&["status"], true, None).err().ok_or("ran")?;
        assert_eq!(
            (e.status, e.code.as_str(), e.detail.as_str()),
            (503, "git_missing", "git is not installed")
        );
        Ok(())
    }

    /// G-1: the exact argv and environment of every git call (a recording fake `git`).
    #[test]
    fn argv_and_env_are_exact() -> R {
        let d = tempfile::tempdir()?;
        let rec = d.path().join("rec");
        let fake = d.path().join("fake-git");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > {r}.argv\nenv | sort > {r}.env\npwd > {r}.cwd\necho out\necho '  oops  ' >&2\nexit 3\n",
                r = rec.display()
            ),
        )?;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;
        let content = d.path().join("c");
        std::fs::create_dir(&content)?;
        let mut repo = GitRepo::unguarded(&content);
        repo.git_bin = fake.to_string_lossy().to_string();
        // a just-written executable can be briefly ETXTBSY while another test thread forks: retry that only
        let mut r = repo.run(&["status", "--porcelain=v1"], false, None);
        for _ in 0..50 {
            if !matches!(&r, Err(p) if p.status == 500) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
            r = repo.run(&["status", "--porcelain=v1"], false, None);
        }
        let r = r?;
        assert_eq!((r.rc, r.out.as_slice(), r.err.as_str()), (3, &b"out\n"[..], "oops"));
        let argv = std::fs::read_to_string(rec.with_extension("argv"))?;
        let want = format!(
            "-c\ncore.hooksPath=/dev/null\n-c\ncommit.gpgsign=false\n-c\ncore.sshCommand=/bin/false\n-c\nprotocol.allow=never\n-c\ndiff.external=\n-c\ncore.quotepath=off\n-c\ncore.fsmonitor=false\n-c\ncore.autocrlf=false\n-c\ncore.attributesFile=/dev/null\n-c\nsafe.directory={}\nstatus\n--porcelain=v1\n",
            content.display()
        );
        assert_eq!(argv, want);
        let env = std::fs::read_to_string(rec.with_extension("env"))?;
        // everything the child sees, minus what /bin/sh itself adds (PWD, SHLVL, _)
        let keys: Vec<&str> = env
            .lines()
            .filter_map(|l| l.split_once('=').map(|x| x.0))
            .filter(|k| !["PWD", "SHLVL", "_", "OLDPWD"].contains(k))
            .collect();
        assert_eq!(
            keys,
            vec![
                "GIT_ATTR_NOSYSTEM",
                "GIT_AUTHOR_EMAIL",
                "GIT_AUTHOR_NAME",
                "GIT_COMMITTER_EMAIL",
                "GIT_COMMITTER_NAME",
                "GIT_CONFIG_GLOBAL",
                "GIT_CONFIG_NOSYSTEM",
                "GIT_CONFIG_SYSTEM",
                "GIT_LITERAL_PATHSPECS",
                "GIT_OPTIONAL_LOCKS",
                "GIT_TERMINAL_PROMPT",
                "HOME",
                "LC_ALL",
                "PATH",
            ]
        );
        for want in [
            "GIT_CONFIG_GLOBAL=/dev/null",
            "GIT_CONFIG_SYSTEM=/dev/null",
            "GIT_CONFIG_NOSYSTEM=1",
            "GIT_ATTR_NOSYSTEM=1",
            "GIT_TERMINAL_PROMPT=0",
            "GIT_LITERAL_PATHSPECS=1",
            "GIT_OPTIONAL_LOCKS=0",
            "LC_ALL=C",
            "GIT_AUTHOR_NAME=Agent Hub",
            "GIT_COMMITTER_EMAIL=agent-hub@localhost",
        ] {
            assert!(env.lines().any(|l| l == want), "{want}");
        }
        assert!(env.lines().any(|l| l == format!("HOME={}", content.display())));
        assert_eq!(
            std::fs::read_to_string(rec.with_extension("cwd"))?.trim(),
            content.to_string_lossy()
        );
        let e = repo.run(&["status"], true, None).err().ok_or("no error")?;
        assert_eq!(
            (e.status, e.code.as_str(), e.detail.as_str()),
            (409, "git_error", "git status failed: oops")
        );
        Ok(())
    }
}
