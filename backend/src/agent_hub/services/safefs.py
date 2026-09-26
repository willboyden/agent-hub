"""Directory-fd relative file operations: every path component is opened with O_NOFOLLOW|O_DIRECTORY relative to the previous
directory fd, so a component swapped for a symlink between planning and writing cannot redirect a write (TOCTOU)."""
from __future__ import annotations

import contextlib
import os
import secrets


class RaceError(Exception):
    """The live tree changed under us (symlink swapped in, file edited since planning)."""


def open_dir(root: str, parts: list[str], create: bool, created: list[list[str]]) -> int:
    """Open root/parts as a directory fd, refusing symlinks at every step. Missing components are created (0755) when
    `create`, and recorded in `created` (as component lists from the root) so a rollback can remove them."""
    fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
    try:
        for i, part in enumerate(parts):
            try:
                nfd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            except FileNotFoundError:
                if not create:
                    raise
                os.mkdir(part, 0o755, dir_fd=fd)
                created.append(parts[: i + 1])
                nfd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            except OSError as exc:                       # ELOOP / ENOTDIR: a symlink or a file where a directory should be
                raise RaceError(f"path component {part!r} is not a plain directory ({exc.strerror})") from exc
            os.close(fd)
            fd = nfd
        return fd
    except BaseException:
        os.close(fd)
        raise


def read_at(dirfd: int, name: str) -> tuple[bytes | None, int | None]:
    """(content, mode) of a regular file, (None, None) when absent. A symlink or special file is a RaceError."""
    try:
        fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=dirfd)
    except FileNotFoundError:
        return None, None
    except OSError as exc:
        raise RaceError(f"{name!r} is not a plain file ({exc.strerror})") from exc
    with os.fdopen(fd, "rb") as fh:
        st = os.fstat(fh.fileno())
        if (st.st_mode & 0o170000) != 0o100000:
            raise RaceError(f"{name!r} is not a regular file")
        return fh.read(), st.st_mode & 0o777


def write_tmp(dirfd: int, data: bytes, mode: int) -> str:
    name = f".hub-tmp-{secrets.token_hex(6)}"
    fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=dirfd)
    with os.fdopen(fd, "wb") as fh:
        fh.write(data)
        fh.flush()
        os.fsync(fh.fileno())
        os.fchmod(fh.fileno(), mode)
    return name


def replace(dirfd: int, tmp: str, name: str) -> None:
    os.replace(tmp, name, src_dir_fd=dirfd, dst_dir_fd=dirfd)
    with contextlib.suppress(OSError):
        os.fsync(dirfd)


def unlink_quiet(dirfd: int, name: str) -> None:
    with contextlib.suppress(OSError):
        os.unlink(name, dir_fd=dirfd)
