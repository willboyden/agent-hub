from __future__ import annotations

from pathlib import Path

import pytest

from agent_hub.domain.errors import BadRequest, Conflict, NotFound, Unprocessable
from agent_hub.services.hub import Hub

from .conftest import add_client, seed_content


def test_seeded_repo_layout(hub: Hub) -> None:
    root = hub.cfg.content_dir
    assert (root / ".git").is_dir() and (root / "hub.yaml").is_file() and (root / "endpoints.yaml").is_file()
    assert hub.repo.head() is not None and hub.repo.is_clean()
    assert hub.work.endpoints()["router"].startswith("http://127.0.0.1")


def test_skill_crud_with_extra_files_and_sidecar(hub: Hub) -> None:
    w = hub.work
    it = w.put("skill", "alpha", {"description": "A", "body": "hello\n", "files": {"ref/a.md": "one", "b.txt": "two"},
                                  "tags": ["x"], "notes": "n", "source": "manual"})
    assert it.valid and set(it.files) == {"SKILL.md", "ref/a.md", "b.txt"}
    assert it.sidecar and it.sidecar.tags == ["x"] and it.sidecar.notes == "n"
    # SKILL.md stays standard-clean: bookkeeping lives in hub.yaml only
    assert "tags" not in it.files["SKILL.md"].decode() and "notes" not in it.files["SKILL.md"].decode()
    # files=None leaves extras alone; files={...} replaces the set
    w.put("skill", "alpha", {"description": "A2", "body": "hello\n"})
    assert set(w.load("skill", "alpha").files) == {"SKILL.md", "ref/a.md", "b.txt"}
    it = w.put("skill", "alpha", {"description": "A2", "body": "hello\n", "files": {"only.txt": "x"}})
    assert set(it.files) == {"SKILL.md", "only.txt"} and it.sidecar and it.sidecar.tags == ["x"]
    w.delete("skill", "alpha")
    with pytest.raises(NotFound):
        w.load("skill", "alpha")


def test_skill_extra_file_traversal_and_reserved_rejected(hub: Hub) -> None:
    for bad in ("../evil", "/abs", "a/../../b", "SKILL.md", "hub.yaml", ".git/config", "a\\b"):
        with pytest.raises(Unprocessable):
            hub.work.put("skill", "bad", {"description": "d", "body": "", "files": {bad: "x"}})
    assert not (hub.cfg.content_dir.parent / "evil").exists()


def test_skill_duplicate(hub: Hub) -> None:
    hub.work.put("skill", "alpha", {"description": "A", "body": "b\n", "files": {"x.txt": "1"}})
    dup = hub.work.duplicate_skill("alpha", "alpha-copy")
    assert dup.valid and dup.meta.name == "alpha-copy" and dup.files["x.txt"] == b"1"  # type: ignore[union-attr]
    with pytest.raises(Conflict):
        hub.work.duplicate_skill("alpha", "alpha-copy")


@pytest.mark.parametrize("kind,payload", [
    ("agent", {"description": "d", "capabilities": ["read", "shell"], "model_tier": "fast", "body": "b"}),
    ("instruction", {"title": "T", "order": 5, "applies_to": ["fake1"], "body": "b"}),
    ("mcp", {"transport": "stdio", "command": "srv", "args": ["--x"], "env_names": ["API_TOKEN"], "egress_hosts": ["api.example.com"]}),
    ("rule", {"title": "R", "kind": "path_read", "match": "/data/**", "decision": "ask", "reason": "why"}),
    ("memory", {"type": "user", "title": "M", "body": "text"}),
])
def test_every_kind_roundtrips(hub: Hub, kind: str, payload: dict) -> None:  # type: ignore[type-arg]
    it = hub.work.put(kind, "thing", payload)
    assert it.valid, it.issues
    again = hub.work.load(kind, "thing")
    assert again.meta == it.meta and again.body == it.body
    assert "thing" in hub.work.names(kind)
    hub.work.delete(kind, "thing")
    assert "thing" not in hub.work.names(kind)


@pytest.mark.parametrize("kind,payload", [
    ("agent", {"description": "d", "capabilities": ["teleport"]}),
    ("agent", {"description": "d", "surprise": 1}),
    ("instruction", {"title": "T", "order": "x"}),
    ("mcp", {"transport": "stdio"}),                                           # stdio needs command
    ("mcp", {"transport": "http", "url": "ftp://x"}),
    ("mcp", {"transport": "stdio", "command": "c", "env_names": ["TOKEN=hunter2"]}),   # values never live in the hub
    ("mcp", {"transport": "stdio", "command": "c", "egress_hosts": ["bad host!"]}),
    ("rule", {"title": "R", "kind": "nonsense", "match": "x", "decision": "allow"}),
    ("rule", {"title": "R", "kind": "tool", "match": "x", "decision": "maybe"}),
    ("memory", {"type": "secret", "title": "M"}),
])
def test_unknown_keys_and_bad_values_are_errors(hub: Hub, kind: str, payload: dict) -> None:  # type: ignore[type-arg]
    with pytest.raises(Unprocessable):
        hub.work.put(kind, "thing", payload)
    assert "thing" not in hub.work.names(kind)


@pytest.mark.parametrize("name", ["../x", "A", "a b", "", "x" * 65, ".x", "a/b", "a\n"])
def test_hostile_ids_rejected(hub: Hub, name: str) -> None:
    with pytest.raises((BadRequest, NotFound)):
        hub.work.put("agent", name, {"description": "d", "capabilities": []})
    with pytest.raises((BadRequest, NotFound)):
        hub.work.load("agent", name)


def test_body_id_must_match_url(hub: Hub) -> None:
    with pytest.raises(Unprocessable):
        hub.work.put("instruction", "one", {"id": "two", "title": "T"})


def test_invalid_file_on_disk_is_reported_not_fatal(hub: Hub) -> None:
    (hub.cfg.content_dir / "agents" / "broken.md").write_text("---\nname: broken\nbogus: 1\n---\nbody")
    (hub.cfg.content_dir / "agents" / "mismatch.md").write_text("---\nname: other\ndescription: d\n---\nbody")
    (hub.cfg.content_dir / "agents" / "nofm.md").write_text("no frontmatter")
    cat = hub.work.catalog()
    assert {p.path.split(":")[0] for p in cat.problems} >= {"agents/broken", "agents/mismatch", "agents/nofm"}
    assert not cat.items[("agent", "broken")].valid


def test_yaml_bomb_and_symlink_and_huge_file_in_content(hub: Hub, tmp_path: Path) -> None:
    root = hub.cfg.content_dir
    (root / "rules" / "bomb.yaml").write_text("id: bomb\ntitle: &a t\nkind: tool\nmatch: *a\ndecision: deny\n")
    (root / "rules" / "big.yaml").write_text("id: big\n" + "x: " + "a" * 400_000)
    secret = tmp_path / "secret.yaml"
    secret.write_text("id: link\ntitle: t\nkind: tool\nmatch: x\ndecision: deny\n")
    (root / "rules" / "link.yaml").symlink_to(secret)
    cat = hub.work.catalog()
    assert not cat.items[("rule", "bomb")].valid and not cat.items[("rule", "big")].valid
    assert ("rule", "link") not in cat.items        # symlinked content files are ignored, not followed


def test_delete_drops_references(hub: Hub, home: Path) -> None:
    add_client(hub, home)
    seed_content(hub)
    hub.work.delete("skill", "alpha")
    assert all(m.name != "alpha" for m in hub.work.get_collection("core").members)
    hub.work.delete("agent", "reviewer")
    assert "reviewer" not in hub.work.get_profile("fake1").enable.agents


def test_client_and_profile_validation(hub: Hub, home: Path) -> None:
    with pytest.raises(Unprocessable):
        hub.work.put_client("bad", {"adapter": "generic", "display_name": "x"})            # generic needs spec
    with pytest.raises(Unprocessable):
        hub.work.put_client("bad", {"adapter": "fake", "display_name": "x", "manage": {"nonsense": True}})
    with pytest.raises(Unprocessable):
        hub.work.put_profile("fake1", {"enable": {"skills": ["../x"]}})
    with pytest.raises(Unprocessable):
        hub.work.put_profile("fake1", {"mystery": []})


def test_endpoints_validation(hub: Hub) -> None:
    with pytest.raises(Unprocessable):
        hub.work.put_endpoints({"router": "file:///etc/passwd"})
    with pytest.raises(Unprocessable):
        hub.work.put_endpoints({"Bad Key": "http://x"})
    assert hub.work.put_endpoints({"router": "http://127.0.0.1:4000/v1"}) == {"router": "http://127.0.0.1:4000/v1"}
