//! domain/pathsafe.py: relative-path rules for everything the hub reads or writes for a client. Security control
//! (PORTING.md "Security controls" S-PATH). Owner: CORE (LOCKED).

use std::path::{Path, PathBuf};

use crate::py::os::{expanduser, realpath};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PathError(pub String);

pub const MAX_REL: usize = 512;

fn err<T>(m: &str) -> Result<T, PathError> {
    Err(PathError(m.to_string()))
}

/// `check_relpath`: relative only; no `..`, empty or `.` segments, no absolute path, backslash, NUL or control char.
pub fn check_relpath(rel: &str) -> Result<&str, PathError> {
    if rel.is_empty() {
        return err("empty path");
    }
    if rel.chars().count() > MAX_REL {
        return err("path too long");
    }
    if rel.contains('\0') || rel.contains('\\') {
        return err("NUL or backslash in path");
    }
    if rel.chars().any(|c| (c as u32) < 32) {
        return err("control character in path");
    }
    if rel.starts_with('/') || rel.chars().nth(1) == Some(':') {
        return err("absolute path");
    }
    if rel.split('/').any(|p| p.is_empty() || p == "." || p == "..") {
        return err("empty, '.' or '..' path segment");
    }
    Ok(rel)
}

/// `resolve_under(root, rel)`: the absolute target, refusing symlinks below the (possibly symlinked) root and escapes.
pub fn resolve_under(root: &Path, rel: &str) -> Result<PathBuf, PathError> {
    let rel = check_relpath(rel)?;
    if !root.is_absolute() {
        return err("root must be absolute");
    }
    let real_root = realpath(root);
    let mut cur = real_root.clone();
    let mut checking = true;
    for part in rel.split('/') {
        cur = cur.join(part);
        if checking {
            if cur
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                return err("symlink in path");
            }
            if !cur.exists() {
                checking = false;
            }
        }
    }
    if !cur.starts_with(&real_root) || cur == real_root {
        return err("path escapes its root");
    }
    Ok(cur)
}

/// `expand_root(value)`: expand `~` (from `$HOME`), demand an absolute path, refuse NUL and `..`.
pub fn expand_root(value: &str) -> Result<PathBuf, PathError> {
    if value.is_empty() || value.contains('\0') {
        return err("bad root");
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let p = expanduser(value, &home);
    if !p.starts_with('/') {
        return err("root must be absolute (or start with ~)");
    }
    if p.split('/').any(|c| c == "..") {
        return err("root must not contain '..'");
    }
    Ok(PathBuf::from(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relpath_rules() {
        assert!(check_relpath("a/b.md").is_ok());
        for bad in ["", "/a", "a/../b", "a//b", "./a", "a\\b", "a\0b", "a\nb", "C:x"] {
            assert!(check_relpath(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn resolve_refuses_symlinks() -> std::io::Result<()> {
        let d = tempfile::tempdir()?;
        std::fs::create_dir(d.path().join("real"))?;
        std::os::unix::fs::symlink(d.path().join("real"), d.path().join("link"))?;
        assert!(resolve_under(d.path(), "link/x").is_err());
        assert!(resolve_under(d.path(), "real/x").is_ok());
        assert!(resolve_under(Path::new("rel"), "x").is_err());
        Ok(())
    }
}
