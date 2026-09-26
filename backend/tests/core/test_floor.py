from __future__ import annotations

from pathlib import Path

import pytest

from agent_hub.adapters.base import McpItem, PermissionSet, Rule
from agent_hub.config import APP_ROOT
from agent_hub.domain.errors import Unavailable
from agent_hub.services.floor import Floor, load_floor, overlaps

HOME = str(Path.home())


@pytest.fixture(scope="module")
def floor() -> Floor:
    return Floor(load_floor(APP_ROOT / "policy" / "floor.yaml"))


def r(kind: str, match: str, decision: str) -> Rule:
    return Rule(kind=kind, match=match, decision=decision)  # type: ignore[arg-type]


def test_floor_file_loads_and_is_strict(tmp_path: Path) -> None:
    f = load_floor(APP_ROOT / "policy" / "floor.yaml")
    assert f.network_default == "deny" and "~/.aws/**" in f.deny_path_read
    bad = tmp_path / "floor.yaml"
    for text in ("", "version: 1\nunknown_key: 1\n", "{{{"):
        bad.write_text(text)
        with pytest.raises(Unavailable):
            load_floor(bad)
    with pytest.raises(Unavailable):
        load_floor(tmp_path / "missing.yaml")            # fails CLOSED


@pytest.mark.parametrize("match", ["~/.ssh/id_ed25519", "~/.ssh/**", f"{HOME}/.aws/credentials", "~/**", "**", "/home/**",
                                   "~/.config/gcloud/*", "~/.gnupg/", "~/.netrc", "~/.mozilla/firefox/x/logins.json",
                                   "/proj/.env", "~/.config/google-chrome/Default/Cookies"])
def test_allow_read_of_secrets_is_a_violation(floor: Floor, match: str) -> None:
    f = floor.check_rule(r("path_read", match, "allow"), rid="x")
    assert f is not None and f.code == "floor_path_read" and f.severity == "error"


@pytest.mark.parametrize("match", ["~/src/**", "/work/repo/**", "~/.config/opencode/**", "/tmp/**"])
def test_allow_read_elsewhere_is_fine(floor: Floor, match: str) -> None:
    assert floor.check_rule(r("path_read", match, "allow")) is None


def test_stricter_content_is_fine(floor: Floor) -> None:
    assert floor.check_rule(r("path_read", "~/.ssh/**", "deny")) is None
    assert floor.check_rule(r("path_read", "~/notes/**", "ask")) is None
    assert floor.check_rule(r("tool", "web_fetch", "deny")) is None
    assert floor.check_rule(r("tool", "web_fetch", "ask")) is None


@pytest.mark.parametrize("tool", ["web_fetch", "web_search", "shell"])
def test_tool_allow_beyond_max_is_a_violation(floor: Floor, tool: str) -> None:
    f = floor.check_rule(r("tool", tool, "allow"))
    assert f is not None and f.code == "floor_tool"


@pytest.mark.parametrize("match", ["*", "**", "*.*"])
def test_catch_all_egress_allow_is_a_violation(floor: Floor, match: str) -> None:
    f = floor.check_rule(r("egress_host", match, "allow"))
    assert f is not None and f.code == "floor_egress_wildcard"


@pytest.mark.parametrize("match", ["*", "**", "  ", "?*"])
def test_shell_allow_needs_a_command_pattern(floor: Floor, match: str) -> None:
    f = floor.check_rule(r("command", match, "allow"))
    assert f is not None and f.code == "floor_command_pattern"
    assert floor.check_rule(r("command", "git status*", "allow")) is None


def test_enforce_drops_violations_and_adds_floor_rules(floor: Floor) -> None:
    res = floor.enforce([("good", r("path_read", "~/work/**", "allow")), ("evil", r("path_read", "~/.ssh/**", "allow")),
                         ("web", r("tool", "web_fetch", "ask"))])
    assert [f.item for f in res.findings] == ["evil"]
    rules = res.permissions.rules
    assert any(x.kind == "path_read" and x.match == "~/.ssh/**" and x.decision == "deny" for x in rules)
    assert not any(x.decision == "allow" and x.match == "~/.ssh/**" for x in rules)
    assert any(x.match == "~/work/**" for x in rules)
    # web_search has no content rule -> the floor default (deny) is emitted; web_fetch's content `ask` replaces it
    assert any(x.kind == "tool" and x.match == "web_search" and x.decision == "deny" for x in rules)
    assert not any(x.kind == "tool" and x.match == "web_fetch" and x.decision == "deny" for x in rules)
    assert res.permissions.network_default == "deny" and res.permissions.default_tool_decision == "ask"


def test_mcp_needs_clean_scan_and_explicit_egress(floor: Floor) -> None:
    m = McpItem(name="s", transport="stdio", command="c", sandbox_profile="srt", egress_hosts=["api.example.com"], scan_status="unscanned")
    codes = {f.code for f in floor.check_mcp(m, [])}
    assert codes == {"mcp_not_clean", "mcp_egress_not_allowed"}
    clean = m.model_copy(update={"scan_status": "clean"})
    assert {f.code for f in floor.check_mcp(clean, [r("egress_host", "api.example.com", "allow")])} == set()
    assert {f.code for f in floor.check_mcp(clean, [r("egress_host", "*.example.com", "allow")])} == set()
    both = [r("egress_host", "*.example.com", "allow"), r("egress_host", "api.example.com", "deny")]
    assert {f.code for f in floor.check_mcp(clean, both)} == {"mcp_egress_not_allowed"}
    assert {f.code for f in floor.check_mcp(clean, [r("egress_host", "other.com", "allow")])} == {"mcp_egress_not_allowed"}


def test_mcp_url_host_must_be_declared(floor: Floor) -> None:
    m = McpItem(name="s", transport="http", url="https://evil.example.net/mcp", scan_status="clean")
    assert [f.code for f in floor.check_mcp(m, [])] == ["mcp_no_egress_declared"]
    declared = m.model_copy(update={"egress_hosts": ["other.example.org"]})
    assert {f.code for f in floor.check_mcp(declared, [])} >= {"mcp_url_host_undeclared"}
    assert floor.check_mcp(m.model_copy(update={"url": "http://127.0.0.1:9000/mcp"}), []) == []


def test_live_permission_set_audit(floor: Floor) -> None:
    ps = PermissionSet(rules=[r("path_read", "~/.ssh/**", "allow"), r("tool", "web_fetch", "allow")],
                       default_tool_decision="allow", network_default="allow")
    codes = {f.code for f in floor.check_permission_set(ps)}
    assert codes == {"floor_path_read", "floor_tool", "floor_default_tool", "floor_network_default"}
    missing = floor.missing_denies(PermissionSet(rules=[r("path_read", "~/.ssh/**", "deny")]))
    reads = [f.rule for f in missing if (f.rule or "").startswith("path_read:")]
    assert "path_read:~/.aws/**" in reads and "path_read:~/.ssh/**" not in reads


def test_overlap_is_symmetric_and_conservative() -> None:
    assert overlaps("~/**", "~/.ssh/**") and overlaps("~/.ssh/**", "~/**")
    assert not overlaps("~/a/**", "~/b/**")


@pytest.mark.parametrize("match", ["~/.config/agent-hub/env", "~/.local/share/agent-hub/backups/**", "~/.local/share/agent-hub/bootstrap-admin.key",
                                   "~/.local/share/agent-knowledge/**", "~/.config/agent-knowledge/tokens.json", "/srv/x/admin.key"])
def test_hub_and_knowledge_state_is_unreadable_to_clients(floor: Floor, match: str) -> None:
    f = floor.check_rule(r("path_read", match, "allow"))
    assert f is not None and f.code == "floor_path_read"


def test_profile_allowing_hub_state_is_a_floor_violation(hub, home) -> None:  # type: ignore[no-untyped-def]
    from .conftest import add_client
    add_client(hub, home, manage={"permissions": True})
    for i, m in enumerate([str(hub.cfg.data_dir) + "/backups/**", "~/.local/share/agent-hub/**", "~/.config/agent-knowledge/**"]):
        hub.work.put("rule", f"r{i}", {"title": "x", "kind": "path_read", "match": m, "decision": "allow"})
    hub.work.put_profile("fake1", {"enable": {"rules": ["r0", "r1", "r2"]}})
    hub.commit("c", "t")
    cp = hub.plan(None).clients[0]
    assert cp.blocked and [f.code for f in cp.floor_violations] == ["floor_path_read"] * 3
    assert any(x.decision == "deny" and str(hub.cfg.data_dir) in x.match for x in hub.resolver.resolve(hub.approved()[1].catalog(), "fake1").permissions.rules)


def test_knowledge_admin_key_file_is_unreadable_to_clients(floor: Floor) -> None:
    for m in ("~/.local/share/agent-knowledge/admin.key", "~/.local/share/agent-knowledge/**", "/anywhere/admin.key"):
        f = floor.check_rule(r("path_read", m, "allow"))
        assert f is not None and f.code == "floor_path_read"
    assert any(x.match == "~/.local/share/agent-knowledge/**" for x in floor.floor_rules() if x.kind == "path_read")
