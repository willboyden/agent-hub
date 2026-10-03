"""`agent-hub open` / `stop`: URL, ownership check, background start and stop (all faked: nothing is started,
opened or signalled)."""
from __future__ import annotations

import os
import signal
import stat
import sys
import threading
import time
from pathlib import Path
from typing import Any

import httpx
import pytest

from agent_hub import desktop
from agent_hub.config import Settings

URL = "http://127.0.0.1:8792/"
HEALTH = {"status": "ok", "version": "0.1.0", "content_initialised": True, "adapters": ["claude-code"],
          "trust_loopback": False, "warnings": []}
TCP_HEADER = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n"


class FakeProc:
    pid = 4242

    def __init__(self, exit_code: int | None = None) -> None:
        self.exit_code = exit_code
        self.terminated = False

    def poll(self) -> int | None:
        return self.exit_code

    def terminate(self) -> None:
        self.terminated = True

    def wait(self, timeout: float) -> int:
        return 0


@pytest.fixture
def launcher(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> dict[str, Any]:
    rec: dict[str, Any] = {"checks": [], "started": 0, "opened": [], "notes": [], "proc": FakeProc(),
                           "cfg": Settings(host="127.0.0.1", port=8792, data_dir=tmp_path)}

    def fake_check(url: str, port: int) -> str:
        assert url in (URL, "http://[::1]:8792/") and port == 8792
        return str(rec["checks"].pop(0) if len(rec["checks"]) > 1 else rec["checks"][0])

    def fake_start(cfg: Settings) -> FakeProc:
        rec["started"] += 1
        (cfg.data_dir / "hub.pid").write_text(f"{FakeProc.pid}\n")
        proc: FakeProc = rec["proc"]
        return proc

    def fake_open(url: str) -> bool:
        rec["opened"].append(url)
        return True

    monkeypatch.setattr(desktop, "check", fake_check)
    monkeypatch.setattr(desktop, "start_background", fake_start)
    monkeypatch.setattr(desktop, "open_browser", fake_open)
    monkeypatch.setattr(desktop, "notify", rec["notes"].append)
    monkeypatch.setattr(desktop, "POLL_S", 0)
    return rec


# --- URL -------------------------------------------------------------------------------------------------------

def test_hub_url_maps_wildcard_binds_to_loopback() -> None:
    assert desktop.hub_url(Settings(host="127.0.0.1", port=8792)) == URL
    assert desktop.hub_url(Settings(host="0.0.0.0", port=9000)) == "http://127.0.0.1:9000/"
    assert desktop.hub_url(Settings(host="::", port=8792)) == "http://[::1]:8792/"
    assert desktop.hub_url(Settings(host="::1", port=8792)) == "http://[::1]:8792/"
    assert desktop.hub_url(Settings(host="localhost", port=8792)) == "http://localhost:8792/"


@pytest.mark.parametrize("host", ["127.0.0.1@evil.example", "evil.example/#", "evil.example", "192.168.1.7", "10.0.0.1"])
def test_hub_url_refuses_anything_off_the_box(host: str) -> None:
    with pytest.raises(desktop.LauncherError, match="not a loopback address"):
        desktop.hub_url(Settings(host=host, port=8792))


def test_hub_url_refuses_an_out_of_range_port() -> None:
    with pytest.raises(desktop.LauncherError, match="out of range"):
        desktop.hub_url(Settings(host="127.0.0.1", port=70000))


# --- who holds the port ----------------------------------------------------------------------------------------

def test_listen_uids_reads_every_listener_on_the_port(tmp_path: Path) -> None:
    tcp, tcp6 = tmp_path / "tcp", tmp_path / "tcp6"
    tcp.write_text(TCP_HEADER
                   + "   0: 0100007F:2258 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 11\n"
                   + "   1: 0100007F:2258 0100007F:9C40 01 00000000:00000000 00:00000000 00000000     0        0 12\n"
                   + "   2: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 13\n")
    tcp6.write_text(TCP_HEADER
                    + "   0: 00000000000000000000000000000000:2258 00000000000000000000000000000000:0000 0A "
                      "00000000:00000000 00:00000000 00000000   999        0 14\n")
    assert desktop.listen_uids(8792, (str(tcp),)) == {1000}                  # ESTABLISHED and other ports ignored
    assert desktop.listen_uids(8792, (str(tcp), str(tcp6))) == {1000, 999}   # a wildcard IPv6 squatter counts
    assert desktop.listen_uids(8792, (str(tmp_path / "missing"),)) == set()


def test_check_flags_a_hub_shaped_answer_from_another_users_socket(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(desktop, "probe", lambda url: "hub")
    monkeypatch.setattr(desktop, "listen_uids", lambda port: {0})
    assert desktop.check(URL, 8792) == "other-user"
    monkeypatch.setattr(desktop, "listen_uids", lambda port: {os.getuid(), 0})
    assert desktop.check(URL, 8792) == "other-user"
    monkeypatch.setattr(desktop, "listen_uids", lambda port: None)            # unreadable: fail closed
    assert desktop.check(URL, 8792) == "other-user"
    monkeypatch.setattr(desktop, "listen_uids", lambda port: {os.getuid()})
    assert desktop.check(URL, 8792) == "hub"


class FakeStream:
    def __init__(self, chunks: list[bytes], delay: float = 0.0) -> None:
        self.chunks, self.delay = chunks, delay

    def __enter__(self) -> FakeStream:
        return self

    def __exit__(self, *exc: object) -> None:
        return None

    def iter_bytes(self) -> Any:
        for c in self.chunks:
            time.sleep(self.delay)
            yield c


@pytest.fixture
def stream(monkeypatch: pytest.MonkeyPatch) -> list[Any]:
    answer: list[Any] = []

    def fake_stream(method: str, url: str, **kw: Any) -> FakeStream:
        assert (method, url) == ("GET", f"{URL}api/v1/health")
        assert kw["trust_env"] is False and kw["follow_redirects"] is False
        if isinstance(answer[0], Exception):
            raise answer[0]
        fs: FakeStream = answer[0]
        return fs

    monkeypatch.setattr(desktop.httpx, "stream", fake_stream)
    return answer


def test_probe_tells_hub_foreign_and_down_apart(stream: list[Any]) -> None:
    import json
    for resp, want in [(FakeStream([json.dumps(HEALTH).encode()]), "hub"),
                       (FakeStream([b"<html>some other service</html>"]), "foreign"),
                       (FakeStream([b'{"status": "ok"}']), "foreign"),
                       (httpx.ConnectError("connection refused"), "down")]:
        stream[:] = [resp]
        assert desktop.probe(URL) == want


def test_probe_caps_the_body_and_the_total_read_time(stream: list[Any], monkeypatch: pytest.MonkeyPatch) -> None:
    stream[:] = [FakeStream([b" " * 1024] * 100)]                      # 100 KiB of padding before any JSON
    assert desktop.probe(URL) == "foreign"
    monkeypatch.setattr(desktop, "PROBE_TIMEOUT_S", 0.05)
    stream[:] = [FakeStream([b" "] * 1000, delay=0.01)]                # a drip that would take 10 s
    t0 = time.monotonic()
    assert desktop.probe(URL) == "foreign" and time.monotonic() - t0 < 1


# --- open ------------------------------------------------------------------------------------------------------

def test_open_when_already_running_starts_nothing(launcher: dict[str, Any]) -> None:
    launcher["checks"] = ["hub"]
    assert desktop.open_hub([], launcher["cfg"]) == 0
    assert launcher["started"] == 0 and launcher["opened"] == [URL] and launcher["notes"] == []


def test_open_starts_the_hub_then_waits_for_health(launcher: dict[str, Any]) -> None:
    launcher["checks"] = ["down", "down", "hub"]
    assert desktop.open_hub([], launcher["cfg"]) == 0
    assert launcher["started"] == 1 and launcher["opened"] == [URL]


@pytest.mark.parametrize(("state", "note"), [("foreign", "something other than Agent Hub"),
                                             ("other-user", "another user's process")])
def test_open_refuses_a_port_it_cannot_trust(launcher: dict[str, Any], state: str, note: str) -> None:
    launcher["checks"] = [state]
    assert desktop.open_hub([], launcher["cfg"]) == 1
    assert launcher["started"] == 0 and launcher["opened"] == [] and note in launcher["notes"][0]


def test_open_refuses_a_squatter_that_wins_the_port_during_start(launcher: dict[str, Any]) -> None:
    launcher["checks"] = ["down", "other-user"]            # our child lost the bind; the squatter answers
    launcher["proc"] = FakeProc(exit_code=1)
    assert desktop.open_hub([], launcher["cfg"]) == 1
    assert launcher["opened"] == [] and "another user's process" in launcher["notes"][0]


def test_open_reports_a_hub_that_exits_during_start(launcher: dict[str, Any]) -> None:
    launcher["checks"] = ["down"]
    launcher["proc"] = FakeProc(exit_code=1)
    assert desktop.open_hub([], launcher["cfg"]) == 1
    assert launcher["opened"] == [] and "hub.log" in launcher["notes"][0]


def test_open_stops_its_child_after_the_start_timeout(launcher: dict[str, Any],
                                                      monkeypatch: pytest.MonkeyPatch) -> None:
    launcher["checks"] = ["down"]
    monkeypatch.setattr(desktop, "START_TIMEOUT_S", 0)
    assert desktop.open_hub([], launcher["cfg"]) == 1
    assert launcher["opened"] == [] and launcher["proc"].terminated


def test_open_refuses_a_non_loopback_host_before_probing(launcher: dict[str, Any], tmp_path: Path) -> None:
    assert desktop.open_hub([], Settings(host="evil.example", port=8792, data_dir=tmp_path)) == 1
    assert launcher["started"] == 0 and "not a loopback address" in launcher["notes"][0]


def test_open_does_not_record_a_child_that_lost_the_port_to_another_hub(launcher: dict[str, Any]) -> None:
    launcher["checks"] = ["down", "hub"]                   # a hub of ours answers, but our child has exited
    launcher["proc"] = FakeProc(exit_code=1)
    assert desktop.open_hub([], launcher["cfg"]) == 0
    assert launcher["opened"] == [URL] and not (launcher["cfg"].data_dir / "hub.pid").exists()


def test_concurrent_launches_wait_for_each_other(launcher: dict[str, Any]) -> None:
    import fcntl
    launcher["checks"] = ["hub"]
    fd = os.open(launcher["cfg"].data_dir / "hub.lock", os.O_RDWR | os.O_CREAT, 0o600)
    fcntl.flock(fd, fcntl.LOCK_EX)                         # another launch is mid-start
    t = threading.Thread(target=desktop.open_hub, args=([], launcher["cfg"]))
    t.start()
    time.sleep(0.3)
    assert launcher["opened"] == []                        # blocked on the lock, has not even probed
    os.close(fd)
    t.join(5)
    assert launcher["opened"] == [URL] and launcher["started"] == 0


@pytest.mark.parametrize("host", ["0.0.0.0", "::"])
def test_open_never_starts_a_wildcard_bound_hub(launcher: dict[str, Any], tmp_path: Path, host: str) -> None:
    cfg = Settings(host=host, port=8792, data_dir=tmp_path)
    launcher["checks"] = ["down"]
    assert desktop.open_hub([], cfg) == 1
    assert launcher["started"] == 0 and "binds every interface" in launcher["notes"][0]


def test_open_and_stop_reject_arguments(launcher: dict[str, Any]) -> None:
    assert desktop.open_hub(["--port", "1"], launcher["cfg"]) == 2
    assert desktop.stop_hub(["now"], launcher["cfg"]) == 2
    assert launcher["started"] == 0


# --- background start ------------------------------------------------------------------------------------------

@pytest.fixture
def popen(monkeypatch: pytest.MonkeyPatch) -> list[tuple[list[str], dict[str, Any]]]:
    calls: list[tuple[list[str], dict[str, Any]]] = []

    def fake_popen(args: list[str], **kw: Any) -> FakeProc:
        calls.append((args, kw))
        return FakeProc()

    monkeypatch.setattr(desktop.subprocess, "Popen", fake_popen)
    return calls


def test_start_background_is_isolated_detached_and_private(popen: list[tuple[list[str], dict[str, Any]]],
                                                           tmp_path: Path) -> None:
    state = tmp_path / "state"
    desktop.start_background(Settings(data_dir=state))
    args, kw = popen[0]
    assert args == [sys.executable, "-I", "-m", "agent_hub.main"] and kw["start_new_session"] is True
    assert stat.S_IMODE(state.stat().st_mode) == 0o700
    for name in ("hub.log", "hub.pid"):
        assert stat.S_IMODE((state / name).stat().st_mode) == 0o600
    assert (state / "hub.pid").read_text() == "4242\n"


def test_start_background_refuses_a_symlinked_log(popen: list[tuple[list[str], dict[str, Any]]],
                                                  tmp_path: Path) -> None:
    state, target = tmp_path / "state", tmp_path / "elsewhere"
    state.mkdir(mode=0o700)
    target.write_text("keep\n")
    (state / "hub.log").symlink_to(target)
    with pytest.raises(OSError):
        desktop.start_background(Settings(data_dir=state))
    assert popen == [] and target.read_text() == "keep\n"


def test_start_background_refuses_a_symlinked_pid_file_before_spawning(popen: list[tuple[list[str], dict[str, Any]]],
                                                                       tmp_path: Path) -> None:
    state, target = tmp_path / "state", tmp_path / "elsewhere"
    state.mkdir(mode=0o700)
    target.write_text("keep\n")
    (state / "hub.pid").symlink_to(target)
    with pytest.raises(OSError):
        desktop.start_background(Settings(data_dir=state))
    assert popen == [] and target.read_text() == "keep\n"


def test_start_background_tightens_an_existing_log_and_rotates_a_big_one(
        popen: list[tuple[list[str], dict[str, Any]]], tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    state = tmp_path / "state"
    state.mkdir(mode=0o755)
    log = state / "hub.log"
    log.write_text("x" * 20)
    log.chmod(0o644)
    monkeypatch.setattr(desktop, "LOG_ROTATE_BYTES", 10)
    desktop.start_background(Settings(data_dir=state))
    assert (state / "hub.log.1").read_text() == "x" * 20
    assert stat.S_IMODE(log.stat().st_mode) == 0o600 and stat.S_IMODE(state.stat().st_mode) == 0o700


# --- stop ------------------------------------------------------------------------------------------------------

@pytest.fixture
def stopper(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> dict[str, Any]:
    rec: dict[str, Any] = {"alive": [], "signals": [], "notes": [], "cfg": Settings(data_dir=tmp_path)}
    (tmp_path / "hub.pid").write_text("4242\n")

    def fake_cmdline(pid: int) -> tuple[int, list[str]] | None:
        assert pid == 4242
        if not rec["alive"]:
            return None
        proc: tuple[int, list[str]] = rec["alive"].pop(0)
        return proc

    monkeypatch.setattr(desktop, "_pid_cmdline", fake_cmdline)
    monkeypatch.setattr(desktop, "_signal", lambda pid, sig: rec["signals"].append((pid, sig)))
    monkeypatch.setattr(desktop, "notify", rec["notes"].append)
    monkeypatch.setattr(desktop, "POLL_S", 0)
    return rec


def test_stop_terminates_the_hub_it_started(stopper: dict[str, Any]) -> None:
    me = (os.getuid(), [sys.executable, "-I", "-m", "agent_hub.main"])
    stopper["alive"] = [me, me]                         # identity check, then one poll before it exits
    assert desktop.stop_hub([], stopper["cfg"]) == 0
    assert stopper["signals"] == [(4242, signal.SIGTERM)]
    assert not (stopper["cfg"].data_dir / "hub.pid").exists()


@pytest.mark.parametrize("proc", [(0, [sys.executable, "-I", "-m", "agent_hub.main"]),     # another user
                                  (os.getuid(), ["/usr/bin/vim", "backend/src/agent_hub/main.py"]),  # recycled pid
                                  (os.getuid(), ["/usr/bin/python3", "-I", "-m", "agent_hub.main"])])  # other interpreter
def test_stop_never_signals_a_process_it_did_not_start(stopper: dict[str, Any],
                                                       proc: tuple[int, list[str]]) -> None:
    stopper["alive"] = [proc]
    assert desktop.stop_hub([], stopper["cfg"]) == 1
    assert stopper["signals"] == [] and "nothing stopped" in stopper["notes"][0]


def test_stop_reports_a_hub_that_is_already_gone(stopper: dict[str, Any]) -> None:
    assert desktop.stop_hub([], stopper["cfg"]) == 1
    assert stopper["signals"] == [] and not (stopper["cfg"].data_dir / "hub.pid").exists()


# --- entry point -----------------------------------------------------------------------------------------------

def test_a_malformed_setting_is_reported_not_raised(monkeypatch: pytest.MonkeyPatch) -> None:
    notes: list[str] = []
    monkeypatch.setattr(desktop, "notify", notes.append)
    monkeypatch.setenv("HUB_PORT", "value-that-must-not-be-echoed")
    assert desktop.open_hub([]) == 1 and len(notes) == 1
    assert "port" in notes[0] and "value-that-must-not-be-echoed" not in notes[0]


@pytest.mark.parametrize("cmd", ["open", "stop"])
def test_agent_hub_dispatches_open_and_stop_to_the_launcher(monkeypatch: pytest.MonkeyPatch, cmd: str) -> None:
    from agent_hub import main

    seen: list[tuple[str, list[str]]] = []
    monkeypatch.setattr(desktop, "open_hub", lambda argv: seen.append(("open", argv)) or 0)
    monkeypatch.setattr(desktop, "stop_hub", lambda argv: seen.append(("stop", argv)) or 0)
    monkeypatch.setattr(main.uvicorn, "run", lambda *a, **kw: pytest.fail("the launcher must not serve in-process"))
    monkeypatch.setattr(sys, "argv", ["agent-hub", cmd])
    with pytest.raises(SystemExit) as exc:
        main.run()
    assert exc.value.code == 0 and seen == [(cmd, [])]
