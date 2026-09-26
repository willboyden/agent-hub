from __future__ import annotations

from pathlib import Path

import pytest

from agent_hub.domain.pathsafe import PathError
from agent_hub.services.fs import LiveFiles, RealFS, RealRunner
from agent_hub.services.merge import Desired, del_dotted, find_region, get_dotted, merge_file, set_dotted


def D(merge: str, content: bytes | None, keys: list[str] | None = None, path: str = "f") -> Desired:
    return Desired("home", path, "mcp", merge, [], 0o644, keys or [], content)


def test_realfs_is_limited_to_roots(tmp_path: Path) -> None:
    root, outside = tmp_path / "root", tmp_path / "outside"
    root.mkdir()
    outside.mkdir()
    (root / "a.txt").write_text("a")
    (outside / "s.txt").write_text("secret")
    (root / "link").symlink_to(outside)
    fs = RealFS([root])
    assert fs.read_bytes(str(root / "a.txt")) == b"a" and fs.exists(str(root / "a.txt")) and fs.is_dir(str(root))
    assert fs.listdir(str(root)) == ["a.txt", "link"]
    for bad in (str(outside / "s.txt"), str(root / "link" / "s.txt"), str(root / ".." / "outside" / "s.txt"), "/etc/hostname"):
        with pytest.raises(PermissionError):
            fs.read_bytes(bad)
        assert fs.exists(bad) is False and fs.is_dir(bad) is False
    with pytest.raises(PermissionError):
        fs.listdir("/etc")
    with pytest.raises(PermissionError):
        fs.read_bytes(str(root / "a\x00b"))


def test_realrunner_no_shell_timeout_and_limits() -> None:
    r = RealRunner(max_timeout=1.0)
    rc, out, _ = r.run(["echo", "$HOME; echo pwned"], timeout=5)
    assert rc == 0 and out.strip() == "$HOME; echo pwned"                    # no shell expansion
    assert r.run(["definitely-not-a-command-xyz"], timeout=5)[0] == 127
    assert r.run([], timeout=5)[0] == 127 and r.run(["a\x00b"], timeout=5)[0] == 127
    assert r.run(["sleep", "5"], timeout=30)[0] == 124                       # capped at max_timeout


def test_livefiles(tmp_path: Path) -> None:
    lf = LiveFiles({"home": str(tmp_path)}, max_bytes=10)
    assert lf.read("home", "missing") is None
    (tmp_path / "f").write_text("hi")
    assert lf.read("home", "f") == b"hi"
    (tmp_path / "big").write_text("x" * 100)
    (tmp_path / "d").mkdir()
    for bad in ("big", "d"):
        with pytest.raises(OSError):
            lf.read("home", bad)
    with pytest.raises(PathError):
        lf.read("nope", "f")


def test_dotted_helpers() -> None:
    doc: dict = {}  # type: ignore[type-arg]
    set_dotted(doc, "a.b.c", 1)
    assert get_dotted(doc, "a.b.c") == (True, 1) and get_dotted(doc, "a.x") == (False, None)
    set_dotted(doc, "a.keep", 2)
    del_dotted(doc, "a.b.c")
    assert doc == {"a": {"keep": 2}}                                         # emptied parents pruned, siblings kept
    del_dotted(doc, "no.such.key")
    del_dotted(doc, "a.keep")
    assert doc == {}


def test_yaml_keys_merge_preserves_foreign() -> None:
    live = b"theme: dark\nagent:\n  disabled_toolsets: [old]\n  other: 1\n"
    m = merge_file([D("yaml_keys", b"agent:\n  disabled_toolsets: [browser]\n", ["agent.disabled_toolsets"])], live)
    assert m.new is not None and b"theme: dark" in m.new and b"other: 1" in m.new and b"browser" in m.new and b"old" not in m.new
    assert m.slice_live != m.slice_new
    again = merge_file([D("yaml_keys", b"agent:\n  disabled_toolsets: [browser]\n", ["agent.disabled_toolsets"])], m.new)
    assert again.slice_live == again.slice_new
    assert merge_file([D("yaml_keys", b"x: &a 1\ny: *a\n", ["x"])], b"").unparseable          # aliases refused
    assert merge_file([D("yaml_keys", b"- list\n", ["x"])], b"").unparseable


def test_json_merge_edge_cases() -> None:
    assert merge_file([D("json_keys", b"{}", ["a"])], None).new is None                          # nothing to write
    assert merge_file([D("json_keys", b"[1]", ["a"])], None).unparseable
    assert merge_file([D("json_keys", b'{"a": 1}', ["a"])], b"[1]").unparseable
    assert merge_file([D("json_keys", b'{"a": 1}', ["a"])], b"[1]", adopt_broken=True).new is not None
    m = merge_file([D("json_keys", None, ["a"])], b'{"a": 1, "b": 2}')
    assert m.new is not None and b'"b"' in m.new and b'"a"' not in m.new and not m.empty_after
    assert merge_file([D("json_keys", None, ["a"])], b'{"a": 1}').empty_after
    assert merge_file([D("nonsense", b"x")], b"").unparseable


def test_hash_style_block_and_removal() -> None:
    blk = b"# agent-hub:begin (managed)\nkey = 1\n# agent-hub:end\n"
    live = b"[core]\nx = 1\n"
    m = merge_file([D("block", blk, path="config.toml")], live)
    assert m.new == live + b"\n" + blk and m.block_style == "hash"
    removed = merge_file([D("block", None, path="config.toml")], m.new)
    assert removed.new == live and removed.slice_new is None
    only = merge_file([D("block", None, path="config.toml")], blk)
    assert only.empty_after and only.new == b""
    assert merge_file([D("block", None, path="x.md")], None).new is None
    text = "a\n<!-- agent-hub:begin x -->\nbody\n"
    assert find_region(text, "md") == "broken" and find_region("plain\n", "md") is None
    adopt = merge_file([D("block", b"<!-- agent-hub:begin x -->\nnew\n<!-- agent-hub:end -->\n", path="x.md")], text.encode(),
                       adopt_broken=True)
    assert adopt.new is not None and adopt.new.startswith(b"a\n\n<!-- agent-hub:begin x -->\nnew")


def test_realfs_size_uses_stat_with_the_same_boundaries(tmp_path: Path) -> None:
    root, outside = tmp_path / "root", tmp_path / "outside"
    root.mkdir()
    outside.mkdir()
    (root / "a").write_bytes(b"12345")
    (outside / "b").write_bytes(b"x")
    fs = RealFS([root])
    assert fs.size(str(root / "a")) == 5
    with pytest.raises(PermissionError):
        fs.size(str(outside / "b"))
    with pytest.raises(FileNotFoundError):
        fs.size(str(root / "missing"))
