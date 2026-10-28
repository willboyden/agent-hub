//! Port of services/contentguard.py: the content repo is the approval boundary, so it must not be writable by the
//! agents it governs; refuse a content dir inside a client root or the app tree, not owned by the user, group/world
//! writable, or whose git metadata (config keys, hooks, attributes, .gitattributes) is outside the allowlist.
//! Errors: ProblemError 503 `content_dir_unsafe`, detail "content directory is not safe: <msg>. <FIX>".
//! Security control S-CONTENTGUARD. Owner: WS-F.
//!
//! An OSError Python does not catch (a permission error on stat, say) becomes `Problem::internal()` here: in Python it
//! escapes to the API as an unexpected error (HTTP 500).
//!
//! Deliberate differences (security, reported to the lead as Python bugs SB-1/SB-2):
//! - `sanitize_git` never writes or deletes THROUGH a symlink. Python follows a symlinked `.git/hooks` (deleting every
//!   non-sample file, and rmtree-ing every directory, of the link's target), and truncates or overwrites the target of
//!   a symlinked `.git/config`, `.git/info/attributes` or `.gitattributes`, i.e. files outside the copy it sanitises.
//!   Rust removes the link itself and reports it (`config (symlink)`, `hooks (symlink)`, `info (symlink)`,
//!   `info/attributes`, `<relpath>`).
//! - `verify_git_metadata` refuses a symlinked `.git/config` without reading it (Python reads the link's target and
//!   echoes its first "keys" -- the text before `=` of up to six lines, e.g. of a private key file -- in the error).

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use hub_core::{HubResult, Problem};

use super::targets::within;

pub const CODE: &str = "content_dir_unsafe";
pub const FIX: &str = "Keep the content repo in a private directory outside every client root and the app tree (default ~/.local/share/agent-hub/content, mode 0700) and move it with `hubctl migrate-content --to PATH`.";
pub const ALLOWED_CONFIG_KEYS: [&str; 8] = [
    "core.repositoryformatversion",
    "core.filemode",
    "core.bare",
    "core.logallrefupdates",
    "core.ignorecase",
    "core.precomposeunicode",
    "user.name",
    "user.email",
];

fn unsafe_dir(msg: &str) -> Problem {
    Problem::new(503, CODE, format!("content directory is not safe: {msg}. {FIX}"))
}

fn real(p: &Path) -> PathBuf {
    hub_core::py::os::realpath(p)
}

/// pathlib's `_ignore_error` (ENOENT, ENOTDIR, EBADF, ELOOP) plus the ValueError of a non-encodable path.
fn ignored(e: &io::Error) -> bool {
    use nix::errno::Errno;
    e.kind() == io::ErrorKind::NotFound
        || e.kind() == io::ErrorKind::InvalidInput
        || matches!(e.raw_os_error(), Some(c) if c == Errno::ENOTDIR as i32 || c == Errno::EBADF as i32 || c == Errno::ELOOP as i32)
}

#[allow(clippy::needless_pass_by_value)] // used as `map_err(oserror)`
fn oserror(_e: io::Error) -> Problem {
    Problem::internal()
}

/// `Path.stat()` / `Path.lstat()` the way pathlib's predicates use them: `None` for an ignored error.
fn stat(p: &Path, follow: bool) -> HubResult<Option<fs::Metadata>> {
    let r = if follow {
        fs::metadata(p)
    } else {
        fs::symlink_metadata(p)
    };
    match r {
        Ok(m) => Ok(Some(m)),
        Err(e) if ignored(&e) => Ok(None),
        Err(e) => Err(oserror(e)),
    }
}

fn exists(p: &Path) -> HubResult<bool> {
    Ok(stat(p, true)?.is_some())
}

fn is_dir(p: &Path) -> HubResult<bool> {
    Ok(stat(p, true)?.is_some_and(|m| m.is_dir()))
}

fn is_file(p: &Path) -> HubResult<bool> {
    Ok(stat(p, true)?.is_some_and(|m| m.is_file()))
}

fn is_symlink(p: &Path) -> HubResult<bool> {
    Ok(stat(p, false)?.is_some_and(|m| m.file_type().is_symlink()))
}

/// `Path.read_text(errors="replace")` (UTF-8 with U+FFFD for bad bytes).
fn read_text_lossy(p: &Path) -> HubResult<String> {
    fs::read(p)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .map_err(oserror)
}

/// `Path.read_text()` (strict UTF-8; an OSError or a decoding error is `None` for the callers here).
fn read_text_strict(p: &Path) -> Option<String> {
    fs::read(p).ok().and_then(|b| String::from_utf8(b).ok())
}

/// `os.path.getsize` (follows symlinks; raises).
fn getsize(p: &Path) -> HubResult<u64> {
    fs::metadata(p).map(|m| m.len()).map_err(oserror)
}

/// Entry names of a directory in `os.scandir` order (`Path.iterdir`); raises like Python.
fn listdir(p: &Path) -> HubResult<Vec<String>> {
    let rd = fs::read_dir(p).map_err(oserror)?;
    let mut out = Vec::new();
    for e in rd {
        out.push(e.map_err(oserror)?.file_name().to_string_lossy().into_owned());
    }
    Ok(out)
}

/// `os.walk(top)` (top-down, scandir order, symlinked directories listed but never entered, scandir errors skipped):
/// yields `(dirpath, dirnames, filenames)`; `prune` removes names from `dirnames` before descending.
type Visit<'a> = dyn FnMut(&Path, &[String], &[String]) -> HubResult<()> + 'a;

fn walk(top: &Path, prune: &dyn Fn(&str) -> bool, visit: &mut Visit<'_>) -> HubResult<()> {
    let mut stack: Vec<PathBuf> = vec![top.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        let (mut dirs, mut files) = (Vec::new(), Vec::new());
        let mut broken = false;
        for e in rd {
            let Ok(e) = e else {
                broken = true;
                break;
            };
            let name = e.file_name().to_string_lossy().into_owned();
            // DirEntry.is_dir() follows symlinks
            let is_dir = fs::metadata(e.path()).map(|m| m.is_dir()).unwrap_or(false);
            if is_dir { dirs.push(name) } else { files.push(name) }
        }
        if broken {
            continue;
        }
        dirs.retain(|d| !prune(d));
        visit(&dir, &dirs, &files)?;
        for d in dirs.iter().rev() {
            let p = dir.join(d);
            if !fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
                stack.push(p);
            }
        }
    }
    Ok(())
}

/// `oct(mode)`.
fn oct(mode: u32) -> String {
    format!("0o{mode:o}")
}

/// `check_location(content_dir, client_roots, app_root)`: the content dir must not be inside the app tree or a
/// client root, and no client root may be inside it.
pub fn check_location(content_dir: &Path, client_roots: &[String], app_root: &Path) -> HubResult<()> {
    let cd = real(content_dir);
    let app = real(app_root);
    if within(&cd, &app) {
        return Err(unsafe_dir(&format!(
            "{} is inside the app tree {}, which agents can write",
            cd.display(),
            app.display()
        )));
    }
    for r in client_roots {
        let rr = real(Path::new(r));
        if within(&cd, &rr) {
            return Err(unsafe_dir(&format!(
                "{} is inside the client root {}",
                cd.display(),
                rr.display()
            )));
        }
        if within(&rr, &cd) {
            return Err(unsafe_dir(&format!(
                "the client root {} is inside the content directory",
                rr.display()
            )));
        }
    }
    Ok(())
}

fn check_owner_mode(p: &Path, uid: u32) -> HubResult<()> {
    let st = match fs::symlink_metadata(p) {
        Ok(st) => st,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(oserror(e)),
    };
    if st.uid() != uid {
        return Err(unsafe_dir(&format!("{} is not owned by the current user", p.display())));
    }
    if st.mode() & 0o022 != 0 {
        return Err(unsafe_dir(&format!(
            "{} is group- or world-writable (mode {})",
            p.display(),
            oct(st.mode() & 0o777)
        )));
    }
    Ok(())
}

/// `check_ownership(content_dir)`: the dir, `.git` and its hooks/info/objects/refs are the user's and not
/// group/world-writable; `.git` is a real directory.
pub fn check_ownership(content_dir: &Path) -> HubResult<()> {
    check_ownership_as(content_dir, nix::unistd::getuid().as_raw())
}

/// `check_ownership` for an explicit uid (Python's tests monkeypatch `os.getuid`).
pub fn check_ownership_as(content_dir: &Path, uid: u32) -> HubResult<()> {
    check_owner_mode(content_dir, uid)?;
    let git = content_dir.join(".git");
    if exists(&git)? || is_symlink(&git)? {
        if is_symlink(&git)? || !is_dir(&git)? {
            return Err(unsafe_dir(
                ".git must be a real directory (not a symlink or gitdir file)",
            ));
        }
        check_owner_mode(&git, uid)?;
        for sub in ["hooks", "info", "objects", "refs"] {
            check_owner_mode(&git.join(sub), uid)?;
        }
    }
    Ok(())
}

/// Keys (`section.key`; any `[section "sub"]` as `section "sub".*`) of a git config file, sorted and unique (Python
/// returns a set; every caller sorts it).
pub fn parse_config_keys(text: &str) -> Vec<String> {
    use hub_core::py::text::{split_ws, splitlines, strip};
    let mut keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut section = String::new();
    for raw in splitlines(text, false) {
        let line = strip(raw);
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(after) = line.strip_prefix('[') {
            // Python: end = line.find("]"); head = line[1:end] if end > 0 else line[1:]
            let end = after.find(']');
            let head = end.map_or(after, |e| &after[..e]);
            let hs = strip(head);
            section = if hs.is_empty() {
                String::new()
            } else {
                split_ws(head).first().copied().unwrap_or("").to_lowercase()
            };
            if hs.contains(' ') || head.contains('"') {
                keys.insert(format!("{}.*", hs.to_lowercase()));
            }
            let rest = end.map_or("", |e| strip(&after[e + 1..]));
            if !rest.is_empty() && rest.contains('=') {
                let k = rest.split_once('=').map_or(rest, |(k, _)| k);
                keys.insert(format!("{section}.{}", strip(k).to_lowercase()));
            }
            continue;
        }
        let key = line.split_once('=').map_or(line, |(k, _)| k);
        keys.insert(format!("{section}.{}", strip(key).to_lowercase()));
    }
    keys.into_iter().collect()
}

fn list_repr(items: &[String], n: usize) -> String {
    super::floor::list_repr(&items[..items.len().min(n)])
}

/// `verify_git_metadata(content_dir)`: config keys within the allowlist, no `.git/info/attributes`, only sample
/// hooks, no non-empty `.gitattributes` anywhere in the tree.
pub fn verify_git_metadata(content_dir: &Path) -> HubResult<()> {
    let git = content_dir.join(".git");
    if !is_dir(&git)? {
        return Ok(());
    }
    let cfg = git.join("config");
    if is_symlink(&cfg)? {
        // deviation SB-2: never read the target of a planted link (its text would be echoed in the error)
        return Err(unsafe_dir(".git/config is a symlink"));
    }
    if exists(&cfg)? {
        let bad: Vec<String> = parse_config_keys(&read_text_lossy(&cfg)?)
            .into_iter()
            .filter(|k| !ALLOWED_CONFIG_KEYS.contains(&k.as_str()))
            .collect();
        if !bad.is_empty() {
            return Err(unsafe_dir(&format!(
                ".git/config contains keys outside the allowlist: {}",
                list_repr(&bad, 6)
            )));
        }
    }
    let attrs = git.join("info").join("attributes");
    if exists(&attrs)? && getsize(&attrs)? > 0 {
        return Err(unsafe_dir(".git/info/attributes is not empty"));
    }
    let hooks = git.join("hooks");
    if is_dir(&hooks)? {
        let mut live: Vec<String> = listdir(&hooks)?
            .into_iter()
            .filter(|n| !n.ends_with(".sample"))
            .collect();
        live.sort();
        if !live.is_empty() {
            return Err(unsafe_dir(&format!(
                ".git/hooks contains non-sample files: {}",
                list_repr(&live, 5)
            )));
        }
    }
    walk(content_dir, &|d| d == ".git", &mut |dir, _dirs, files| {
        if files.iter().any(|f| f == ".gitattributes") {
            let p = dir.join(".gitattributes");
            if getsize(&p)? > 0 {
                return Err(unsafe_dir(&format!("{} is not empty", p.display())));
            }
        }
        Ok(())
    })
}

/// The reason verify_git_metadata would refuse this repo, or None. Plain file reads only: git is never run.
pub fn hostile_findings(content_dir: &Path) -> HubResult<Option<String>> {
    match verify_git_metadata(content_dir) {
        Ok(()) => Ok(None),
        Err(p) if p.code == CODE => Ok(Some(
            p.detail
                .split(". Keep the content repo")
                .next()
                .unwrap_or("")
                .to_string(),
        )),
        Err(p) => Err(p),
    }
}

fn unlink(p: &Path) -> HubResult<()> {
    fs::remove_file(p).map_err(oserror)
}

/// Strip everything the safety gate refuses from a COPY (never the source): config keys outside the allowlist,
/// non-sample hooks, info/attributes and non-empty .gitattributes. Plain file operations only, never through a
/// symlink (deviation SB-1). Returns what was removed.
pub fn sanitize_git(root: &Path) -> HubResult<Vec<String>> {
    let mut removed: Vec<String> = Vec::new();
    let git = root.join(".git");
    // a symlinked .git is left alone (nothing is written through it); check_ownership refuses it afterwards
    if !is_symlink(&git)? {
        let cfg = git.join("config");
        if is_symlink(&cfg)? {
            unlink(&cfg)?;
            fs::write(&cfg, "").map_err(oserror)?;
            removed.push("config (symlink)".into());
        } else if exists(&cfg)? {
            sanitize_config(&cfg, &mut removed)?;
        }
        let hooks = git.join("hooks");
        if is_symlink(&hooks)? {
            unlink(&hooks)?;
            removed.push("hooks (symlink)".into());
        } else if is_dir(&hooks)? {
            for name in listdir(&hooks)? {
                if name.ends_with(".sample") {
                    continue;
                }
                removed.push(format!("hook {name}"));
                let h = hooks.join(&name);
                if is_file(&h)? || is_symlink(&h)? {
                    unlink(&h)?;
                } else {
                    // remove_dir_all never follows symlinks inside the tree (like shutil.rmtree)
                    fs::remove_dir_all(&h).map_err(oserror)?;
                }
            }
        }
        let info = git.join("info");
        if is_symlink(&info)? {
            unlink(&info)?;
            removed.push("info (symlink)".into());
        } else {
            let attrs = info.join("attributes");
            if is_symlink(&attrs)? {
                unlink(&attrs)?;
                removed.push("info/attributes".into());
            } else if exists(&attrs)? && getsize(&attrs)? > 0 {
                removed.push("info/attributes".into());
                fs::write(&attrs, "").map_err(oserror)?;
            }
        }
    }
    let root_s = root.to_string_lossy().to_string();
    let mut found: Vec<PathBuf> = Vec::new();
    walk(root, &|d| d == ".git", &mut |dir, _dirs, files| {
        if files.iter().any(|f| f == ".gitattributes") {
            found.push(dir.join(".gitattributes"));
        }
        Ok(())
    })?;
    for p in found {
        let rel = p
            .to_string_lossy()
            .strip_prefix(&format!("{root_s}/"))
            .map_or_else(|| p.to_string_lossy().to_string(), str::to_string);
        if is_symlink(&p)? {
            unlink(&p)?;
            removed.push(rel);
        } else if getsize(&p)? > 0 {
            removed.push(rel);
            fs::write(&p, "").map_err(oserror)?;
        }
    }
    Ok(removed)
}

/// The config half of `sanitize_git`: keep only allowlisted keys of plain `[section]`s, rewritten as
/// `[section]\n\t<line>\n...`.
fn sanitize_config(cfg: &Path, removed: &mut Vec<String>) -> HubResult<()> {
    use hub_core::py::text::{splitlines, strip};
    let text = read_text_lossy(cfg)?;
    let mut keep: indexmap::IndexMap<String, Vec<String>> = indexmap::IndexMap::new();
    let mut section = String::new();
    for raw in splitlines(&text, false) {
        let line = strip(raw);
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(after) = line.strip_prefix('[') {
            let head = after.find(']').map_or(after, |e| &after[..e]);
            let hs = strip(head);
            section = if !hs.contains(' ') && !head.contains('"') {
                hs.to_lowercase()
            } else {
                String::new()
            };
            continue;
        }
        let key = strip(line.split_once('=').map_or(line, |(k, _)| k)).to_lowercase();
        if !section.is_empty() && ALLOWED_CONFIG_KEYS.contains(&format!("{section}.{key}").as_str()) {
            keep.entry(section.clone()).or_default().push(line.to_string());
        } else {
            let s = if section.is_empty() { "?" } else { section.as_str() };
            removed.push(format!("config {s}.{key}"));
        }
    }
    let mut out = String::new();
    for (s, lines) in &keep {
        out.push_str(&format!("[{s}]\n"));
        for ln in lines {
            out.push_str(&format!("\t{ln}\n"));
        }
    }
    fs::write(cfg, out).map_err(oserror)
}

/// HEAD commit id read from the ref files (no git process). Python raises on a HEAD file that is not UTF-8; that is
/// `None` here (the migrate check then reports a HEAD mismatch).
pub fn plain_head(root: &Path) -> Option<String> {
    use hub_core::py::text::{split_ws, splitlines, strip};
    let git = root.join(".git");
    let head = strip(&read_text_strict(&git.join("HEAD"))?).to_string();
    let Some(r) = head.strip_prefix("ref:") else {
        return (!head.is_empty()).then_some(head);
    };
    let r = strip(r).to_string();
    if let Some(t) = read_text_strict(&git.join(&r)) {
        return Some(strip(&t).to_string());
    }
    let packed = read_text_strict(&git.join("packed-refs"))?;
    for line in splitlines(&packed, false) {
        if line.ends_with(&format!(" {r}")) {
            return split_ws(line).first().map(|s| (*s).to_string());
        }
    }
    None
}

/// Directories 0700; files lose group/world write (other bits kept, so executables stay executable).
pub fn tighten(root: &Path) -> HubResult<()> {
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).map_err(oserror)?;
    walk(root, &|_| false, &mut |dir, dirs, files| {
        for d in dirs {
            let p = dir.join(d);
            if !fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
                fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).map_err(oserror)?;
            }
        }
        for f in files {
            let p = dir.join(f);
            if !fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
                let mode = fs::metadata(&p).map_err(oserror)?.mode();
                fs::set_permissions(&p, fs::Permissions::from_mode(mode & 0o7755 & !0o022)).map_err(oserror)?;
            }
        }
        Ok(())
    })
}

/// The whole gate: location, ownership/modes, git metadata.
pub fn verify_all(content_dir: &Path, client_roots: &[String], app_root: &Path) -> HubResult<()> {
    check_location(content_dir, client_roots, app_root)?;
    check_ownership(content_dir)?;
    verify_git_metadata(content_dir)
}
