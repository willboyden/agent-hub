//! services/common.py: clock, ids, hashing, redaction, atomic file writes. Security controls: `scrub`/`redact`
//! (secrets never reach audit rows or diffs) and the 0600/0700 writers. Owner: CORE (LOCKED).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// `time.time()`.
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    // getrandom only fails if the OS has no entropy source at all; zero bytes would then be the visible symptom.
    let _ = getrandom::fill(&mut b);
    b
}

/// `secrets.token_hex(n)`.
pub fn token_hex(n: usize) -> String {
    hex::encode(random_bytes(n))
}

/// `secrets.token_urlsafe(n)`: base64url without padding of n random bytes.
pub fn token_urlsafe(n: usize) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes(n))
}

/// `new_id(prefix)`: prefix + 12 hex chars.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", token_hex(6))
}

/// `sha256(data)` hex digest.
pub fn sha256(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

static SENSITIVE_KEY: LazyLock<Regex> =
    LazyLock::new(|| crate::re(r"(?i)(token|secret|password|passwd|api[_-]?key|authorization|cookie|credential)"));
static SECRET_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    crate::re(concat!(
        r"(\b(hf_[A-Za-z0-9]{8,}|ec_[A-Za-z0-9_\-]{16,}|hc_[A-Za-z0-9_\-]{16,}|sk-[A-Za-z0-9_\-]{16,}|",
        r"ghp_[A-Za-z0-9]{16,}|xox[bp]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16})\b|Bearer\s+[A-Za-z0-9._\-]{8,}|",
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----)"
    ))
});
static ASSIGN: LazyLock<Regex> = LazyLock::new(|| {
    crate::re(
        r#"(?i)((?:token|secret|password|passwd|api[_-]?key|credential)[\w-]*["']?\s*[:=]\s*["']?)([^\s"',}]{6,})"#,
    )
});

/// `scrub(text)`: mask secret-looking tokens in free text (diffs, logs). No truncation.
pub fn scrub(text: &str) -> String {
    let first = SECRET_VALUE.replace_all(text, "[redacted]");
    ASSIGN.replace_all(&first, "${1}[redacted]").into_owned()
}

/// `redact(obj)`: deep copy with sensitive keys masked, secret-looking strings scrubbed, strings over 500 chars cut.
pub fn redact(obj: &Value) -> Value {
    redact_at(obj, 0)
}

fn redact_at(obj: &Value, depth: usize) -> Value {
    if depth > 8 {
        return Value::String("[truncated]".into());
    }
    match obj {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let v = if SENSITIVE_KEY.is_match(k) {
                        Value::String("[redacted]".into())
                    } else {
                        redact_at(v, depth + 1)
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().take(100).map(|v| redact_at(v, depth + 1)).collect()),
        Value::String(s) => {
            let s = scrub(s);
            let n = s.chars().count();
            if n <= 500 {
                Value::String(s)
            } else {
                Value::String(format!(
                    "{}...[{} more chars]",
                    crate::py::text::take_chars(&s, 500),
                    n - 500
                ))
            }
        }
        other => other.clone(),
    }
}

/// `_mkdirs(d, mode)`: mkdir -p where every directory THIS call creates gets `mode` (chmod'ed, so umask cannot loosen it).
pub fn mkdirs(d: &Path, mode: u32) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut p = d.to_path_buf();
    while !p.exists() {
        missing.push(p.clone());
        match p.parent() {
            Some(parent) if parent != p => p = parent.to_path_buf(),
            _ => break,
        }
    }
    for m in missing.iter().rev() {
        match fs::create_dir(m) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        fs::set_permissions(m, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// `fsync_dir(d)` (best effort).
pub fn fsync_dir(d: &Path) {
    if let Ok(f) = fs::File::open(d) {
        let _ = f.sync_all();
    }
}

/// `atomic_write(path, data, mode, dir_mode)`: temp file `.hub-tmp-<12 hex>` in the SAME directory (O_EXCL|O_NOFOLLOW,
/// 0600), write, fsync, fchmod(mode), rename over the target, fsync the directory. The temp file is removed on error.
pub fn atomic_write(path: &Path, data: &[u8], mode: u32, dir_mode: u32) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("/"));
    mkdirs(parent, dir_mode)?;
    let tmp = parent.join(format!(".hub-tmp-{}", token_hex(6)));
    let res = (|| -> io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(data)?;
        f.flush()?;
        f.sync_all()?;
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res?;
    fsync_dir(parent);
    Ok(())
}

/// `harden_tree(root)`: chmod 0700 on root and every directory below it; symlinks are never followed.
pub fn harden_tree(root: &Path) {
    let Ok(meta) = fs::symlink_metadata(root) else { return };
    if !meta.is_dir() {
        return;
    }
    let _ = fs::set_permissions(root, fs::Permissions::from_mode(0o700));
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if let Ok(m) = fs::symlink_metadata(&p)
                && m.is_dir()
            {
                let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o700));
                stack.push(p);
            }
        }
    }
}

/// `write_secret_file(path, text)`: 0600 from the first byte (O_NOFOLLOW, fchmod for a pre-existing looser file).
pub fn write_secret_file(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    f.write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn scrub_and_redact() {
        assert_eq!(scrub("key hc_abcdefghijklmnopqrstuvwxyz0123 end"), "key [redacted] end");
        assert_eq!(scrub("API_KEY=supersecret1"), "API_KEY=[redacted]");
        assert_eq!(scrub("Authorization: Bearer abcdefgh123"), "Authorization: [redacted]");
        let r = redact(&json!({"api_key": "x", "nested": {"password": 1}, "s": "a".repeat(510)}));
        assert_eq!(r["api_key"], "[redacted]");
        assert_eq!(r["nested"]["password"], "[redacted]");
        assert!(r["s"].as_str().is_some_and(|s| s.ends_with("...[10 more chars]")));
    }

    #[test]
    fn atomic_write_modes() -> io::Result<()> {
        let d = tempfile::tempdir()?;
        let p = d.path().join("a/b/f.json");
        atomic_write(&p, b"x", 0o600, 0o700)?;
        assert_eq!(fs::metadata(&p)?.permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(d.path().join("a"))?.permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::read(&p)?, b"x");
        Ok(())
    }
}
