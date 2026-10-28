//! `os` / `os.path` / `posixpath` behaviour the hub relies on, and Python's rendering of OS errors (API messages embed
//! `type(exc).__name__` and `str(exc)`, e.g. `"FileNotFoundError: [Errno 2] No such file or directory: 'x'"`).

use std::io;
use std::path::{Path, PathBuf};

use super::text;

/// `type(exc).__name__` of the `OSError` subclass CPython raises for this error.
pub fn error_name(e: &io::Error) -> &'static str {
    match e.raw_os_error() {
        Some(code) => errno_name(code),
        None => match e.kind() {
            io::ErrorKind::NotFound => "FileNotFoundError",
            io::ErrorKind::PermissionDenied => "PermissionError",
            io::ErrorKind::AlreadyExists => "FileExistsError",
            io::ErrorKind::TimedOut => "TimeoutError",
            io::ErrorKind::BrokenPipe => "BrokenPipeError",
            io::ErrorKind::InvalidData => "UnicodeDecodeError",
            _ => "OSError",
        },
    }
}

/// CPython's errno -> OSError subclass mapping (Objects/exceptions.c).
pub fn errno_name(code: i32) -> &'static str {
    use nix::errno::Errno as E;
    match E::from_raw(code) {
        E::ENOENT => "FileNotFoundError",
        E::EEXIST => "FileExistsError",
        E::EACCES | E::EPERM => "PermissionError",
        E::EISDIR => "IsADirectoryError",
        E::ENOTDIR => "NotADirectoryError",
        E::EINTR => "InterruptedError",
        E::ECHILD => "ChildProcessError",
        E::EAGAIN | E::EALREADY | E::EINPROGRESS => "BlockingIOError",
        E::ESRCH => "ProcessLookupError",
        E::ETIMEDOUT => "TimeoutError",
        E::ECONNREFUSED => "ConnectionRefusedError",
        E::ECONNRESET => "ConnectionResetError",
        E::ECONNABORTED => "ConnectionAbortedError",
        E::EPIPE | E::ESHUTDOWN => "BrokenPipeError",
        _ => "OSError",
    }
}

/// `os.strerror(code)` (the libc text, without Rust's " (os error N)" suffix).
pub fn strerror(code: i32) -> String {
    let s = io::Error::from_raw_os_error(code).to_string();
    match s.rfind(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// `str(exc)` of an OSError: `[Errno N] text: 'filename'` (filename part only when known).
pub fn error_str(e: &io::Error, filename: Option<&str>) -> String {
    match e.raw_os_error() {
        Some(code) => match filename {
            Some(f) => format!("[Errno {code}] {}: {}", strerror(code), text::repr(f)),
            None => format!("[Errno {code}] {}", strerror(code)),
        },
        None => e.to_string(),
    }
}

/// `posixpath.normpath`.
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else {
        usize::from(path.starts_with('/'))
    };
    let mut comps: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp != ".." || (initial == 0 && comps.is_empty()) || comps.last() == Some(&"..") {
            comps.push(comp);
        } else if !comps.is_empty() {
            comps.pop();
        }
    }
    let joined = comps.join("/");
    let out = format!("{}{}", "/".repeat(initial), joined);
    if out.is_empty() { ".".into() } else { out }
}

/// `posixpath.join(a, b)` for two components.
pub fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `posixpath.basename`.
pub fn basename(p: &str) -> &str {
    match p.rfind('/') {
        Some(i) => &p[i + 1..],
        None => p,
    }
}

/// `os.path.expanduser` with `home` (Python reads `$HOME`).
pub fn expanduser(p: &str, home: &str) -> String {
    if p == "~" {
        home.to_string()
    } else if let Some(rest) = p.strip_prefix("~/") {
        let h = home.trim_end_matches('/');
        format!("{}/{rest}", if h.is_empty() { "" } else { h })
    } else {
        p.to_string()
    }
}

/// `os.path.realpath(p)` (non-strict, CPython 3.13): resolves symlinks component by component, keeps a missing tail
/// as written, and on a symlink loop returns the path joined so far. Relative paths are made absolute against the cwd.
pub fn realpath(p: &Path) -> PathBuf {
    let s = p.to_string_lossy().to_string();
    let abs = if s.starts_with('/') {
        s
    } else {
        let cwd = std::env::current_dir()
            .map(|c| c.to_string_lossy().to_string())
            .unwrap_or_else(|_| "/".into());
        join(&cwd, &s)
    };
    PathBuf::from(resolve(&abs, 0))
}

fn resolve(path: &str, depth: usize) -> String {
    let mut resolved = String::from("/");
    let mut rest: Vec<String> = path.split('/').filter(|c| !c.is_empty()).map(str::to_string).collect();
    rest.reverse();
    let mut links = depth;
    while let Some(name) = rest.pop() {
        if name == "." {
            continue;
        }
        if name == ".." {
            let parent = Path::new(&resolved)
                .parent()
                .map(|x| x.to_string_lossy().to_string())
                .unwrap_or_else(|| "/".into());
            resolved = parent;
            continue;
        }
        let newpath = join(&resolved, &name);
        match std::fs::symlink_metadata(&newpath) {
            Ok(m) if m.file_type().is_symlink() => {
                links += 1;
                if links > 40 {
                    // loop: CPython (non-strict) keeps the unresolved remainder joined
                    let mut tail: Vec<String> = rest.clone();
                    tail.reverse();
                    let mut out = newpath;
                    for t in tail {
                        out = join(&out, &t);
                    }
                    return out;
                }
                match std::fs::read_link(&newpath) {
                    Ok(target) => {
                        let t = target.to_string_lossy().to_string();
                        let base = if t.starts_with('/') { t } else { join(&resolved, &t) };
                        let mut more: Vec<String> =
                            base.split('/').filter(|c| !c.is_empty()).map(str::to_string).collect();
                        more.reverse();
                        resolved = "/".into();
                        // re-resolve the target from the root, then continue with the remainder
                        let mut stack = rest;
                        stack.extend(more);
                        rest = stack;
                    }
                    Err(_) => resolved = newpath,
                }
            }
            _ => resolved = newpath,
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normpath_like_posixpath() {
        assert_eq!(normpath("/home/u/a/../.aws"), "/home/u/.aws");
        assert_eq!(normpath("//a//b/./c"), "//a/b/c");
        assert_eq!(normpath("///a"), "/a");
        assert_eq!(normpath("a/../../b"), "../b");
        assert_eq!(normpath(""), ".");
        assert_eq!(normpath("/.."), "/");
    }

    #[test]
    fn os_error_rendering() {
        let e = io::Error::from_raw_os_error(2);
        assert_eq!(error_name(&e), "FileNotFoundError");
        assert_eq!(error_str(&e, Some("x")), "[Errno 2] No such file or directory: 'x'");
        assert_eq!(errno_name(21), "IsADirectoryError");
    }

    #[test]
    fn realpath_resolves_links() -> io::Result<()> {
        let d = tempfile::tempdir()?;
        let real = d.path().join("real");
        std::fs::create_dir(&real)?;
        std::os::unix::fs::symlink(&real, d.path().join("link"))?;
        let got = realpath(&d.path().join("link/missing/x"));
        assert_eq!(got, realpath(&real).join("missing/x"));
        Ok(())
    }
}
