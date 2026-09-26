from __future__ import annotations

import os
from pathlib import Path

import pytest

from agent_hub.domain.ids import ID_RE, valid_id
from agent_hub.domain.pathsafe import PathError, check_relpath, expand_root, resolve_under


@pytest.mark.parametrize("bad", ["", "/etc/passwd", "../x", "a/../b", "a//b", "./a", "a/.", "a\\b", "a\x00b", "C:/x",
                                 "a/b/..", "x" * 600, "a\nb"])
def test_bad_relpaths_rejected(bad: str) -> None:
    with pytest.raises(PathError):
        check_relpath(bad)


@pytest.mark.parametrize("ok", ["a", "a/b.md", "skills/x/SKILL.md", ".claude/settings.json", "a b/c"])
def test_good_relpaths(ok: str) -> None:
    assert check_relpath(ok) == ok


def test_resolve_under_inside(tmp_path: Path) -> None:
    assert resolve_under(tmp_path, "a/b/c.txt") == Path(os.path.realpath(tmp_path)) / "a/b/c.txt"


def test_relative_root_rejected() -> None:
    with pytest.raises(PathError):
        resolve_under("relative/root", "a")


def test_symlink_component_escape_rejected(tmp_path: Path) -> None:
    root, outside = tmp_path / "root", tmp_path / "outside"
    root.mkdir()
    outside.mkdir()
    (root / "link").symlink_to(outside)
    with pytest.raises(PathError):
        resolve_under(root, "link/evil.txt")


def test_symlink_leaf_rejected_even_inside_root(tmp_path: Path) -> None:
    root = tmp_path / "root"
    root.mkdir()
    (root / "real").write_text("x")
    (root / "alias").symlink_to(root / "real")
    with pytest.raises(PathError):
        resolve_under(root, "alias")


def test_dangling_symlink_rejected(tmp_path: Path) -> None:
    root = tmp_path / "root"
    root.mkdir()
    (root / "dangling").symlink_to(tmp_path / "nowhere")
    with pytest.raises(PathError):
        resolve_under(root, "dangling")


def test_root_itself_may_be_a_symlink(tmp_path: Path) -> None:
    real = tmp_path / "real"
    real.mkdir()
    link = tmp_path / "link"
    link.symlink_to(real)
    assert resolve_under(link, "f.txt") == Path(os.path.realpath(real)) / "f.txt"


def test_expand_root(tmp_path: Path) -> None:
    assert expand_root("~/x").is_absolute()
    for bad in ("", "relative", "/a/../b", "/a\x00b"):
        with pytest.raises(PathError):
            expand_root(bad)


@pytest.mark.parametrize("bad", ["../x", "A", "-x", "", "a b", "a/b", "x" * 65, ".hidden", "a\n"])
def test_ids_use_fullmatch(bad: str) -> None:
    assert not valid_id(bad)


def test_id_trailing_newline_is_not_accepted() -> None:
    # `$`-anchored match() would accept this; fullmatch must not
    assert ID_RE.fullmatch("abc\n") is None
    assert valid_id("a.b_c-d0") and valid_id("x" * 64)
