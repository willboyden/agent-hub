//! Port of services/safefs.py: directory-fd relative file operations. Every path component is opened with
//! O_NOFOLLOW|O_DIRECTORY relative to the previous directory fd, so a component swapped for a symlink between planning
//! and writing cannot redirect a write (TOCTOU). `nix` (openat/mkdirat/renameat/unlinkat/fstat); no `unsafe`.
//! Security control S-SAFEFS. Owner: WS-B2.
//!
//! One deliberate difference (PORTING.md deviation D-B2-FIFO, a local DoS fix): `read_at` opens with O_NONBLOCK, so a
//! FIFO swapped in at a target path is refused as "not a regular file" instead of blocking the apply (and the hub lock)
//! forever, as Python's blocking `os.open` would. Regular files read the same either way.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;

use hub_core::py::os::strerror;
use hub_core::py::repr;
use nix::errno::Errno;
use nix::fcntl::{OFlag, open, openat, renameat};
use nix::sys::stat::{Mode, SFlag, fchmod, fstat, mkdirat};
use nix::unistd::{UnlinkatFlags, fsync, unlinkat};

/// The live tree changed under us (symlink swapped in, file edited since planning). Message as Python's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RaceError(pub String);

#[derive(Debug, thiserror::Error)]
pub enum SafeFsError {
    #[error(transparent)]
    Race(#[from] RaceError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<Errno> for SafeFsError {
    fn from(e: Errno) -> Self {
        SafeFsError::Io(std::io::Error::from(e))
    }
}

/// An OSError as Python renders it: the errno (for `type(exc).__name__`) and `str(exc)` with the file name(s)
/// (`[Errno 2] No such file or directory: 'x'`, `... 'a' -> 'b'` for a rename). Carried inside `SafeFsError::Io`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{text}")]
pub struct PyOsError {
    pub errno: i32,
    pub text: String,
}

/// `OSError(errno, strerror, filename[, filename2])`.
pub fn os_err(e: Errno, f1: Option<&str>, f2: Option<&str>) -> SafeFsError {
    let code = e as i32;
    let mut text = format!("[Errno {code}] {}", strerror(code));
    if let Some(a) = f1 {
        text.push_str(&format!(": {}", repr(a)));
        if let Some(b) = f2 {
            text.push_str(&format!(" -> {}", repr(b)));
        }
    }
    SafeFsError::Io(std::io::Error::new(
        std::io::Error::from(e).kind(),
        PyOsError { errno: code, text },
    ))
}

/// Python's `(type(exc).__name__, str(exc))` for an error of this module (a RaceError's name is `RaceError`).
pub fn py_error(e: &SafeFsError) -> (String, String) {
    match e {
        SafeFsError::Race(r) => ("RaceError".into(), r.0.clone()),
        SafeFsError::Io(io) => {
            if let Some(p) = io.get_ref().and_then(|x| x.downcast_ref::<PyOsError>()) {
                return (hub_core::py::os::errno_name(p.errno).into(), p.text.clone());
            }
            (
                hub_core::py::os::error_name(io).into(),
                hub_core::py::os::error_str(io, None),
            )
        }
    }
}

fn mode(bits: u32) -> Mode {
    Mode::from_bits_truncate(bits)
}

/// `open_dir(root, parts, create, created)`: open root/parts as a directory fd, refusing symlinks at every step.
/// Missing components are created 0755 (umask applies, as `os.mkdir`) when `create`, and recorded in `created`
/// (component lists from the root) so a rollback can remove them.
pub fn open_dir(
    root: &str,
    parts: &[String],
    create: bool,
    created: &mut Vec<Vec<String>>,
) -> Result<OwnedFd, SafeFsError> {
    let mut fd = open(
        root,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| os_err(e, Some(root), None))?;
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    for (i, part) in parts.iter().enumerate() {
        let nfd = match openat(&fd, part.as_str(), flags, Mode::empty()) {
            Ok(n) => n,
            Err(Errno::ENOENT) => {
                if !create {
                    return Err(os_err(Errno::ENOENT, Some(part), None));
                }
                mkdirat(&fd, part.as_str(), mode(0o755)).map_err(|e| os_err(e, Some(part), None))?;
                created.push(parts[..=i].to_vec());
                openat(&fd, part.as_str(), flags, Mode::empty()).map_err(|e| os_err(e, Some(part), None))?
            }
            // ELOOP / ENOTDIR: a symlink or a file where a directory should be
            Err(e) => {
                return Err(RaceError(format!(
                    "path component {} is not a plain directory ({})",
                    repr(part),
                    strerror(e as i32)
                ))
                .into());
            }
        };
        fd = nfd;
    }
    Ok(fd)
}

/// `read_at(dirfd, name)`: (content, mode & 0o777) of a regular file, (None, None) when absent; symlink/special = race.
pub fn read_at(dirfd: &OwnedFd, name: &str) -> Result<(Option<Vec<u8>>, Option<u32>), SafeFsError> {
    let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK;
    let fd = match openat(dirfd, name, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::ENOENT) => return Ok((None, None)),
        Err(e) => {
            return Err(RaceError(format!("{} is not a plain file ({})", repr(name), strerror(e as i32))).into());
        }
    };
    let st = fstat(&fd)?;
    let fmt = SFlag::S_IFMT.bits();
    if st.st_mode & fmt != SFlag::S_IFREG.bits() {
        return Err(RaceError(format!("{} is not a regular file", repr(name))).into());
    }
    let mut f = std::fs::File::from(fd);
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok((Some(buf), Some(st.st_mode & 0o777)))
}

/// `write_tmp(dirfd, data, mode)`: `.hub-tmp-<12 hex>` (O_EXCL|O_NOFOLLOW, 0600, fsync, fchmod mode); returns its name.
pub fn write_tmp(dirfd: &OwnedFd, data: &[u8], file_mode: u32) -> Result<String, SafeFsError> {
    let name = format!(".hub-tmp-{}", hub_core::common::token_hex(6));
    let flags = OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let fd = openat(dirfd, name.as_str(), flags, mode(0o600)).map_err(|e| os_err(e, Some(&name), None))?;
    let mut f = std::fs::File::from(fd);
    f.write_all(data)?;
    f.flush()?;
    f.sync_all()?;
    fchmod(&f, mode(file_mode))?;
    Ok(name)
}

/// `replace(dirfd, tmp, name)`: renameat within the dir, then fsync the dir (best effort).
pub fn replace(dirfd: &OwnedFd, tmp: &str, name: &str) -> Result<(), SafeFsError> {
    renameat(dirfd, tmp, dirfd, name).map_err(|e| os_err(e, Some(tmp), Some(name)))?;
    let _ = fsync(dirfd);
    Ok(())
}

/// `os.unlink(name, dir_fd=dirfd)`.
pub fn unlink_at(dirfd: &OwnedFd, name: &str) -> Result<(), SafeFsError> {
    unlinkat(dirfd, name, UnlinkatFlags::NoRemoveDir).map_err(|e| os_err(e, Some(name), None))?;
    Ok(())
}

/// `os.rmdir(name, dir_fd=dirfd)`.
pub fn rmdir_at(dirfd: &OwnedFd, name: &str) -> Result<(), SafeFsError> {
    unlinkat(dirfd, name, UnlinkatFlags::RemoveDir).map_err(|e| os_err(e, Some(name), None))?;
    Ok(())
}

pub fn unlink_quiet(dirfd: &OwnedFd, name: &str) {
    let _ = unlinkat(dirfd, name, UnlinkatFlags::NoRemoveDir);
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    type R = Result<(), Box<dyn std::error::Error>>;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn open_dir_creates_and_records_and_refuses_links() -> R {
        let d = tempfile::tempdir()?;
        let root = d.path().to_string_lossy().to_string();
        let mut created = Vec::new();
        assert!(matches!(
            open_dir(&root, &s(&["a", "b"]), false, &mut created),
            Err(SafeFsError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound
        ));
        let fd = open_dir(&root, &s(&["a", "b"]), true, &mut created)?;
        assert_eq!(created, vec![s(&["a"]), s(&["a", "b"])]);
        let name = write_tmp(&fd, b"hello", 0o640)?;
        assert!(name.starts_with(".hub-tmp-") && name.len() == ".hub-tmp-".len() + 12);
        replace(&fd, &name, "f.txt")?;
        let p = d.path().join("a/b/f.txt");
        assert_eq!(std::fs::read(&p)?, b"hello");
        assert_eq!(std::fs::metadata(&p)?.permissions().mode() & 0o777, 0o640);
        assert_eq!(read_at(&fd, "f.txt")?, (Some(b"hello".to_vec()), Some(0o640)));
        assert_eq!(read_at(&fd, "missing")?, (None, None));
        // a symlinked component is a race, never followed
        let outside = tempfile::tempdir()?;
        std::os::unix::fs::symlink(outside.path(), d.path().join("link"))?;
        let e = open_dir(&root, &s(&["link"]), true, &mut Vec::new())
            .err()
            .ok_or("followed")?;
        assert_eq!(
            e.to_string(),
            "path component 'link' is not a plain directory (Not a directory)"
        );
        std::fs::write(d.path().join("file"), "x")?;
        let e = open_dir(&root, &s(&["file", "x"]), true, &mut Vec::new())
            .err()
            .ok_or("opened")?;
        assert!(matches!(e, SafeFsError::Race(_)), "{e}");
        assert_eq!(std::fs::read_dir(outside.path())?.count(), 0);
        Ok(())
    }

    #[test]
    fn read_at_refuses_symlinks_dirs_and_fifos() -> R {
        let d = tempfile::tempdir()?;
        let fd = open_dir(&d.path().to_string_lossy(), &[], false, &mut Vec::new())?;
        std::fs::write(d.path().join("victim"), "v")?;
        std::os::unix::fs::symlink(d.path().join("victim"), d.path().join("leaf"))?;
        let e = read_at(&fd, "leaf").err().ok_or("followed")?;
        assert_eq!(
            e.to_string(),
            "'leaf' is not a plain file (Too many levels of symbolic links)"
        );
        std::fs::create_dir(d.path().join("dir"))?;
        assert_eq!(
            read_at(&fd, "dir").err().map(|e| e.to_string()),
            Some("'dir' is not a regular file".into())
        );
        nix::unistd::mkfifo(&d.path().join("fifo"), Mode::from_bits_truncate(0o600))?;
        assert_eq!(
            read_at(&fd, "fifo").err().map(|e| e.to_string()),
            Some("'fifo' is not a regular file".into())
        );
        Ok(())
    }

    #[test]
    fn write_tmp_is_exclusive_and_unlink_helpers() -> R {
        let d = tempfile::tempdir()?;
        let fd = open_dir(&d.path().to_string_lossy(), &[], false, &mut Vec::new())?;
        let a = write_tmp(&fd, b"1", 0o600)?;
        let b = write_tmp(&fd, b"2", 0o600)?;
        assert_ne!(a, b);
        unlink_quiet(&fd, &a);
        unlink_quiet(&fd, &a); // quiet when already gone
        let e = unlink_at(&fd, &a).err().ok_or("unlinked twice")?;
        assert_eq!(
            py_error(&e),
            (
                "FileNotFoundError".into(),
                format!("[Errno 2] No such file or directory: '{a}'")
            )
        );
        let e = replace(&fd, "nope", "x").err().ok_or("renamed")?;
        assert_eq!(py_error(&e).1, "[Errno 2] No such file or directory: 'nope' -> 'x'");
        unlink_at(&fd, &b)?;
        std::fs::create_dir(d.path().join("e"))?;
        rmdir_at(&fd, "e")?;
        assert_eq!(std::fs::read_dir(d.path())?.count(), 0);
        Ok(())
    }
}
