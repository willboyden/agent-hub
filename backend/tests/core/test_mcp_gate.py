"""H2: the MCP gate (attested digest, sandbox, egress) and I1 (slice verify after a real merge)."""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from agent_hub.adapters.base import McpItem
from agent_hub.config import APP_ROOT
from agent_hub.services.floor import Floor, load_floor
from agent_hub.services.hub import Hub

from .conftest import add_client

BASE = {"transport": "stdio", "command": "srv", "args": ["--x"], "sandbox_profile": "srt", "scan_status": "clean"}


def commit(hub: Hub) -> None:
    hub.commit("c", "t")


@pytest.fixture
def mcp_client(hub: Hub, home: Path) -> Hub:
    add_client(hub, home, manage={"mcp": True})
    hub.work.put("mcp", "srv", BASE)
    hub.work.put_profile("fake1", {"enable": {"mcp": ["srv"]}})
    commit(hub)
    return hub


def codes(hub: Hub) -> set[str]:
    return {f.code for f in hub.plan(None).clients[0].floor_violations}


def test_clean_is_bound_to_a_digest_of_the_scanned_fields(mcp_client: Hub) -> None:
    hub = mcp_client
    assert codes(hub) == set() and hub.work.load("mcp", "srv").meta.scan_digest  # type: ignore[union-attr]
    # an edit that changes command/args/url/pinned_ref/transport invalidates `clean`, even when the request resends `clean`
    for change in ({"command": "evil"}, {"args": ["--y"]}, {"pinned_ref": "abc123"}):
        hub.work.put("mcp", "srv", {**BASE, **change})
        commit(hub)
        assert codes(hub) == {"mcp_not_clean"}, change
        assert hub.effective("fake1")["mcp"] == []
        hub.work.put("mcp", "srv", BASE)                                   # restoring the scanned fields restores `clean`
        commit(hub)
        assert codes(hub) == set()
    # changing an unrelated field keeps it; a file edit that bypasses the API cannot forge the digest
    hub.work.put("mcp", "srv", {**BASE, "notes": "hello"})
    commit(hub)
    assert codes(hub) == set()
    p = hub.cfg.content_dir / "mcp" / "srv.yaml"
    p.write_text(p.read_text().replace("command: srv", "command: rogue"))
    commit(hub)
    assert codes(hub) == {"mcp_not_clean"}


def test_reattesting_from_unscanned_binds_the_current_fields(mcp_client: Hub) -> None:
    hub = mcp_client
    hub.work.put("mcp", "srv", {**BASE, "command": "new", "scan_status": "unscanned"})
    hub.work.put("mcp", "srv", {**BASE, "command": "new"})                 # operator attests to what is there now
    commit(hub)
    assert codes(hub) == set()


def test_sandbox_and_egress_declarations_are_required(hub: Hub) -> None:
    floor = Floor(load_floor(APP_ROOT / "policy" / "floor.yaml"))
    stdio = McpItem(name="a", transport="stdio", command="c", scan_status="clean")
    assert {f.code for f in floor.check_mcp(stdio, [])} == {"mcp_no_sandbox"}
    http = McpItem(name="b", transport="http", url="https://x.example.com/mcp", scan_status="clean")
    assert {f.code for f in floor.check_mcp(http, [])} == {"mcp_no_egress_declared"}
    assert floor.check_mcp(http.model_copy(update={"url": "http://127.0.0.1:9/mcp"}), []) == []       # loopback needs none


def test_mcp_allow_rules_wildcards_always_error_named_only_for_enabled_clean_servers(mcp_client: Hub) -> None:
    hub = mcp_client
    hub.work.put("mcp", "shady", {**BASE, "command": "shady", "scan_status": "unscanned"})
    rules = {"wild": ("tool", "mcp__*"), "toolmcp": ("tool", "mcp"), "glob": ("mcp_server", "sh*"),
             "shady-named": ("mcp_server", "shady"), "shady-tool": ("tool", "mcp__shady__run"),
             "absent": ("tool", "mcp__notenabled"), "okname": ("mcp_server", "srv"), "oktool": ("tool", "mcp__srv__search")}
    for rid, (kind, match) in rules.items():
        hub.work.put("rule", rid, {"title": rid, "kind": kind, "match": match, "decision": "allow"})
    hub.work.put_profile("fake1", {"enable": {"mcp": ["srv", "shady"], "rules": list(rules)}})
    commit(hub)
    bad = {f.item for f in hub.plan(None).clients[0].floor_violations if f.code == "floor_mcp_rule"}
    assert bad == {"wild", "toolmcp", "glob", "shady-named", "shady-tool", "absent"}      # okname/oktool (clean + enabled) pass
    # a wildcard is an error even when EVERY enabled server is clean (the old behaviour is gone)
    hub.work.put_profile("fake1", {"enable": {"mcp": ["srv"], "rules": ["wild", "okname"]}})
    commit(hub)
    assert {f.item for f in hub.plan(None).clients[0].floor_violations if f.code == "floor_mcp_rule"} == {"wild"}


def test_merged_files_verify_on_the_managed_slice(hub: Hub, home: Path) -> None:
    """I1: a correct json/block merge verifies; hand-editing a managed key makes verify fail."""
    add_client(hub, home, manage={"mcp": True, "permissions": True})
    hub.work.put("mcp", "srv", BASE)
    hub.work.put("instruction", "style", {"title": "S", "order": 1, "body": "Be concise.\n"})
    hub.work.put_profile("fake1", {"enable": {"mcp": ["srv"], "instructions": ["style"]}})
    (home / ".mcp.json").write_text(json.dumps({"other": 1}))
    (home / "CLAUDE.md").write_text("mine\n")
    commit(hub)
    plan = hub.plan(None)
    res = hub.apply(plan.id, True, [], "t")["results"][0]
    assert res["status"] == "applied" and res["verify_ok"] is True, res["checks"]
    assert hub.verify("fake1")["ok"] is True
    doc = json.loads((home / ".mcp.json").read_text())
    doc["mcpServers"]["srv"]["command"] = "tampered"
    (home / ".mcp.json").write_text(json.dumps(doc))
    assert hub.verify("fake1")["ok"] is False
    doc["mcpServers"]["srv"]["command"] = "srv"
    doc["other"] = 2                                                       # an UNMANAGED edit does not fail verify
    (home / ".mcp.json").write_text(json.dumps(doc))
    (home / "CLAUDE.md").write_text("mine, edited\n" + (home / "CLAUDE.md").read_text().split("mine\n", 1)[1])
    assert hub.verify("fake1")["ok"] is True
