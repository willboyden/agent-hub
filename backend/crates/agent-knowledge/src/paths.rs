//! Port of paths.py: path-ingest safety. Everything is resolved (symlinks followed) and must land under an allowlisted
//! root; the glob charset is restricted; secret-looking parts, non-text suffixes and huge files are skipped.
//! [`py_glob`] reproduces CPython 3.13 `Path.glob` for the patterns [`check_glob`] lets through (`*`, `?`, `**`,
//! literal segments): `**` does not descend into symlinked directories, `*` matches dotfiles, literal and `*/`
//! segments follow symlinks, results are sorted like `pathlib` paths (component-wise).

use std::collections::HashSet;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::errors::KnowledgeError;

static GLOB_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^[A-Za-z0-9_.*?/\-]{1,200}$"));
pub const TEXT_SUFFIXES: [&str; 23] = [
    ".md", ".txt", ".rst", ".py", ".ts", ".js", ".json", ".yaml", ".yml", ".toml", ".sh", ".go", ".rs", ".c", ".h",
    ".cpp", ".java", ".html", ".css", ".csv", ".tex", ".org", ".ipynb",
];
/// Never ingest things conventionally treated as secrets, even when they sit inside an allowlisted root.
pub const DENY_PARTS: [&str; 9] = [
    ".ssh",
    ".aws",
    ".gnupg",
    ".git",
    ".config",
    ".kube",
    ".docker",
    "secrets",
    "__pycache__",
];
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

fn deny(code: &str, detail: &str) -> KnowledgeError {
    KnowledgeError::new(if code == "path_not_allowed" { 403 } else { 422 }, code, detail)
}

/// `Path.is_relative_to(root)` (component prefix; `base == root` included).
pub fn is_relative_to(p: &Path, root: &Path) -> bool {
    p.starts_with(root)
}

/// `resolve_base(path, roots)` -> (resolved base, its resolved root).
pub fn resolve_base(path: &str, roots: &[PathBuf]) -> Result<(PathBuf, PathBuf), KnowledgeError> {
    if roots.is_empty() {
        return Err(deny("path_not_allowed", "no ingest roots are configured"));
    }
    if path.contains('\0') || path.chars().count() > 1024 || !path.starts_with('/') {
        return Err(deny(
            "path_not_allowed",
            "path must be an absolute path under an allowlisted root",
        ));
    }
    let base = fs::canonicalize(path).map_err(|_| deny("path_not_found", "path does not exist"))?;
    for root in roots {
        let rr = hub_core::py::os::realpath(root);
        if is_relative_to(&base, &rr) {
            return Ok((base, rr));
        }
    }
    Err(deny("path_not_allowed", "path is outside every allowlisted root"))
}

/// `check_glob(glob)`.
pub fn check_glob(glob: &str) -> Result<String, KnowledgeError> {
    if !GLOB_RE.is_match(glob) || glob.starts_with('/') || glob.split('/').any(|p| p == "..") {
        return Err(deny(
            "invalid_glob",
            "glob may only use [A-Za-z0-9_.*?/-], must be relative and contain no '..'",
        ));
    }
    Ok(glob.to_string())
}

// ------------------------------------------------------------------------------------------------ Path.glob

enum Sel {
    Exists,
    Special(String, Box<Sel>),
    Literal(String, Box<Sel>),
    Wildcard {
        pat: Option<String>,
        dir_only: bool,
        next: Box<Sel>,
    },
    Recursive {
        dir_only: bool,
        next: Box<Sel>,
    },
}

fn magic(p: &str) -> bool {
    p.contains(['*', '?', '['])
}

/// `_Globber.selector(parts)` (parts in pattern order).
fn selector(parts: &[String]) -> Sel {
    let Some(part) = parts.first() else { return Sel::Exists };
    let mut rest = &parts[1..];
    if part == "**" {
        while rest.first().is_some_and(|p| p == "**") {
            rest = &rest[1..];
        }
        return Sel::Recursive {
            dir_only: !rest.is_empty(),
            next: Box::new(selector(rest)),
        };
    }
    if part.is_empty() || part == "." || part == ".." {
        return Sel::Special(part.clone(), Box::new(selector(rest)));
    }
    if !magic(part) {
        let mut joined = part.clone();
        while let Some(p) = rest.first().filter(|p| !magic(p)) {
            joined.push('/');
            joined.push_str(p);
            rest = &rest[1..];
        }
        return Sel::Literal(joined, Box::new(selector(rest)));
    }
    Sel::Wildcard {
        pat: if part == "*" { None } else { Some(part.clone()) },
        dir_only: !rest.is_empty(),
        next: Box::new(selector(rest)),
    }
}

fn add_slash(p: &str) -> String {
    if p.is_empty() || p.ends_with('/') {
        p.to_string()
    } else {
        format!("{p}/")
    }
}

fn entry_path(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// fnmatch of one name against a segment pattern of `*`, `?` and literal characters (`include_hidden=True`).
fn seg_match(pat: &[char], name: &[char]) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while n < name.len() {
        if p < pat.len() && pat[p] != '*' && (pat[p] == '?' || pat[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            mark = n;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

fn scandir(path: &str) -> Vec<(String, fs::DirEntry)> {
    let Ok(rd) = fs::read_dir(if path.is_empty() { "." } else { path }) else {
        return Vec::new();
    };
    rd.filter_map(Result::ok)
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e))
        .collect()
}

fn run(sel: &Sel, path: String, exists: bool, out: &mut Vec<String>) {
    match sel {
        Sel::Exists => {
            if exists || fs::symlink_metadata(&path).is_ok() {
                out.push(path);
            }
        }
        Sel::Special(part, next) => run(next, format!("{}{part}", add_slash(&path)), exists, out),
        Sel::Literal(part, next) => run(next, format!("{}{part}", add_slash(&path)), false, out),
        Sel::Wildcard { pat, dir_only, next } => {
            let pat: Option<Vec<char>> = pat.as_ref().map(|p| p.chars().collect());
            for (name, entry) in scandir(&path) {
                let name_chars: Vec<char> = name.chars().collect();
                if pat.as_ref().is_some_and(|p| !seg_match(p, &name_chars)) {
                    continue;
                }
                let ep = entry_path(&path, &name);
                if *dir_only {
                    // DirEntry.is_dir() follows symlinks
                    if !fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()) {
                        continue;
                    }
                    run(next, ep, true, out);
                } else {
                    out.push(ep);
                }
            }
        }
        Sel::Recursive { dir_only, next } => {
            let path = add_slash(&path);
            run(next, path.clone(), exists, out);
            let mut stack = vec![path];
            while let Some(p) = stack.pop() {
                for (name, entry) in scandir(&p) {
                    // is_dir(follow_symlinks=False): symlinked directories are not descended
                    let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                    if is_dir || !dir_only {
                        let ep = entry_path(&p, &name);
                        if *dir_only {
                            run(next, ep.clone(), true, out);
                        } else {
                            out.push(ep.clone());
                        }
                        if is_dir {
                            stack.push(ep);
                        }
                    }
                }
            }
        }
    }
}

/// The sort key of a `pathlib` path: its string split on `/`.
fn path_key(p: &Path) -> Vec<Vec<u8>> {
    p.as_os_str()
        .as_bytes()
        .split(|&b| b == b'/')
        .map(<[u8]>::to_vec)
        .collect()
}

/// `sorted(base.glob(pattern))`; `Err` = Python's `ValueError("Unacceptable pattern")` (the job's internal error).
pub fn py_glob(base: &Path, pattern: &str) -> Result<Vec<PathBuf>, String> {
    let mut parts: Vec<String> = pattern
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .map(str::to_string)
        .collect();
    if parts.is_empty() {
        return Err(format!("Unacceptable pattern: {}", hub_core::py::text::repr(pattern)));
    }
    if pattern.ends_with('/') {
        parts.push(String::new());
    }
    let last = parts.last().cloned().unwrap_or_default();
    let mut out = Vec::new();
    run(&selector(&parts), base.to_string_lossy().into_owned(), false, &mut out);
    let mut paths: Vec<PathBuf> = out
        .into_iter()
        .map(|s| {
            let s = if (last.is_empty() || last == "**") && s.len() > 1 && s.ends_with('/') {
                s[..s.len() - 1].to_string()
            } else {
                s
            };
            crate::config::py_path(&s)
        })
        .collect();
    paths.sort_by_key(|p| path_key(p));
    Ok(paths)
}

/// `PurePath.suffix` (CPython 3.13).
fn suffix(p: &Path) -> String {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let chars: Vec<char> = name.chars().collect();
    match chars.iter().rposition(|&c| c == '.') {
        Some(i) if i > 0 && i < chars.len() - 1 => chars[i..].iter().collect(),
        _ => String::new(),
    }
}

/// `iter_files(base, root, glob, max_files)`: resolved regular files under `root`, skipping symlink escapes, denied
/// parts, non-text and huge files. Errors: 413 `too_many_files`, or `internal_error` for the ValueError of an
/// unacceptable pattern.
pub fn iter_files(base: &Path, root: &Path, glob: &str, max_files: i64) -> Result<Vec<PathBuf>, KnowledgeError> {
    let candidates = if base.is_file() {
        vec![base.to_path_buf()]
    } else {
        py_glob(base, glob).map_err(|_| KnowledgeError::internal())?
    };
    let deny: HashSet<&str> = DENY_PARTS.iter().copied().collect();
    let mut out = Vec::new();
    let mut n: i64 = 0;
    for c in candidates {
        let Ok(real) = fs::canonicalize(&c) else { continue };
        if !is_relative_to(&real, root) || !real.is_file() {
            continue; // symlink pointing outside the root, or a directory
        }
        let Ok(rel) = real.strip_prefix(root) else { continue };
        let bad = rel.components().any(|comp| match comp {
            Component::Normal(p) => {
                let s = p.to_string_lossy();
                deny.contains(s.as_ref()) || s.starts_with(".env")
            }
            _ => false,
        });
        let suf = suffix(&real).to_lowercase();
        if bad || !TEXT_SUFFIXES.contains(&suf.as_str()) {
            continue;
        }
        match fs::metadata(&real) {
            Ok(m) if m.len() <= MAX_FILE_BYTES => {}
            _ => continue,
        }
        n += 1;
        if n > max_files {
            return Err(KnowledgeError::new(
                413,
                "too_many_files",
                format!("glob matches more than {max_files} files"),
            ));
        }
        out.push(real);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_matching() {
        let m = |p: &str, n: &str| seg_match(&p.chars().collect::<Vec<_>>(), &n.chars().collect::<Vec<_>>());
        assert!(m("*.md", "a.md"));
        assert!(m("*.md", ".md"));
        assert!(m("a?c", "abc"));
        assert!(!m("a?c", "ac"));
        assert!(m("*a*b*", "xxaybz"));
        assert!(!m("*.md", "a.mdx"));
    }

    #[test]
    fn suffix_like_pathlib() {
        assert_eq!(suffix(Path::new("/a/b.MD")), ".MD");
        assert_eq!(suffix(Path::new("/a/.bashrc")), "");
        assert_eq!(suffix(Path::new("/a/x.")), "");
        assert_eq!(suffix(Path::new("/a/x.tar.gz")), ".gz");
    }
}
