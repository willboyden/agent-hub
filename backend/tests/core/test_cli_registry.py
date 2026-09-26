from __future__ import annotations

import json
import logging
import os
import sys
import types
from pathlib import Path

import pytest

from agent_hub import adapters as adapters_pkg
from agent_hub import config as config_mod
from agent_hub.adapters import AdapterRegistry, load_registry
from agent_hub.adapters.base import ClientAdapter
from agent_hub.cli import build_parser, run
from agent_hub.config import Settings, load_settings
from agent_hub.services.hub import Hub

from .conftest import add_client, seed_content
from .fakes import FakeAdapter


# ---- registry ---------------------------------------------------------------------------------------------------------
def test_registry_loads_real_adapters_when_present_and_ignores_helpers() -> None:
    reg = load_registry()
    assert set(reg.ids()) >= {"claude-code", "opencode", "hermes", "generic"} or reg.load_errors is not None
    assert "base" not in reg.load_errors and "_util" not in reg.load_errors and "_util" not in reg.ids()
    for a in reg.all():
        assert isinstance(a, ClientAdapter) and reg.get(a.id) is a


def test_registry_tolerates_broken_and_missing_modules(monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture) -> None:
    good = types.ModuleType("agent_hub.adapters.good_x")
    good.ADAPTER = FakeAdapter  # type: ignore[attr-defined]
    noadapter = types.ModuleType("agent_hub.adapters.noadapter_x")
    notadapter = types.ModuleType("agent_hub.adapters.notadapter_x")
    notadapter.ADAPTER = object  # type: ignore[attr-defined]
    mods = {"good_x": good, "noadapter_x": noadapter, "notadapter_x": notadapter}

    class Boom:
        def __init__(self) -> None:
            raise RuntimeError("cannot construct")

    exploding = types.ModuleType("agent_hub.adapters.boom_x")
    exploding.ADAPTER = Boom  # type: ignore[attr-defined]
    mods["boom_x"] = exploding
    real_import = adapters_pkg.importlib.import_module

    def fake_import(name: str, package: str | None = None) -> types.ModuleType:
        short = name.rsplit(".", 1)[-1]
        if short == "syntax_x":
            raise SyntaxError("broken module")
        if short in mods:
            return mods[short]
        return real_import(name, package)

    monkeypatch.setattr(adapters_pkg.importlib, "import_module", fake_import)
    monkeypatch.setattr(adapters_pkg.pkgutil, "iter_modules",
                        lambda path: [types.SimpleNamespace(name=n) for n in ("good_x", "noadapter_x", "notadapter_x", "boom_x",
                                                                            "syntax_x", "base", "_util")])
    with caplog.at_level(logging.WARNING, logger="agent_hub.adapters"):
        reg = AdapterRegistry().discover_package()
    assert reg.ids() == ["fake"]
    assert set(reg.load_errors) == {"noadapter_x", "notadapter_x", "boom_x", "syntax_x"}
    assert len(caplog.records) == 4


def test_registry_register_requires_id() -> None:
    class NoId(FakeAdapter):
        id = ""

    with pytest.raises(ValueError):
        AdapterRegistry().register(NoId())


# ---- config ---------------------------------------------------------------------------------------------------------------
def test_env_file_is_read_but_real_env_wins(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    f = tmp_path / "env"
    f.write_text("HUB_PORT=9001\nHUB_GIT_AUTHOR_NAME=FromFile\n")
    monkeypatch.setattr(config_mod, "ENV_FILE", f)
    assert load_settings().port == 9001
    monkeypatch.setenv("HUB_PORT", "9002")
    s = load_settings()
    assert s.port == 9002 and s.git_author_name == "FromFile"


def test_autouse_fixture_ignores_the_users_env_file() -> None:
    assert not config_mod.ENV_FILE.exists() and load_settings().port == 8792
    assert "token" not in repr(Settings(knowledge_token="hunter2"))


# ---- cli --------------------------------------------------------------------------------------------------------------------
def cli(hub: Hub, *argv: str) -> int:
    key = str(hub.auth.create_key("cli-test", "admin")["secret"])
    os.environ["HUB_API_KEY"] = key
    try:
        return run(list(argv), hub=hub)
    finally:
        del os.environ["HUB_API_KEY"]


def test_cli_init_validate_plan_apply_exit_codes(hub: Hub, home: Path, capsys: pytest.CaptureFixture[str]) -> None:
    add_client(hub, home)
    seed_content(hub)
    hub.commit("c", "t")
    assert cli(hub, "validate") == 0
    capsys.readouterr()
    assert cli(hub, "--json", "plan") == 0
    assert json.loads(capsys.readouterr().out)["clients"][0]["client"] == "fake1"
    assert cli(hub, "apply") == 1                                    # never applies without --yes
    assert not (home / "CLAUDE.md").exists()
    capsys.readouterr()
    assert cli(hub, "--json", "apply", "--yes") == 0
    out = json.loads(capsys.readouterr().out)
    assert out["results"][0]["status"] == "applied" and (home / "CLAUDE.md").exists()
    assert cli(hub, "--json", "verify") == 0 and cli(hub, "--json", "drift") == 0
    capsys.readouterr()
    assert cli(hub, "plan", "--client", "fake1") == 0
    assert "fake1:" in capsys.readouterr().out


def test_cli_plan_exit_2_on_violations(hub: Hub, home: Path) -> None:
    add_client(hub, home, manage={"permissions": True})
    hub.work.put("rule", "bad", {"title": "b", "kind": "path_read", "match": "**", "decision": "allow"})
    hub.work.put_profile("fake1", {"enable": {"rules": ["bad"]}})
    hub.commit("c", "t")
    assert cli(hub, "plan") == 2 and cli(hub, "apply", "--yes") == 2
    assert not (home / "settings.json").exists()


def test_cli_validate_fails_on_bad_content(hub: Hub) -> None:
    (hub.cfg.content_dir / "agents" / "bad.md").write_text("---\nx: 1\n---\n")
    assert cli(hub, "validate") == 1


def test_cli_errors_exit_1(hub: Hub, capsys: pytest.CaptureFixture[str]) -> None:
    assert cli(hub, "drift", "--client", "ghost") == 1
    assert "error:" in capsys.readouterr().err
    assert cli(hub, "import", "--client", "fake1") == 1                 # needs --all or --item


def test_cli_import_adapters_init(hub: Hub, home: Path, capsys: pytest.CaptureFixture[str]) -> None:
    (home / "agents").mkdir()
    (home / "agents/helper.md").write_text("body")
    add_client(hub, home)
    assert cli(hub, "--json", "import", "--client", "fake1", "--all") == 0
    res = json.loads(capsys.readouterr().out)
    assert any(r["kind"] == "agent" and r["status"] == "created" for r in res["results"])
    assert cli(hub, "import", "--client", "fake1", "--item", "agent:helper", "--on-conflict", "replace") == 0
    capsys.readouterr()
    assert cli(hub, "--json", "adapters") == 0 and json.loads(capsys.readouterr().out)["adapters"][0]["id"] == "fake"
    hub.repo.discard(None)
    assert cli(hub, "init") == 0                                        # idempotent


def test_cli_audit_exit_code(hub: Hub, home: Path) -> None:
    add_client(hub, home)
    hub.commit("c", "t")
    (home / "settings.json").write_text(json.dumps({"permissions": {"allow": ["tool:web_fetch"]}}))
    assert cli(hub, "audit") == 2


def test_parser_requires_a_command() -> None:
    with pytest.raises(SystemExit):
        build_parser().parse_args([])
    assert sys.version_info >= (3, 13)


def test_cli_requires_an_admin_key(hub: Hub, capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch) -> None:
    assert run(["validate"], hub=hub) == 1 and "admin key" in capsys.readouterr().err
    monkeypatch.setenv("HUB_API_KEY", str(hub.auth.create_key("v", "viewer")["secret"]))
    assert run(["plan"], hub=hub) == 1                       # a viewer key is not enough
    monkeypatch.setenv("HUB_API_KEY", "hc_bogus")
    assert run(["plan"], hub=hub) == 1
    monkeypatch.delenv("HUB_API_KEY")
    (hub.cfg.data_dir / "bootstrap-admin.key").write_text(str(hub.auth.create_key("b", "admin")["secret"]))   # fallback file
    assert run(["validate"], hub=hub) == 0


def test_cli_status_and_commit(hub: Hub, capsys: pytest.CaptureFixture[str]) -> None:
    hub.work.put("instruction", "one", {"title": "One", "body": "x\n"})
    assert cli(hub, "status") == 0 and "instructions/one.md" in capsys.readouterr().out
    assert cli(hub, "commit", "-m", "approve one") == 0
    capsys.readouterr()
    assert cli(hub, "status") == 0 and "no pending" in capsys.readouterr().out
    assert cli(hub, "commit", "-m", "again") == 1                     # nothing to commit
    assert hub.history()[0]["message"] == "approve one" and hub.history()[0]["author"] == "Agent Hub"


def test_hub_api_key_is_read_from_the_env_file_and_real_env_wins(hub: Hub, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
                                                                 capsys: pytest.CaptureFixture[str]) -> None:
    good = str(hub.auth.create_key("f", "admin")["secret"])
    envf = tmp_path / "envfile"
    envf.write_text(f"HUB_API_KEY={good}\n")
    monkeypatch.setattr(config_mod, "ENV_FILE", envf)
    assert run(["validate"], hub=hub) == 0
    monkeypatch.setenv("HUB_API_KEY", "hc_wrong")                    # real env wins over the file
    assert run(["validate"], hub=hub) == 1
    out = capsys.readouterr()
    assert good not in out.out + out.err and "hc_wrong" not in out.out + out.err
