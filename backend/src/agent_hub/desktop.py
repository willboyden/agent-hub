"""`agent-hub open` / `agent-hub stop`: the desktop launcher's entry points (Linux).

`open` starts the hub in the background if nothing is serving yet, waits for `/api/v1/health`, then opens the UI with
`xdg-open`. It opens a page only on a loopback address, and only when every socket listening on the port belongs to the
current user and the answer looks like Agent Hub. A process running as the same user can still impersonate it
(docs/SECURITY.md section 13). It never reads or passes a key: the UI still asks for one. Concurrent launches are
serialised by `<data dir>/hub.lock`. `stop` stops the hub that `open` started (tracked in `<data dir>/hub.pid`). A
desktop launch has no terminal, so failures also go to `notify-send` when it exists.
"""
from __future__ import annotations

import contextlib
import fcntl
import html
import ipaddress
import json
import os
import shutil
import signal
import stat
import subprocess
import sys
import time
from collections.abc import Iterator
from pathlib import Path
from typing import Literal

import httpx
from pydantic import ValidationError

from agent_hub.config import Settings, load_settings

START_TIMEOUT_S = 30.0
STOP_TIMEOUT_S = 10.0
POLL_S = 0.5
PROBE_TIMEOUT_S = 2.0           # whole /health read, not per socket operation
PROBE_MAX_BYTES = 64 * 1024
LOG_ROTATE_BYTES = 10 * 1024 * 1024
HEALTH_KEYS = {"status", "version", "content_initialised", "adapters", "trust_loopback"}
PROC_NET_TABLES = ("/proc/net/tcp", "/proc/net/tcp6")
LOG_NAME = "hub.log"
PID_NAME = "hub.pid"
LOCK_NAME = "hub.lock"
WILDCARD_HOSTS = ("", "0.0.0.0", "::", "[::]")         # noqa: S104 - recognised so they are never started from here
# -I (isolated): the launch directory must not land on the admin server's sys.path, where a stray uvicorn.py or
# secrets.py would run inside it.
SERVER_ARGS = ["-I", "-m", "agent_hub.main"]
Check = Literal["hub", "other-user", "foreign", "down"]


class LauncherError(Exception):
    """A refusal the launcher reports (notification + exit status 1)."""


def hub_url(cfg: Settings) -> str:
    """The hub's URL, loopback only: neither the probe nor the browser may be sent off the box."""
    host = cfg.host.strip()
    if host in ("", "0.0.0.0"):                    # noqa: S104 - mapping a wildcard bind to loopback, not binding
        host = "127.0.0.1"
    elif host in ("::", "[::]"):
        host = "::1"
    if host != "localhost":
        try:
            ip = ipaddress.ip_address(host.removeprefix("[").removesuffix("]"))
        except ValueError:
            ip = None
        if ip is None or not ip.is_loopback:
            raise LauncherError(f"host {cfg.host!r} is not a loopback address; the launcher only opens a local hub")
        host = f"[{ip}]" if ip.version == 6 else str(ip)
    if not 1 <= cfg.port <= 65535:
        raise LauncherError(f"port {cfg.port} is out of range")
    return f"http://{host}:{cfg.port}/"


def listen_uids(port: int, tables: tuple[str, ...] = PROC_NET_TABLES) -> set[int] | None:
    """Owners of every LISTEN socket on `port`, any address, IPv4 and IPv6. None if a table is unreadable."""
    uids: set[int] = set()
    for table in tables:
        try:
            rows = Path(table).read_text(encoding="ascii").splitlines()[1:]
        except FileNotFoundError:
            continue                            # no IPv6
        except OSError:
            return None
        for row in rows:
            f = row.split()
            if len(f) > 7 and f[3] == "0A" and int(f[1].rsplit(":", 1)[1], 16) == port:
                uids.add(int(f[7]))
    return uids


def held_by_me(port: int) -> bool:
    uids = listen_uids(port)
    return bool(uids) and uids == {os.getuid()}


def probe(url: str) -> Literal["hub", "foreign", "down"]:
    """`hub` if the answer to /health has the hub's shape, `foreign` for any other answer, `down` for none. The body is
    capped in size and in total read time, so a listener that drips bytes cannot hold the launcher."""
    deadline = time.monotonic() + PROBE_TIMEOUT_S
    raw = bytearray()
    try:
        with httpx.stream("GET", f"{url}api/v1/health", timeout=PROBE_TIMEOUT_S, trust_env=False,
                          follow_redirects=False) as r:
            for chunk in r.iter_bytes():
                raw += chunk
                if len(raw) > PROBE_MAX_BYTES or time.monotonic() > deadline:
                    return "foreign"
    except httpx.HTTPError:
        return "down"
    try:
        body = json.loads(raw)
    except ValueError:
        return "foreign"
    return "hub" if isinstance(body, dict) and body.keys() >= HEALTH_KEYS else "foreign"


def check(url: str, port: int) -> Check:
    state = probe(url)
    if state == "hub" and not held_by_me(port):
        return "other-user"                     # answers like the hub, but not every listener is ours
    return state


def _private_data_dir(data_dir: Path) -> None:
    data_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    st = os.lstat(data_dir)
    if not stat.S_ISDIR(st.st_mode) or st.st_uid != os.getuid():
        raise LauncherError(f"data dir {data_dir} is not a directory you own")
    os.chmod(data_dir, 0o700)


def _open_private(path: Path, flags: int) -> int:
    """Open a regular file we own, never through a symlink or a FIFO, and force mode 0600."""
    fd = os.open(path, flags | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC, 0o600)
    st = os.fstat(fd)
    if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid():
        os.close(fd)
        raise LauncherError(f"{path} is not a regular file you own")
    os.fchmod(fd, 0o600)
    return fd


def _rotate(log: Path) -> None:
    try:
        st = os.lstat(log)
    except FileNotFoundError:
        return
    if stat.S_ISREG(st.st_mode) and st.st_size > LOG_ROTATE_BYTES:
        os.replace(log, log.with_name(log.name + ".1"))


@contextlib.contextmanager
def _launch_lock(data_dir: Path) -> Iterator[None]:
    """Serialise check-start-record across launches (a double click), so one hub is started and hub.pid names it."""
    fd = _open_private(data_dir / LOCK_NAME, os.O_RDWR)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)                            # closing the last descriptor releases the lock


def start_background(cfg: Settings) -> subprocess.Popen[bytes]:
    _private_data_dir(cfg.data_dir)
    log = cfg.data_dir / LOG_NAME
    _rotate(log)
    log_fd = _open_private(log, os.O_WRONLY | os.O_APPEND)
    try:        # both files are opened before the spawn, so a refused file never leaves an unrecorded hub running
        pid_fd = _open_private(cfg.data_dir / PID_NAME, os.O_WRONLY)
    except BaseException:
        os.close(log_fd)
        raise
    try:        # the child gets its own copies of the log descriptor
        proc = subprocess.Popen([sys.executable, *SERVER_ARGS], stdin=subprocess.DEVNULL, stdout=log_fd, stderr=log_fd,
                                start_new_session=True)
        os.ftruncate(pid_fd, 0)
        os.write(pid_fd, f"{proc.pid}\n".encode())
    finally:
        os.close(log_fd)
        os.close(pid_fd)
    return proc


def _stop_child(proc: subprocess.Popen[bytes]) -> None:
    proc.terminate()
    try:
        proc.wait(5)
    except subprocess.TimeoutExpired:
        proc.kill()


def open_browser(url: str) -> bool:
    opener = shutil.which("xdg-open")
    if opener is None:
        return False
    subprocess.Popen([opener, url], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                     start_new_session=True)
    return True


def notify(message: str) -> None:
    print(f"agent-hub: {message}", file=sys.stderr)
    tool = shutil.which("notify-send")
    if tool:
        with contextlib.suppress(subprocess.TimeoutExpired, OSError):    # the body may be parsed as markup
            subprocess.run([tool, "--", "Agent Hub", html.escape(message, quote=False)], check=False, timeout=5,
                           stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def _other_user(url: str) -> LauncherError:
    return LauncherError(f"another user's process is listening on the hub's port ({url}); not opening it")


def _open(cfg: Settings) -> None:
    url = hub_url(cfg)
    _private_data_dir(cfg.data_dir)
    with _launch_lock(cfg.data_dir):
        state = check(url, cfg.port)
        if state == "other-user":
            raise _other_user(url)
        if state == "foreign":
            raise LauncherError(f"something other than Agent Hub answers on {url}; not opening it")
        if state == "down":
            if cfg.host.strip() in WILDCARD_HOSTS:
                raise LauncherError(f"HUB_HOST {cfg.host!r} binds every interface; the launcher only starts a "
                                    "loopback-bound hub")
            _start_and_wait(cfg, url)
    if not open_browser(url):
        raise LauncherError(f"xdg-open not found; open {url} in a browser")


def _start_and_wait(cfg: Settings, url: str) -> None:
    proc = start_background(cfg)
    log = cfg.data_dir / LOG_NAME
    deadline = time.monotonic() + START_TIMEOUT_S
    while (state := check(url, cfg.port)) != "hub":             # check first: another hub may have won the port
        if state == "other-user":
            _stop_child(proc)
            raise _other_user(url)
        if proc.poll() is not None:
            raise LauncherError(f"the hub did not start; see {log}")
        if time.monotonic() > deadline:
            _stop_child(proc)
            raise LauncherError(f"the hub did not become healthy in {START_TIMEOUT_S:.0f} s; see {log}")
        time.sleep(POLL_S)
    if proc.poll() is not None:     # a hub of ours answers, but our child lost the port to it: do not record a dead pid
        (cfg.data_dir / PID_NAME).unlink(missing_ok=True)
        print(f"Agent Hub was started by something else; log of the failed start: {log}")
        return
    print(f"started Agent Hub in the background (pid {proc.pid}); log: {log}")


def _pid_cmdline(pid: int) -> tuple[int, list[str]] | None:
    """(owner uid, argv) of a live process, or None if it is gone."""
    try:
        uid = os.stat(f"/proc/{pid}").st_uid
        raw = Path(f"/proc/{pid}/cmdline").read_bytes()
    except OSError:
        return None
    return uid, [a.decode(errors="replace") for a in raw.split(b"\0") if a]


def _signal(pid: int, sig: int) -> None:
    os.kill(pid, sig)


def _stop(cfg: Settings) -> None:
    pid_file = cfg.data_dir / PID_NAME
    try:
        fd = os.open(pid_file, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    except FileNotFoundError:
        raise LauncherError("no hub started by `agent-hub open` is recorded") from None
    try:
        text = os.read(fd, 64).decode(errors="replace").strip()
    finally:
        os.close(fd)
    if not text.isdigit():
        raise LauncherError(f"{pid_file} does not hold a pid")
    pid = int(text)
    proc = _pid_cmdline(pid)
    if proc is None:
        pid_file.unlink(missing_ok=True)
        raise LauncherError("the hub started by `agent-hub open` is no longer running")
    uid, argv = proc
    if uid != os.getuid() or argv != [sys.executable, *SERVER_ARGS]:     # a recycled pid: never signal a stranger
        pid_file.unlink(missing_ok=True)
        raise LauncherError(f"pid {pid} is not a hub started by `agent-hub open`; nothing stopped")
    _signal(pid, signal.SIGTERM)
    deadline = time.monotonic() + STOP_TIMEOUT_S
    while _pid_cmdline(pid) is not None:
        if time.monotonic() > deadline:
            raise LauncherError(f"pid {pid} did not exit within {STOP_TIMEOUT_S:.0f} s")
        time.sleep(POLL_S)
    pid_file.unlink(missing_ok=True)
    print(f"stopped Agent Hub (pid {pid})")


def _run(command: str, argv: list[str], cfg: Settings | None) -> int:
    if argv:
        print(f"usage: agent-hub {command}", file=sys.stderr)
        return 2
    try:
        cfg = cfg or load_settings()
        (_open if command == "open" else _stop)(cfg)
    except ValidationError as e:        # names and reasons only: pydantic's text would echo the offending values
        notify("invalid setting: " + "; ".join(f"{'.'.join(map(str, err['loc']))}: {err['msg']}" for err in e.errors()))
        return 1
    except (LauncherError, OSError, ValueError) as e:     # ValueError: e.g. an unparsable /proc/net row
        notify(str(e))
        return 1
    return 0


def open_hub(argv: list[str], cfg: Settings | None = None) -> int:
    return _run("open", argv, cfg)


def stop_hub(argv: list[str], cfg: Settings | None = None) -> int:
    return _run("stop", argv, cfg)
