"""Re-audit items: N2 skill tools, N4 sensitive targets, N6 launcher probes, HMAC scan attestation."""
from __future__ import annotations

import os
import stat
from pathlib import Path

import pytest

from agent_hub.adapters.base import Artifact, RenderContext, RenderResult, Rule
from agent_hub.cli import run
from agent_hub.config import APP_ROOT
from agent_hub.services.attest import Attestor
from agent_hub.services.floor import Floor, load_floor
from agent_hub.services.hub import Hub
from agent_hub.services.targets import TargetGuard

from .conftest import add_client
from .fakes import FakeAdapter


@pytest.fixture(scope="module")
def floor() -> Floor:
    return Floor(load_floor(APP_ROOT / "policy" / "floor.yaml"))


# ---- N2 -------------------------------------------------------------------------------------------------------------------
@pytest.mark.parametrize("raw,code", [
    ("Read Bash", "skill_unrestricted_tools"), ("Read, Bash", "skill_unrestricted_tools"), ("Bash(*)", "skill_unrestricted_tools"),
    ("Bash(*:*)", "skill_unrestricted_tools"), ("Bash(:*)", "skill_unrestricted_tools"), ("Read Grep *", "skill_unrestricted_tools"),
    ("Grep mcp__*", "skill_unrestricted_tools"), ("Read mcp__ghost__run", "skill_unscanned_mcp_tool"),
    ("Read Bash(git status:*) Grep", "skill_preapproves_tools"), ("Read, Grep", "skill_preapproves_tools"),
    ("Read mcp__srv__search", "skill_preapproves_tools"),
])
def test_allowed_tools_split_on_commas_and_whitespace(floor: Floor, raw: str, code: str) -> None:
    fs = floor.check_skill("s", {"allowed-tools": raw}, {"srv"})
    assert [f.code for f in fs] == [code], raw


def test_specific_mcp_tools_need_an_enabled_clean_server(floor: Floor) -> None:
    assert floor.check_skill("s", {"allowed-tools": "mcp__srv__x"}, {"srv"})[0].severity == "warn"
    assert floor.check_skill("s", {"allowed-tools": "mcp__srv__x"}, set())[0].code == "skill_unscanned_mcp_tool"
    assert floor.check_skill("s", {"allowed-tools": ["Read"]}, None)[0].code == "skill_preapproves_tools"


# ---- N6 -------------------------------------------------------------------------------------------------------------------
@pytest.mark.parametrize("match", ["git *", "node *", "npx *", "uv run *", "uvx *", "find *", "xargs *", "perl *", "awk *", "env *",
                                   "nohup *", "timeout *", "ssh *", "curl *", "wget *"])
def test_general_purpose_launchers_warn(floor: Floor, match: str) -> None:
    f = floor.check_rule(Rule(kind="command", match=match, decision="allow"))
    assert f is not None and f.code == "floor_command_pattern" and f.severity == "warn", match
    res = floor.enforce([("r", Rule(kind="command", match=match, decision="allow"))])
    assert any(r.match == match for r in res.permissions.rules)          # a warning keeps the rule


def test_bare_wildcards_are_still_errors_and_specific_patterns_pass(floor: Floor) -> None:
    for m in ("*", "**", "* *"):
        f = floor.check_rule(Rule(kind="command", match=m, decision="allow"))
        assert f is not None and f.severity == "error"
    for m in ("git status*", "uv run pytest tests/*", "curl https://api.example.com/health"):
        assert floor.check_rule(Rule(kind="command", match=m, decision="allow")) is None or \
            floor.check_rule(Rule(kind="command", match=m, decision="allow")).severity == "warn"  # type: ignore[union-attr]
    res = floor.enforce([("bare", Rule(kind="command", match="*", decision="allow"))])
    assert not any(r.match == "*" for r in res.permissions.rules)


@pytest.mark.parametrize("host", ["*.io", "*.org", "*.dev", "*.com", "*.net", "*.*"])
def test_tld_wildcard_egress_is_refused(floor: Floor, host: str) -> None:
    f = floor.check_rule(Rule(kind="egress_host", match=host, decision="allow"))
    assert f is not None and f.code == "floor_egress_wildcard" and f.severity == "error"


# ---- N4 -------------------------------------------------------------------------------------------------------------------
def test_whole_app_tree_is_protected_except_out(hub: Hub, tmp_path: Path) -> None:
    g: TargetGuard = hub.guard
    assert g.root_problem(APP_ROOT / "docs") and g.root_problem(APP_ROOT / "frontend")
    assert g.root_problem(APP_ROOT / "out" / "example") is None
    assert g.root_problem(APP_ROOT.parent) is None                               # a broad project root merely CONTAINS the app
    assert g.target_problem(APP_ROOT / "docs" / "x.md", "hub/docs/x.md")
    assert g.target_problem(APP_ROOT / "out" / "example" / "x", "x") is None


@pytest.mark.parametrize("adapter,manage,kind,rel,ok", [
    ("claude-code", {"permissions": True}, "rule", ".claude/settings.json", True),
    ("claude-code", {"permissions": False}, "rule", ".claude/settings.json", False),
    ("opencode", {"permissions": True}, "rule", ".claude/settings.json", False),
    ("claude-code", {"permissions": True}, "rule", ".claude/settings.local.json", False),
    ("claude-code", {"instructions": True}, "instruction", "CLAUDE.md", True),
    ("claude-code", {"instructions": True}, "instruction", "AGENTS.md", False),
    ("hermes-agent", {"instructions": True}, "instruction", "AGENTS.md", False),
    ("opencode", {"instructions": True}, "instruction", "AGENTS.md", True),
    ("opencode", {"instructions": False}, "instruction", "AGENTS.md", False),
    ("generic", {"instructions": True}, "instruction", "sub/AGENTS.md", True),
    ("generic", {"skills": True}, "skill", "security/x.sh", False),
    ("claude-code", {"skills": True}, "skill", ".git/hooks/x", False),
])
def test_sensitive_targets(adapter: str, manage: dict[str, bool], kind: str, rel: str, ok: bool) -> None:
    assert (TargetGuard.sensitive_target(adapter, manage, kind, rel) is None) is ok


def test_sensitive_target_blocks_a_managed_plan_but_not_an_advisory_one(hub: Hub, home: Path) -> None:
    class Sneaky(FakeAdapter):
        id = "sneaky2"

        def render(self, ctx: RenderContext) -> RenderResult:
            return RenderResult(artifacts=[Artifact(root="home", path=p, content=b"{}", kind="rule") for p in
                                           (".claude/settings.json", "security/policy.json", "notes.md")])

    hub.registry.register(Sneaky())
    hub.put_client("sn", {"adapter": "sneaky2", "display_name": "S", "roots": {"home": str(home)},
                          "manage": {"permissions": True}}, create=True)
    hub.put_client("sn2", {"adapter": "sneaky2", "display_name": "S", "roots": {"home": str(home)}}, create=True)   # advisory
    hub.commit("c", "t")
    managed, advisory = hub.plan(["sn", "sn2"]).clients
    assert managed.blocked and len([d for d in managed.diagnostics if d["code"] == "unsafe_path"]) == 2
    assert not advisory.blocked
    assert not (home / "security").exists() and not (home / ".claude").exists()


# ---- HMAC attestation -------------------------------------------------------------------------------------------------------
BASE = {"transport": "stdio", "command": "srv", "sandbox_profile": "srt", "scan_status": "clean"}


def test_attestation_key_is_private_and_created_once(tmp_path: Path) -> None:
    a = Attestor(tmp_path / "state" / "attest.key")
    d1 = a.digest("c", ["a"], None, None, "stdio")
    f = tmp_path / "state" / "attest.key"
    assert stat.S_IMODE(f.stat().st_mode) == 0o600 and len(f.read_text().strip()) == 64
    assert Attestor(f).digest("c", ["a"], None, None, "stdio") == d1                  # same key on the next start
    assert a.digest("c", ["b"], None, None, "stdio") != d1
    f.chmod(0o644)
    from agent_hub.domain.errors import ProblemError
    with pytest.raises(ProblemError):
        Attestor(f).digest("c", [], None, None, "stdio")


def test_forged_or_legacy_digests_do_not_make_a_server_clean(hub: Hub, home: Path) -> None:
    import hashlib
    import json
    add_client(hub, home, manage={"mcp": True})
    hub.work.put("mcp", "srv", BASE)
    hub.work.put_profile("fake1", {"enable": {"mcp": ["srv"]}})
    hub.commit("c", "t")
    assert not hub.plan(None).clients[0].floor_violations
    assert hub.effective("fake1")["mcp"][0]["scan_status"] == "clean"
    # an unkeyed sha256 of the same fields (what the previous version stored, or what an attacker can compute) is worthless
    blob = json.dumps(["srv", [], None, None, "stdio"], sort_keys=True, separators=(",", ":")).encode()
    p = hub.cfg.content_dir / "mcp" / "srv.yaml"
    import re
    text = re.sub(r"scan_digest: \S+", f"scan_digest: {hashlib.sha256(blob).hexdigest()}", p.read_text())
    p.write_text(text)
    hub.commit("forge", "t")
    v = hub.plan(None).clients[0].floor_violations
    assert [f.code for f in v] == ["mcp_not_clean"] and "hubctl attest-mcp" in v[0].message
    # editing the file to `clean` with NO digest is equally worthless
    p.write_text(re.sub(r"scan_digest: \S+\n", "", p.read_text()))
    hub.commit("nodigest", "t")
    assert [f.code for f in hub.plan(None).clients[0].floor_violations] == ["mcp_not_clean"]
    assert (hub.cfg.data_dir / "attest.key").read_text().strip() not in p.read_text()


def test_attest_mcp_cli_recomputes_only_after_confirmation(hub: Hub, home: Path, capsys: pytest.CaptureFixture[str],
                                                          monkeypatch: pytest.MonkeyPatch) -> None:
    add_client(hub, home, manage={"mcp": True})
    hub.work.put("mcp", "srv", {**BASE, "scan_status": "unscanned"})
    hub.work.put_profile("fake1", {"enable": {"mcp": ["srv"]}})
    hub.commit("c", "t")
    monkeypatch.setenv("HUB_API_KEY", str(hub.auth.create_key("k", "admin")["secret"]))
    assert run(["attest-mcp", "srv"], hub=hub) == 1
    err = capsys.readouterr().err
    assert "does NOT run it" in err and "scanner" in err
    assert hub.work.load("mcp", "srv").meta.scan_status == "unscanned"  # type: ignore[union-attr]
    assert run(["attest-mcp", "srv", "--yes"], hub=hub) == 0
    out = capsys.readouterr().out
    assert "attested srv" in out and (hub.cfg.data_dir / "attest.key").read_text().strip() not in out
    hub.commit("attest", "t")
    assert not hub.plan(None).clients[0].floor_violations
    # editing the scanned fields afterwards invalidates it again
    hub.work.put("mcp", "srv", {**BASE, "command": "other"})
    hub.commit("edit", "t")
    assert [f.code for f in hub.plan(None).clients[0].floor_violations] == ["mcp_not_clean"]
    assert run(["attest-mcp", "ghost", "--yes"], hub=hub) == 1
    assert os.path.exists(hub.cfg.data_dir / "attest.key")
