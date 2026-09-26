"""Doctor (read-only host exposure report), retire-bootstrap-key, and the interactive key prompt."""
from __future__ import annotations

import fnmatch
import json
import os
import stat
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest

from agent_hub import cli as cli_mod
from agent_hub.cli import run
from agent_hub.config import APP_ROOT, Settings
from agent_hub.services import doctor as doctor_mod
from agent_hub.services.doctor import DoctorContext, claude_settings_gaps, finding, run_doctor
from agent_hub.services.hub import Hub

from .conftest import add_client

STATE = ["~/.local/share/agent-hub", "~/.config/agent-hub", "~/.local/share/agent-knowledge", "~/.config/agent-knowledge"]


def good_settings() -> dict[str, Any]:
    deny = [f"{t}({p}/**)" for p in STATE for t in ("Read", "Write", "Edit")] + ["Write(**/hub/policy/**)", "Edit(**/hub/policy/**)"]
    return {"sandbox": {"credentials": {"files": [{"path": p, "mode": "deny"} for p in STATE]}, "network": {"allowedDomains": []}},
            "permissions": {"deny": deny}}


class FakeFS:
    def __init__(self, files: dict[str, str] | None = None, modes: dict[str, tuple[int, int]] | None = None,
                 file_modes: dict[str, int] | None = None) -> None:
        self.files = {str(k): v for k, v in (files or {}).items()}
        self.modes = {str(k): v for k, v in (modes or {}).items()}                 # dir path -> (mode, uid)
        self.file_modes = file_modes or {}

    def read_text(self, path: Path) -> str | None:
        return self.files.get(str(path))

    def stat(self, path: Path) -> Any | None:
        if str(path) in self.modes:
            m, u = self.modes[str(path)]
            return SimpleNamespace(st_mode=stat.S_IFDIR | m, st_uid=u)
        if str(path) in self.files:
            return SimpleNamespace(st_mode=stat.S_IFREG | self.file_modes.get(str(path), 0o600), st_uid=os.getuid())
        return None

    def glob(self, directory: Path, pattern: str) -> list[Path]:
        return sorted(Path(k) for k in self.files if Path(k).parent == directory and fnmatch.fnmatch(Path(k).name, pattern))


def ctx(tmp_path: Path, fs: FakeFS, **kw: Any) -> DoctorContext:
    home = tmp_path / "home"
    cfg = Settings(data_dir=tmp_path / "state", content_dir=tmp_path / "state" / "content", trust_loopback=False,
                   knowledge_admin_key_file=tmp_path / "state" / "kadmin.key")
    return DoctorContext(cfg=cfg, fs=fs, env={}, home=home, app_root=APP_ROOT, uid=os.getuid(), **kw)


def by_id(rep: dict[str, Any], fid: str) -> list[dict[str, str]]:
    return [f for f in rep["findings"] if f["id"] == fid or f["id"].startswith(fid)]


def test_findings_have_the_documented_shape_and_no_secrets(tmp_path: Path) -> None:
    rep = run_doctor(ctx(tmp_path, FakeFS({str(tmp_path / "state" / "bootstrap-admin.key"): "hc_SUPERSECRETKEYVALUE"})))
    assert rep["status"] in ("ok", "attention", "action_needed") and set(rep["counts"]) == {"crit", "warn", "info", "ok"}
    for f in rep["findings"]:
        assert set(f) == {"id", "severity", "title", "detail", "fix"} and f["severity"] in ("ok", "info", "warn", "crit")
    assert "SUPERSECRET" not in json.dumps(rep)


def test_bootstrap_key_file_present_warns_with_the_fix(tmp_path: Path) -> None:
    present = run_doctor(ctx(tmp_path, FakeFS({str(tmp_path / "state" / "bootstrap-admin.key"): "k"})))
    f = by_id(present, "bootstrap-key")[0]
    assert f["severity"] == "warn" and "hubctl retire-bootstrap-key" in f["fix"] and present["status"] == "attention"
    assert by_id(run_doctor(ctx(tmp_path, FakeFS())), "bootstrap-key")[0]["severity"] == "ok"


def test_loose_modes_are_reported_with_exact_chmod(tmp_path: Path) -> None:
    st = tmp_path / "state"
    fs = FakeFS({str(st / "hub.db"): "x", str(st / "attest.key"): "y"}, {str(st): (0o755, os.getuid()), str(st / "locks"): (0o700, os.getuid())},
                {str(st / "attest.key"): 0o644})
    f = by_id(run_doctor(ctx(tmp_path, fs)), "modes")[0]
    assert f["severity"] == "warn" and f"chmod 700 {st}" in f["fix"] and f"chmod 600 {st / 'attest.key'}" in f["fix"]
    assert "locks" not in f["detail"] and "hub.db" not in f["detail"]
    assert by_id(run_doctor(ctx(tmp_path, FakeFS())), "modes")[0]["severity"] == "ok"


def test_content_dir_location_and_ownership(tmp_path: Path) -> None:
    c = ctx(tmp_path, FakeFS({}, {str(tmp_path / "state" / "content"): (0o700, os.getuid() + 1)}))
    f = by_id(run_doctor(c), "content-dir")[0]
    assert f["severity"] == "crit" and "not owned" in f["detail"] and "migrate-content" in f["fix"]
    inside = ctx(tmp_path, FakeFS(), clients=[{"id": "c", "adapter": "fake", "roots": {"r": str(tmp_path)}, "manage": {}}])
    assert by_id(run_doctor(inside), "content-dir")[0]["severity"] == "crit"                # content dir inside a client root
    assert by_id(run_doctor(ctx(tmp_path, FakeFS())), "content-dir")[0]["severity"] == "ok"


def test_trust_loopback_and_bind_are_critical(tmp_path: Path) -> None:
    c = ctx(tmp_path, FakeFS())
    c.cfg.trust_loopback = True
    c.cfg.host = "0.0.0.0"  # noqa: S104
    rep = run_doctor(c)
    assert by_id(rep, "trust-loopback")[0]["severity"] == "crit" and by_id(rep, "bind")[0]["severity"] == "crit"
    assert rep["status"] == "action_needed"


PARAMS = {"settings_template": "config/settings.template.json", "network_allowlist_key": "sandbox.network.allowedDomains",
          "rerender_hint": "then run ./sync.sh to re-render"}


def proj_ctx(tmp_path: Path, template: str | None, live: str | None = None, adapter: str = "claude-code",
             params: dict[str, str] | None = None) -> DoctorContext:
    """A claude-code client whose host-specific paths all come from its params (nothing is guessed)."""
    proj = tmp_path / "proj"
    home = tmp_path / "home"
    files = {}
    if template is not None:
        files[str(proj / "config/settings.template.json")] = template
    if live is not None:
        files[str(home / ".config/example/settings.json")] = live
    p = {**PARAMS, "settings_rendered": str(home / ".config/example/settings.json"), **(params or {})}
    return ctx(tmp_path, FakeFS(files), clients=[{"id": "claude-code", "adapter": adapter, "roots": {"project": str(proj)}, "manage": {},
                                                  "params": p}])


def test_claude_template_ok_and_missing_entries(tmp_path: Path) -> None:
    ok = run_doctor(proj_ctx(tmp_path, json.dumps(good_settings())))
    assert by_id(ok, "claude-template")[0]["severity"] == "ok" and by_id(ok, "claude-network")[0]["severity"] == "info"
    bad = good_settings()
    bad["permissions"]["deny"] = [d for d in bad["permissions"]["deny"] if "agent-hub" not in d and "policy" not in d]
    bad["sandbox"]["credentials"]["files"] = bad["sandbox"]["credentials"]["files"][1:]
    f = by_id(run_doctor(proj_ctx(tmp_path, json.dumps(bad))), "claude-template")[0]
    assert f["severity"] == "warn" and "Read(~/.local/share/agent-hub/**)" in f["fix"] and "Write(**/hub/policy/**)" in f["fix"]
    assert '"path": "~/.local/share/agent-hub"' in f["fix"]


def test_rendered_live_config_is_compared_read_only(tmp_path: Path) -> None:
    stale = good_settings()
    stale["permissions"]["deny"] = []
    rep = run_doctor(proj_ctx(tmp_path, json.dumps(good_settings()), json.dumps(stale)))
    f = by_id(rep, "claude-rendered")[0]
    assert f["severity"] == "info" and "sync.sh" in f["fix"]
    both = run_doctor(proj_ctx(tmp_path, json.dumps(good_settings()), json.dumps(good_settings())))
    assert by_id(both, "claude-rendered")[0]["severity"] == "ok"
    assert not by_id(run_doctor(proj_ctx(tmp_path, json.dumps(good_settings()), None)), "claude-rendered")   # unreadable: skipped


def test_sandbox_network_loopback_is_flagged(tmp_path: Path) -> None:
    s = good_settings()
    s["sandbox"]["network"]["allowedDomains"] = ["pypi.org", "localhost", "127.0.0.1"]
    f = by_id(run_doctor(proj_ctx(tmp_path, json.dumps(s))), "claude-network")[0]
    assert f["severity"] == "warn" and "localhost" in f["title"] and "127.0.0.1" in f["title"]


@pytest.mark.parametrize("hostile", ["{not json", "[]", "null", '{"sandbox": 5, "permissions": "x"}',
                                     '{"sandbox": {"credentials": {"files": "no"}}, "permissions": {"deny": [1, null, {"a": 1}]}}',
                                     "[" * 5000, '"\\u0000"', ""])
def test_hostile_or_malformed_settings_never_crash(tmp_path: Path, hostile: str) -> None:
    rep = run_doctor(proj_ctx(tmp_path, hostile, hostile))
    assert rep["status"] in ("ok", "attention", "action_needed")
    assert not [f for f in rep["findings"] if f["id"].startswith("check-")]                 # no check itself blew up


@pytest.mark.parametrize("doc", [None, 5, [], {"sandbox": 5}, {"sandbox": {"credentials": {"files": [1, None, {"path": 3}]}},
                                                              "permissions": {"deny": [1, {"x": 1}]}}])
def test_gap_computation_tolerates_any_shape(doc: Any) -> None:
    assert len(claude_settings_gaps(doc)) == 4 * 4 + 2                                     # everything is missing, nothing raises


def test_template_missing_and_opencode_note(tmp_path: Path) -> None:
    assert by_id(run_doctor(proj_ctx(tmp_path, None)), "claude-template")[0]["severity"] == "info"
    rep = run_doctor(proj_ctx(tmp_path, None, adapter="opencode"))
    f = by_id(rep, "opencode-host")[0]
    assert f["severity"] == "info" and "docs/SECURITY.md" in f["fix"]
    assert not by_id(run_doctor(ctx(tmp_path, FakeFS())), "opencode-host")


def test_ptrace_scope(tmp_path: Path) -> None:
    for val, frag in (("0\n", "= 0"), ("1\n", "= 1"), ("garbage", "unknown")):
        f = by_id(run_doctor(ctx(tmp_path, FakeFS({"/proc/sys/kernel/yama/ptrace_scope": val}))), "ptrace")[0]
        assert f["severity"] == "info" and frag in f["title"]
    assert "sysctl" in by_id(run_doctor(ctx(tmp_path, FakeFS({"/proc/sys/kernel/yama/ptrace_scope": "0"}))), "ptrace")[0]["fix"]


def test_systemd_units_read_write_paths(tmp_path: Path) -> None:
    home = tmp_path / "home"
    ud = home / ".config/systemd/user"
    parent = APP_ROOT.parent
    files = {str(ud / "agent-hub.service"): f"[Service]\nReadWritePaths={parent} %h/.local/share/agent-hub\n",
             str(ud / "agent-hub-x.service"): f"[Service]\nReadWritePaths={APP_ROOT / 'policy'}\n",
             str(ud / "agent-knowledge.service"): "[Service]\nReadWritePaths=%h/.local/share/agent-knowledge -/tmp/x\n"
                                                  f"ReadWritePaths={parent / 'hub/out'}\n",
             str(ud / "other.service"): f"[Service]\nReadWritePaths={parent}\n"}
    rep = run_doctor(ctx(tmp_path, FakeFS(files)))
    sev = {f["id"]: f["severity"] for f in rep["findings"] if f["id"].startswith("unit-")}
    assert sev == {"unit-agent-hub.service": "warn", "unit-agent-hub-x.service": "warn", "unit-agent-knowledge.service": "ok"}
    assert not any(f["id"].startswith("unit-") for f in run_doctor(ctx(tmp_path, FakeFS()))["findings"])


def test_knowledge_summary_states(tmp_path: Path) -> None:
    assert by_id(run_doctor(ctx(tmp_path, FakeFS())), "knowledge")[0]["severity"] == "info"
    assert by_id(run_doctor(ctx(tmp_path, FakeFS(), knowledge={"reachable": False})), "knowledge")[0]["severity"] == "info"
    f = by_id(run_doctor(ctx(tmp_path, FakeFS(), knowledge={"reachable": True, "authenticated": False, "key_source": "none"})), "knowledge")[0]
    assert f["severity"] == "warn" and "HUB_KNOWLEDGE_ADMIN_KEY" in f["fix"]
    ok = run_doctor(ctx(tmp_path, FakeFS(), knowledge={"reachable": True, "authenticated": True, "key_source": "file"}))
    assert by_id(ok, "knowledge")[0]["severity"] == "ok"


def test_advisory_permission_gap_lists_exact_lines(tmp_path: Path) -> None:
    clients = [{"id": "a", "adapter": "generic", "roots": {}, "manage": {"permissions": False}},
               {"id": "b", "adapter": "generic", "roots": {}, "manage": {"permissions": True}},
               {"id": "c", "adapter": "opencode", "roots": {}, "manage": {"permissions": False}}]
    gaps = {"a": [f"deny path_read ~/.aws/{i}" for i in range(12)], "b": ["deny path_read x"], "c": None}
    rep = run_doctor(ctx(tmp_path, FakeFS(), clients=clients, permission_gaps=lambda cid: gaps.get(cid)))
    fs = [f for f in rep["findings"] if f["id"].startswith("advisory-gap")]
    assert [f["id"] for f in fs] == ["advisory-gap-a"] and "deny path_read ~/.aws/0" in fs[0]["fix"] and fs[0]["fix"].endswith("...")


def test_a_crashing_check_does_not_hide_the_others(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    def boom(_c: DoctorContext) -> list[dict[str, str]]:
        raise RuntimeError("secret detail hc_LEAK")

    monkeypatch.setattr(doctor_mod, "CHECKS", (boom, *doctor_mod.CHECKS))
    rep = run_doctor(ctx(tmp_path, FakeFS()))
    assert any(f["id"] == "check-boom" for f in rep["findings"]) and "LEAK" not in json.dumps(rep) and by_id(rep, "modes")


# ---- API / hub integration ---------------------------------------------------------------------------------------------------
def test_hub_doctor_and_endpoints(api: Any, hub: Hub, home: Path, admin_key: str, viewer_key: str) -> None:
    add_client(hub, home)
    rep = hub.doctor(None, fs=FakeFS({}), home=home)
    assert rep["status"] in ("ok", "attention", "action_needed") and by_id(rep, "trust-loopback")[0]["severity"] == "ok"
    h = {"Authorization": f"Bearer {admin_key}"}
    v = {"Authorization": f"Bearer {viewer_key}"}
    full = api.get("/api/v1/doctor", headers=h).json()
    assert set(full) == {"status", "counts", "findings"} and all(set(f) == {"id", "severity", "title", "detail", "fix"} for f in full["findings"])
    assert api.get("/api/v1/doctor", headers=v).status_code == 403                       # the full report is admin only
    s = api.get("/api/v1/doctor/summary", headers=v).json()
    assert set(s) == {"crit", "warn", "info"} and all(isinstance(x, int) for x in s.values())
    assert api.get("/api/v1/doctor/summary").status_code == 401


def test_cli_doctor(hub: Hub, capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("HUB_API_KEY", str(hub.auth.create_key("k", "admin")["secret"]))
    assert run(["doctor"], hub=hub) in (0, 2)
    assert "status:" in capsys.readouterr().out
    assert run(["doctor", "--json"], hub=hub) in (0, 2)
    assert json.loads(capsys.readouterr().out)["counts"]["ok"] >= 1
    hub.cfg.trust_loopback = True
    assert run(["doctor"], hub=hub) == 2                                                   # a critical finding exits 2


# ---- interactive key + retire ------------------------------------------------------------------------------------------------
class Tty:
    def __init__(self, tty: bool) -> None:
        self.tty = tty

    def isatty(self) -> bool:
        return self.tty


def test_key_prompt_only_on_a_tty_and_never_echoed(hub: Hub, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
    key = str(hub.auth.create_key("k", "admin")["secret"])
    asked: list[str] = []
    monkeypatch.setattr(cli_mod.getpass, "getpass", lambda prompt="": asked.append(prompt) or key)
    monkeypatch.setattr(cli_mod.sys, "stdin", Tty(False))
    assert run(["validate"], hub=hub) == 1 and not asked                                    # non-TTY without a key stays an error
    monkeypatch.setattr(cli_mod.sys, "stdin", Tty(True))
    assert run(["validate"], hub=hub) == 0 and len(asked) == 1
    out = capsys.readouterr()
    assert key not in out.out + out.err
    monkeypatch.setenv("HUB_API_KEY", key)
    asked.clear()
    assert run(["validate"], hub=hub) == 0 and not asked                                    # a key at hand: no prompt
    monkeypatch.delenv("HUB_API_KEY")
    monkeypatch.setattr(cli_mod.getpass, "getpass", lambda prompt="": "hc_wrong")
    assert run(["validate"], hub=hub) == 1


def test_retire_bootstrap_key(hub: Hub, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
    f = hub.cfg.data_dir / "bootstrap-admin.key"
    key = str(hub.auth.create_key("boot", "admin")["secret"])
    f.write_text(key + "\n")
    f.chmod(0o600)
    viewer = str(hub.auth.create_key("v", "viewer")["secret"])
    monkeypatch.setattr(cli_mod.sys, "stdin", Tty(False))
    assert run(["retire-bootstrap-key"], hub=hub) == 1 and f.exists()                       # refuses without a TTY
    assert "terminal" in capsys.readouterr().err
    monkeypatch.setattr(cli_mod.sys, "stdin", Tty(True))
    for wrong in ("hc_nope", viewer, ""):
        monkeypatch.setattr(cli_mod.getpass, "getpass", lambda prompt="", w=wrong: w)
        assert run(["retire-bootstrap-key"], hub=hub) == 1 and f.exists() and f.read_text().strip() == key   # untouched
    seen: dict[str, bytes] = {}
    real_unlink = Path.unlink

    def spy(self: Path, *a: Any, **k: Any) -> None:
        if self == f:
            seen["before_unlink"] = self.read_bytes()
        real_unlink(self, *a, **k)

    monkeypatch.setattr(Path, "unlink", spy)
    monkeypatch.setattr(cli_mod.getpass, "getpass", lambda prompt="": key)
    capsys.readouterr()
    assert run(["retire-bootstrap-key"], hub=hub) == 0 and not f.exists()
    assert set(seen["before_unlink"]) <= {0} and len(seen["before_unlink"]) == len(key) + 1        # zeroed first, then unlinked
    out = capsys.readouterr().out
    assert "password manager" in out and key not in out
    assert run(["retire-bootstrap-key"], hub=hub) == 0 and "already gone" in capsys.readouterr().out       # idempotent
    monkeypatch.setattr(cli_mod.sys, "stdin", Tty(True))                                    # the key itself still works, via the prompt
    assert run(["validate"], hub=hub) == 0


def test_retire_refuses_a_symlinked_key_path(hub: Hub, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    key = str(hub.auth.create_key("boot", "admin")["secret"])
    target = tmp_path / "precious"
    target.write_text("do not zero me")
    (hub.cfg.data_dir / "bootstrap-admin.key").symlink_to(target)
    monkeypatch.setattr(cli_mod.sys, "stdin", Tty(True))
    monkeypatch.setattr(cli_mod.getpass, "getpass", lambda prompt="": key)
    assert run(["retire-bootstrap-key"], hub=hub) == 1 and target.read_text() == "do not zero me"


def test_finding_helper() -> None:
    assert finding("x", "ok", "t") == {"id": "x", "severity": "ok", "title": "t", "detail": "", "fix": ""}
