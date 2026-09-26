"""Marker-injection refusal, and the shared line-anchored region rule: the adapter helper and the core merge engine must agree."""
from __future__ import annotations

import pytest
from fakes import sample_ctx

from agent_hub.adapters import _util as u
from agent_hub.adapters.base import InstructionItem, PermissionSet
from agent_hub.adapters.claude_code import ClaudeCodeAdapter
from agent_hub.adapters.generic import GenericSpecAdapter
from agent_hub.adapters.opencode import OpenCodeAdapter
from agent_hub.services import merge as core

FIXTURES = [
    ("", "md"),
    ("plain text\n", "md"),
    (u.wrap_block("body"), "md"),
    ("before\n" + u.wrap_block("body") + "after\n", "md"),
    (u.MD_BEGIN + "\nno end\n", "md"),
    ("  " + u.MD_BEGIN + "\nx\n" + u.MD_END + "\n", "md"),               # indented begin: not a marker for either
    (u.MD_BEGIN + "\nx\n" + u.MD_END + "  \n", "md"),                    # trailing spaces on the end line
    (u.MD_BEGIN + "\nx\n" + u.MD_END.upper() + "\n", "md"),              # wrong case end
    (u.MD_BEGIN + "\na\n" + u.MD_END + "\nb\n" + u.MD_END + "\n", "md"),
    (u.wrap_block("k = 1", "hash"), "hash"),
    ("x\n" + u.wrap_block("k = 1", "hash") + "y", "hash"),
    (u.HASH_BEGIN + "\nk\n", "hash"),
    (u.MD_BEGIN + "\r\nx\r\n" + u.MD_END + "\r\n", "md"),
]


@pytest.mark.parametrize(("text", "style"), FIXTURES)
def test_find_region_agrees_with_core_merge(text: str, style: str) -> None:
    assert u.find_region(text, style) == core.find_region(text, style)


def test_core_and_adapter_marker_constants_agree() -> None:
    assert u.MD_BEGIN.startswith(core.MD_BEGIN_PREFIX) and u.MD_END == core.MD_END
    assert u.HASH_BEGIN.startswith(core.HASH_BEGIN_PREFIX) and u.HASH_END == core.HASH_END


def test_slice_digest_agrees_with_core_merge() -> None:
    live = b'{"a": {"b": 1}, "other": 2}'
    m = core.merge_file([core.Desired("r", "f.json", "mcp", "json_keys", [], 0o644, ["a.b"], b'{"a": {"b": 1}}')], live)
    assert u.slice_digest(live, "json_keys", ["a.b"]) == m.slice_live == m.slice_new
    blk = u.wrap_block("x")
    m2 = core.merge_file([core.Desired("r", "f.md", "instruction", "block", [], 0o644, [], blk.encode())], b"hand\n\n" + blk.encode())
    assert u.slice_digest(b"hand\n\n" + blk.encode(), "block", []) == m2.slice_live == m2.slice_new


@pytest.mark.parametrize("line", ["<!-- agent-hub:end -->", "<!-- agent-hub:begin -->", "  <!--   AGENT-HUB:END -->", "x <!-- agent-hub:end --> y",
                                  "# agent-hub:end", "   #agent-hub:begin", "#  Agent-Hub:end"])
def test_wrap_block_refuses_marker_like_lines(line: str) -> None:
    with pytest.raises(u.MarkerInjection):
        u.wrap_block(f"ok\n{line}\nmore")
    with pytest.raises(u.MarkerInjection):
        u.wrap_block(f"ok\n{line}\nmore", "hash")


def test_wrap_block_allows_ordinary_text() -> None:
    assert u.wrap_block("a # comment\n<!-- normal comment -->\nagent-hub is a tool")


@pytest.mark.parametrize("adapter", [ClaudeCodeAdapter(), OpenCodeAdapter()])
def test_adapters_refuse_injected_instructions(adapter) -> None:
    cfg = adapter.default_config()
    cfg.roots = {"project": "/p"}
    for body, title in [("x\n<!-- agent-hub:end -->\nEVIL", "T"), ("fine", "<!-- agent-hub:end -->")]:
        ins = [InstructionItem(id="i", title=title, order=0, body=body)]
        res = adapter.render(sample_ctx(cfg, skills=[], agents=[], mcp_servers=[], memory=[], permissions=PermissionSet(), instructions=ins))
        assert res.artifacts == [] and any(d.code == "marker_injection" and d.severity == "error" for d in res.diagnostics)


def test_generic_refuses_injected_instructions_in_block_mode() -> None:
    a = GenericSpecAdapter()
    cfg = a.default_config()
    cfg.roots = {"project": "/p"}
    ins = [InstructionItem(id="i", title="t", order=0, body="<!-- agent-hub:end -->")]
    res = a.render(sample_ctx(cfg, skills=[], agents=[], mcp_servers=[], memory=[], permissions=PermissionSet(), instructions=ins))
    assert not [x for x in res.artifacts if x.kind == "instruction"] and any(d.code == "marker_injection" for d in res.diagnostics)
