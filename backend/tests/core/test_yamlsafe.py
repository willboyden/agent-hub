from __future__ import annotations

import pytest

from agent_hub.domain import yamlsafe


def test_loads_plain() -> None:
    assert yamlsafe.load("a: 1\nb: [x, y]\n") == {"a": 1, "b": ["x", "y"]}


def test_billion_laughs_rejected() -> None:
    bomb = "a: &a [x, x]\nb: &b [*a, *a]\nc: &c [*b, *b]\nd: [*c, *c]\n"
    with pytest.raises(yamlsafe.YamlError):
        yamlsafe.load(bomb)


def test_anchor_alone_rejected() -> None:
    with pytest.raises(yamlsafe.YamlError):
        yamlsafe.load("a: &x 1\n")


def test_python_tags_rejected() -> None:
    with pytest.raises(yamlsafe.YamlError):
        yamlsafe.load("a: !!python/object/apply:os.system ['echo hi']\n")


def test_duplicate_keys_rejected() -> None:
    with pytest.raises(yamlsafe.YamlError):
        yamlsafe.load("a: 1\na: 2\n")


def test_too_large_and_too_deep() -> None:
    with pytest.raises(yamlsafe.YamlError):
        yamlsafe.load("a: " + "x" * 300_000)
    with pytest.raises(yamlsafe.YamlError):
        yamlsafe.load("[" * 30 + "]" * 30)


def test_too_many_nodes() -> None:
    with pytest.raises(yamlsafe.YamlError):
        yamlsafe.load("[" + ",".join(["1"] * 6000) + "]")


def test_frontmatter_roundtrip_and_errors() -> None:
    text = yamlsafe.join_frontmatter({"name": "x", "description": "a: b"}, "body\n---\nstill body\n")
    meta, body = yamlsafe.split_frontmatter(text)
    assert meta == {"name": "x", "description": "a: b"} and body == "body\n---\nstill body\n"
    for bad in ("no frontmatter", "---\nname: x\n", "---\n- a\n- b\n---\nbody"):
        with pytest.raises(yamlsafe.YamlError):
            yamlsafe.split_frontmatter(bad)
    assert yamlsafe.split_frontmatter("---\n---\nbody")[0] == {}
