from __future__ import annotations

import pytest
from fakes import FakeFS

from agent_hub.adapters import _util as u
from agent_hub.adapters.base import Check, ClientConfig


@pytest.mark.parametrize("bad", ["../x", "a/../b", "/abs", "a\\b", "a\x00b", "", ".", "a//b", "C:\\x", "a/\x07"])
def test_safe_relpath_rejects(bad: str) -> None:
    with pytest.raises(u.UnsafePath):
        u.safe_relpath(bad)


def test_safe_relpath_joins() -> None:
    assert u.safe_relpath(".claude/skills", "x", "sub/f.md") == ".claude/skills/x/sub/f.md"


def test_names_use_fullmatch() -> None:
    assert u.valid_name("ok-name.1") and not u.valid_name("ok\n") and not u.valid_name("Bad") and not u.valid_name("a" * 65)


def test_frontmatter_round_trip_and_hostile_values() -> None:
    fields = {"name": "n", "description": "a: b # c '\" --- {x}", "tools": "Read, Bash"}
    text = u.render_frontmatter(fields, "body\n\n---\nnot frontmatter")
    fm, body = u.parse_frontmatter(text)
    assert fm == fields and body.startswith("body")


def test_frontmatter_parse_is_tolerant() -> None:
    assert u.parse_frontmatter("no fm") == ({}, "no fm")
    assert u.parse_frontmatter("---\n: : [\n---\nx")[0] == {}
    assert u.parse_frontmatter("---\n- a\n---\nx")[0] == {}
    assert u.parse_frontmatter("---\nunterminated: 1\n")[0] == {}


def test_oneline_kills_multiline_scalars() -> None:
    out: list = []
    assert u.oneline("a\n---\nb", "i", out) == "a --- b" and out[0].code == "description_normalized"
    assert u.oneline("plain", "i", []) == "plain"


def test_block_helpers() -> None:
    blk = u.wrap_block("hello")
    assert u.extract_block("x\n" + blk + "y") == blk
    assert u.extract_block("nothing") is None and u.extract_block(u.MD_BEGIN + "\nunterminated") is None
    assert u.replace_block("", blk) == blk
    hb = u.wrap_block("k = 1", "hash")
    assert hb.startswith(u.HASH_BEGIN) and u.extract_block(hb, "hash") == hb


def test_dotted_helpers_and_merge() -> None:
    d: dict = {}
    u.set_dotted(d, "a.b.c", 1)
    assert d == {"a": {"b": {"c": 1}}} and u.get_dotted(d, "a.b.c") == 1 and u.get_dotted(d, "a.x") is None
    u.del_dotted(d, "a.b.c")
    u.del_dotted(d, "zz.y")
    assert d == {"a": {"b": {}}}
    assert u.merge_keys({"k": 1, "p": {"a": 1, "b": 2}}, {"p": {"a": 9}}, ["p.a", "p.b"]) == {"k": 1, "p": {"a": 9}}


def test_json_and_yaml_dump_are_sorted_and_stable() -> None:
    assert u.dump_json({"b": 1, "a": [2, 1]}) == b'{\n  "a": [\n    2,\n    1\n  ],\n  "b": 1\n}\n'
    assert u.dump_yaml({"b": 1, "a": 2}) == b"a: 2\nb: 1\n"


def test_effective_caps() -> None:
    assert u.effective_caps(["shell", "read", "write"], True) == ["read", "shell"]
    assert u.unknown_caps(["read", "teleport"]) == ["teleport"]


def test_read_tree_bounded_and_skips_caches() -> None:
    fs = FakeFS({"/b/a.txt": b"1", "/b/__pycache__/x.pyc": b"x", "/b/d/e.txt": b"2", "/b/big": b"x" * (u.MAX_FILE_BYTES + 1)})
    assert u.read_tree(fs, "/b") == {"a.txt": b"1", "d/e.txt": b"2"}
    assert len(u.read_tree(fs, "/b", limit=1)) == 1
    assert u.read_tree(fs, "/nope") == {}


def test_verify_expected_unsafe_and_unconfigured_root() -> None:
    from agent_hub.adapters.base import Expected
    cfg = ClientConfig(id="c", adapter="x", display_name="c", roots={"r": "/r"})
    exp = [Expected(root="r", path="../x", sha256="0", kind="skill"), Expected(root="zz", path="a", sha256="0", kind="skill")]
    checks: list[Check] = u.verify_expected(cfg, exp, FakeFS())
    assert not any(c.ok for c in checks) and "unsafe" in checks[0].detail or "unsafe" in checks[1].detail


def test_verify_expected_key_merged_file_compares_only_the_slice() -> None:
    from agent_hub.adapters.base import Expected
    cfg = ClientConfig(id="c", adapter="x", display_name="c", roots={"r": "/r"})
    slice_doc = {"mcpServers": {"a": {"command": "x"}}}
    good = u.slice_digest(b'{"mcpServers": {"a": {"command": "x"}}, "other": 1}', "json_keys", ["mcpServers"])
    exp = lambda sha: [Expected(root="r", path=".mcp.json", sha256=sha, kind="mcp", merge="json_keys", managed_keys=["mcpServers"])]  # noqa: E731
    fs = FakeFS({"/r/.mcp.json": b'{"other": 2, "mcpServers": {"a": {"command": "x"}}}'})    # unmanaged key changed
    assert u.verify_expected(cfg, exp(good), fs)[0].ok
    fs2 = FakeFS({"/r/.mcp.json": b'{"mcpServers": {"a": {"command": "EVIL"}}}'})
    c = u.verify_expected(cfg, exp(good), fs2)[0]
    assert c.ok is False and "slice differs" in c.detail
    assert slice_doc and u.slice_digest(b"not json", "json_keys", ["mcpServers"]) is None


def test_verify_yaml_keys_slice() -> None:
    from agent_hub.adapters.base import Expected
    cfg = ClientConfig(id="c", adapter="x", display_name="c", roots={"r": "/r"})
    want = u.slice_digest(b"agent:\n  disabled_toolsets: [a]\nz: 1\n", "yaml_keys", ["agent.disabled_toolsets"])
    fs = FakeFS({"/r/c.yaml": b"agent:\n  disabled_toolsets: [a]\nz: 2\nnew: 1\n"})
    e = Expected(root="r", path="c.yaml", sha256=want or "", kind="rule", merge="yaml_keys", managed_keys=["agent.disabled_toolsets"])
    assert u.verify_expected(cfg, [e], fs)[0].ok
