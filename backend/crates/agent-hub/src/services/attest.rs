//! Port of services/attest.py: MCP scan attestation. `scan_status: clean` only holds while `scan_digest` equals
//! HMAC-SHA256 (key: 32 random bytes hex in `<data>/attest.key`, 0600, created once with O_EXCL|O_NOFOLLOW) over
//! `json.dumps([command, args, url, pinned_ref, transport], sort_keys=True, separators=(",", ":"))`. Compared in
//! constant time. Security control S-ATTEST. Owner: WS-B1.
//!
//! Anyone who can edit content/*.yaml can change the fields but cannot forge the digest without the key, which lives
//! outside the content repo.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt as _, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hmac::{Hmac, KeyInit, Mac};
use hub_core::py::json as pyjson;
use hub_core::value::PyValue;
use hub_core::{HubResult, Problem};
use serde_json::Value;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use super::content::{Exc, XResult, py_list, truthy};

/// `mcp_blob(command, args, url, pinned_ref, transport)`: an `args` that `list()` cannot iterate yields an empty list
/// here; the hub's callers go through [`mcp_blob_x`], which raises Python's TypeError instead.
pub fn mcp_blob(
    command: &PyValue,
    args: &PyValue,
    url: &PyValue,
    pinned_ref: &PyValue,
    transport: &PyValue,
) -> Vec<u8> {
    mcp_blob_x(command, args, url, pinned_ref, transport).unwrap_or_default()
}

/// `mcp_blob(...)` with `list(args or [])`'s TypeError.
pub fn mcp_blob_x(
    command: &PyValue,
    args: &PyValue,
    url: &PyValue,
    pinned_ref: &PyValue,
    transport: &PyValue,
) -> XResult<Vec<u8>> {
    let args = if truthy(args) { py_list(args)? } else { Vec::new() };
    let doc = Value::Array(vec![
        command.to_json(),
        Value::Array(args.iter().map(PyValue::to_json).collect()),
        url.to_json(),
        pinned_ref.to_json(),
        transport.to_json(),
    ]);
    Ok(pyjson::dumps(&doc, &pyjson::Opts::canon()).into_bytes())
}

pub struct Attestor {
    pub key_file: PathBuf,
    /// `_key`: loaded once, then reused (Python caches it on the instance).
    key: Mutex<Option<Vec<u8>>>,
}

/// `bytes.fromhex(s)`: ASCII whitespace between bytes is skipped; ValueError otherwise.
fn fromhex(s: &str) -> XResult<Vec<u8>> {
    let compact: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    hex::decode(&compact).map_err(|_| Exc::other("ValueError", "non-hexadecimal number found in fromhex() arg"))
}

impl Attestor {
    pub fn new(key_file: &Path) -> Self {
        Self {
            key_file: key_file.to_path_buf(),
            key: Mutex::new(None),
        }
    }

    fn unusable(&self) -> Exc {
        Problem::new(
            503,
            "attest_key_unusable",
            format!(
                "{} must be a regular file with mode 0600 (it keys MCP scan attestations)",
                self.key_file.display()
            ),
        )
        .into()
    }

    /// `_load()`: create the key on first use (O_EXCL|O_NOFOLLOW, 0600), refuse a loose or non-regular file.
    fn load(&self) -> XResult<Vec<u8>> {
        let mut cached = self.key.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(k) = cached.as_ref() {
            return Ok(k.clone());
        }
        let f = &self.key_file;
        let st = match fs::symlink_metadata(f) {
            Ok(st) => st,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = f.parent()
                    && !parent.is_dir()
                {
                    if let Some(gp) = parent.parent() {
                        fs::create_dir_all(gp)?;
                    }
                    match fs::DirBuilder::new().mode(0o700).create(parent) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && parent.is_dir() => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                let mut fh = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(nix::libc::O_NOFOLLOW)
                    .open(f)?;
                fh.write_all(hub_core::common::token_hex(32).as_bytes())?;
                drop(fh);
                fs::symlink_metadata(f)?
            }
            Err(e) => return Err(e.into()),
        };
        let ft = st.file_type();
        let regular = ft.is_file() && !ft.is_symlink() && !ft.is_fifo() && !ft.is_socket();
        if !regular || st.mode() & 0o077 != 0 || st.size() > 4096 {
            return Err(self.unusable());
        }
        let key = fromhex(hub_core::py::text::strip(&fs::read_to_string(f)?))?;
        *cached = Some(key.clone());
        Ok(key)
    }

    /// The hex HMAC (creates the key on first use; 503 `attest_key_unusable` for a loose/non-regular key file).
    pub fn digest(
        &self,
        command: &PyValue,
        args: &PyValue,
        url: &PyValue,
        pinned_ref: &PyValue,
        transport: &PyValue,
    ) -> HubResult<String> {
        self.digest_x(command, args, url, pinned_ref, transport)
            .map_err(Exc::into_problem)
    }

    /// `digest(...)` keeping Python's exception types.
    pub fn digest_x(
        &self,
        command: &PyValue,
        args: &PyValue,
        url: &PyValue,
        pinned_ref: &PyValue,
        transport: &PyValue,
    ) -> XResult<String> {
        let key = self.load()?;
        let blob = mcp_blob_x(command, args, url, pinned_ref, transport)?;
        let mut mac =
            <Hmac<Sha256> as KeyInit>::new_from_slice(&key).map_err(|_| Exc::other("ValueError", "bad HMAC key"))?;
        mac.update(&blob);
        Ok(hex::encode(mac.finalize().into_bytes()))
    }

    /// `valid(digest, ...)`: false on any error (the resolver uses [`Attestor::valid_x`], which propagates them).
    pub fn valid(
        &self,
        digest: Option<&str>,
        command: &PyValue,
        args: &PyValue,
        url: &PyValue,
        pinned_ref: &PyValue,
        transport: &PyValue,
    ) -> bool {
        self.valid_x(digest, command, args, url, pinned_ref, transport)
            .unwrap_or(false)
    }

    /// `bool(digest) and hmac.compare_digest(str(digest), self.digest(...))`: constant-time; a non-ASCII digest is
    /// Python's TypeError; an unusable key is the 503 problem.
    pub fn valid_x(
        &self,
        digest: Option<&str>,
        command: &PyValue,
        args: &PyValue,
        url: &PyValue,
        pinned_ref: &PyValue,
        transport: &PyValue,
    ) -> XResult<bool> {
        let Some(d) = digest.filter(|d| !d.is_empty()) else {
            return Ok(false);
        };
        let want = self.digest_x(command, args, url, pinned_ref, transport)?;
        if !d.is_ascii() {
            return Err(Exc::other(
                "TypeError",
                "comparing strings with non-ASCII characters is not supported",
            ));
        }
        Ok(bool::from(d.as_bytes().ct_eq(want.as_bytes())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_is_canonical_json() {
        let b = mcp_blob(
            &PyValue::str("srv"),
            &PyValue::List(vec![PyValue::str("--x")]),
            &PyValue::None,
            &PyValue::None,
            &PyValue::str("stdio"),
        );
        assert_eq!(String::from_utf8_lossy(&b), r#"["srv",["--x"],null,null,"stdio"]"#);
        // list("ab") iterates characters; an int cannot be iterated (TypeError -> 500 in Python)
        let s = mcp_blob(
            &PyValue::None,
            &PyValue::str("ab"),
            &PyValue::None,
            &PyValue::None,
            &PyValue::None,
        );
        assert_eq!(String::from_utf8_lossy(&s), r#"[null,["a","b"],null,null,null]"#);
        assert!(
            mcp_blob_x(
                &PyValue::None,
                &PyValue::Int(5),
                &PyValue::None,
                &PyValue::None,
                &PyValue::None
            )
            .is_err()
        );
    }
}
