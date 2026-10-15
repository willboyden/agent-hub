"""HermesAdapter.observe (Addendum F2): argv form, constant program, untrusted-output handling, and the program itself run locally."""
from __future__ import annotations

import base64
import json
import subprocess
import sys
from pathlib import Path

import pytest
from fakes import FakeFS, FakeRunner

from agent_hub.adapters import hermes as H
from agent_hub.adapters.hermes import ADAPTER, HermesAdapter


def b64(s: str | bytes) -> str:
    return base64.b64encode(s.encode() if isinstance(s, str) else s).decode()


def md(name: str, desc: str = "d") -> str:
    return f"---\nname: {name}\ndescription: {desc}\n---\nbody\n"


@pytest.fixture
def adapter() -> HermesAdapter:
    return ADAPTER()


@pytest.fixture
def cfg(adapter: HermesAdapter):
    c = adapter.default_config()
    c.params.update(compose_project="proj", compose_service="hermes", docker_context="rootless", exec_user="hermes",
                    skills_dest="/opt/data/skills/hub")
    return c


def doc(files: dict[str, str], manifest: list[str] | None = None, truncated: bool = False) -> str:
    return json.dumps({"manifest": manifest or [], "files": files, "truncated": truncated})


def observe(adapter, cfg, out: str, rc: int = 0, err: str = ""):
    r = FakeRunner([(0, "ctr-1\n", ""), (rc, out, err)])
    return adapter.observe(cfg, FakeFS(), r), r


def test_argv_and_constant_program(adapter, cfg) -> None:
    obs, r = observe(adapter, cfg, doc({"a/SKILL.md": b64(md("a"))}))
    assert r.calls[0][:3] == ["docker", "--context", "rootless"] and r.calls[0][3] == "ps"
    assert "label=com.docker.compose.service=hermes" in r.calls[0]
    assert r.calls[1] == ["docker", "--context", "rootless", "exec", "--user", "hermes", "ctr-1", "python3", "-c",
                          H._OBSERVE_PROGRAM, "/opt/data/skills"]
    assert obs.complete and obs.source == "container ctr-1: /opt/data/skills"


def test_no_container_is_incomplete_and_nothing_started(adapter, cfg) -> None:
    r = FakeRunner([(0, "", "")])
    obs = adapter.observe(cfg, FakeFS(), r)
    assert not obs.complete and obs.skills == [] and obs.notes and len(r.calls) == 1


def test_extra_services_in_order(adapter, cfg) -> None:
    cfg.params["observe_services"] = ["desktop"]
    r = FakeRunner([(0, "", ""), (0, "c2\n", ""), (0, doc({}), "")])
    obs = adapter.observe(cfg, FakeFS(), r)
    assert "label=com.docker.compose.service=desktop" in r.calls[1] and r.calls[2][r.calls[2].index("exec") + 1 + 2] == "c2"
    assert obs.complete


def test_docker_missing(adapter, cfg) -> None:
    obs = adapter.observe(cfg, FakeFS(), FakeRunner(raises=True))
    assert not obs.complete and obs.notes


def test_placeholder_project_never_runs(adapter) -> None:
    r = FakeRunner()
    assert not adapter.observe(adapter.default_config(), FakeFS(), r).complete and r.calls == []


@pytest.mark.parametrize("key,val", [("observe_python", "py thon; rm"), ("observe_root", "relative/x"), ("observe_root", "/a/../b"),
                                      ("observe_root", "/a b"), ("exec_user", "x y"), ("compose_service", "a;b"),
                                      ("observe_services", ["bad name"])])
def test_param_validation_refuses_before_running(adapter, cfg, key, val) -> None:
    cfg.params[key] = val
    r = FakeRunner()
    obs = adapter.observe(cfg, FakeFS(), r)
    assert not obs.complete and r.calls == []


def test_grouping_nesting_and_origins(adapter, cfg) -> None:
    files = {
        "devops/foo/SKILL.md": b64(md("foo-real", "Foo")), "devops/foo/scripts/x.sh": b64("x"),
        "devops/foo/refs/inner/SKILL.md": b64(md("inner")), "devops/foo/refs/inner/y.md": b64("y"),
        "mine/SKILL.md": b64("no frontmatter"),
        "lab/l/SKILL.md": b64(md("l")), "hub/h/SKILL.md": b64(md("h")),
        ".archive/old/SKILL.md": b64(md("old")), ".archive/bundledold/SKILL.md": b64(md("bundledold")),
        "stray.txt": b64("ignored"),
    }
    obs, _ = observe(adapter, cfg, doc(files, ["foo:abc123", "bundledold:ff"]))
    by = {s.path: s for s in obs.skills}
    foo = by["devops/foo"]
    assert foo.name == "foo-real" and foo.description == "Foo" and foo.origin == "shipped"
    assert set(foo.files) == {"SKILL.md", "scripts/x.sh"}
    assert set(by["devops/foo/refs/inner"].files) == {"SKILL.md", "y.md"}
    assert by["mine"].name == "mine" and by["mine"].origin == "agent"
    assert by["lab/l"].origin == "lab_sync" and by["hub/h"].origin == "hub"
    assert by[".archive/old"].origin == "archived" and by[".archive/bundledold"].origin == "archived"
    assert obs.complete


def test_truncated_flag_makes_incomplete(adapter, cfg) -> None:
    obs, _ = observe(adapter, cfg, doc({"a/SKILL.md": b64(md("a"))}, truncated=True))
    assert not obs.complete and len(obs.skills) == 1


@pytest.mark.parametrize("out", ["[]", "null", "not json", '{"files": []}', "", '{"files": {}}'[:-3]])
def test_non_object_or_bad_json_rejected(adapter, cfg, out) -> None:
    obs, _ = observe(adapter, cfg, out)
    assert not obs.complete and obs.skills == [] and obs.notes


def test_bad_paths_and_base64_dropped(adapter, cfg) -> None:
    files = {"ok/SKILL.md": b64(md("ok")), "../evil/SKILL.md": b64("x"), "/abs/SKILL.md": b64("x"), "a//b/SKILL.md": b64("x"),
             "bad/SKILL.md": "not base64!!", "pad/SKILL.md": "QUJD\nREVG", "num/SKILL.md": 5}
    obs, _ = observe(adapter, cfg, doc(files))
    assert [s.path for s in obs.skills] == ["ok"] and not obs.complete and "rejected" in " ".join(obs.notes)


def test_oversize_output_and_file_caps(adapter, cfg) -> None:
    obs, _ = observe(adapter, cfg, " " * (H.MAX_OBSERVE_OUT + 1))
    assert not obs.complete and obs.skills == []
    big = b64(b"x" * (H.MAX_FILE + 1))
    obs, _ = observe(adapter, cfg, doc({"a/SKILL.md": b64(md("a")), "a/big.bin": big}))
    assert not obs.complete and set(obs.skills[0].files) == {"SKILL.md"}


def test_long_name_and_description_capped_and_bad_yaml(adapter, cfg) -> None:
    files = {"a/SKILL.md": b64(md("n" * 5000, "d" * 9000)), "b/SKILL.md": b64("---\nname: &x [*x\n---\n")}
    obs, _ = observe(adapter, cfg, doc(files))
    by = {s.path: s for s in obs.skills}
    assert len(by["a"].name) == H.MAX_SKILL_NAME and len(by["a"].description) == H.MAX_SKILL_DESC and by["b"].name == "b"


def test_stderr_note_is_capped_and_single_line(adapter, cfg) -> None:
    obs, _ = observe(adapter, cfg, "", rc=1, err="line1\nSECRET\x1b[31m" + "z" * 1000)
    assert not obs.complete and len(obs.notes[0]) < 260 and "\n" not in obs.notes[0] and "\x1b" not in obs.notes[0]


# ---- the program itself, run for real against a temp tree -----------------------------------------------------------------
def run_program(root: Path) -> dict:
    p = subprocess.run([sys.executable, "-c", H._OBSERVE_PROGRAM, str(root)], capture_output=True, text=True, timeout=30)
    assert p.returncode == 0, p.stderr
    return json.loads(p.stdout)


def test_program_walks_safely(tmp_path: Path) -> None:
    (tmp_path / "a").mkdir()
    (tmp_path / "a" / "SKILL.md").write_text("x")
    (tmp_path / "a" / ".hidden").write_text("h")
    (tmp_path / ".curator_backups").mkdir()
    (tmp_path / ".curator_backups" / "SKILL.md").write_text("b")
    (tmp_path / ".archive" / "o").mkdir(parents=True)
    (tmp_path / ".archive" / "o" / "SKILL.md").write_text("o")
    (tmp_path / ".other").mkdir()
    (tmp_path / ".other" / "SKILL.md").write_text("o")
    (tmp_path / ".bundled_manifest").write_text("a:123\n")
    outside = tmp_path.parent / (tmp_path.name + "-out")
    outside.mkdir()
    (outside / "SKILL.md").write_text("secret")
    (tmp_path / "link").symlink_to(outside)
    (tmp_path / "a" / "flink").symlink_to(outside / "SKILL.md")
    out = run_program(tmp_path)
    assert sorted(out["files"]) == [".archive/o/SKILL.md", "a/SKILL.md"]
    assert out["manifest"] == ["a:123"] and out["truncated"] is False


def test_program_caps_and_missing_root(tmp_path: Path) -> None:
    (tmp_path / "s").mkdir()
    (tmp_path / "s" / "SKILL.md").write_text("x")
    (tmp_path / "s" / "big").write_bytes(b"x" * (1024 * 1024 + 1))
    out = run_program(tmp_path)
    assert out["truncated"] is True and list(out["files"]) == ["s/SKILL.md"]
    p = subprocess.run([sys.executable, "-c", H._OBSERVE_PROGRAM, str(tmp_path / "nope")], capture_output=True, text=True)
    assert p.returncode == 3


def test_surrogate_and_control_paths_rejected(adapter, cfg) -> None:
    files = {"ok/SKILL.md": b64(md("ok")), "a/\ud800.txt": b64("x"), "a/\x7f.txt": b64("x"), "a/‮.txt": b64("x"),
             "a/ .txt": b64("x")}
    obs, _ = observe(adapter, cfg, json.dumps({"manifest": [], "files": files, "truncated": False}, ensure_ascii=True))
    assert [s.path for s in obs.skills] == ["ok"] and not obs.complete and "rejected" in " ".join(obs.notes)


def test_file_dir_prefix_collision_skipped(adapter, cfg) -> None:
    files = {"a/SKILL.md": b64(md("a")), "a/x": b64("f"), "a/x/y": b64("g")}
    obs, _ = observe(adapter, cfg, doc(files))
    assert not obs.complete and "collide" in " ".join(obs.notes)
    assert set(obs.skills[0].files) == {"SKILL.md", "x/y"}
