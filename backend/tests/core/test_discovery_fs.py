from __future__ import annotations

from pathlib import Path

import pytest

from agent_hub.services.fs import discovery_fs


def test_discovery_fs_source_dirs_denylist_and_escapes(tmp_path: Path) -> None:
    base, stage, other = tmp_path / "base", tmp_path / "stage", tmp_path / "other"
    for d in (base / "clients/x/skills/s1", base / ".ssh", stage, other):
        d.mkdir(parents=True)
    (base / "clients/x/skills/s1/SKILL.md").write_text("skill")
    for n in (".env", ".env.local", "a.pem", "b.key", "id_rsa_x", "credentials.json", "c.p12"):
        (base / "clients/x/skills" / n).write_text("s")
    (base / "clients/x/skills/.ssh").mkdir()
    (base / "clients/x/skills/.ssh/config").write_text("s")
    (base / "clients/x/AGENTS.md").write_text("agents")
    (base / "clients/y").mkdir()
    (base / "clients/y/private.md").write_text("no")
    (other / "leak.txt").write_text("leak")
    (base / "clients/x/skills/escape").symlink_to(other)
    (base / "linkdir").symlink_to(other)
    params = {"source_root": str(base), "source_dirs": [{"kind": "skills", "path": "clients/x/skills"}, "clients/x/AGENTS.md", "../other", "linkdir", "/etc"]}
    fs = discovery_fs({"stage": str(stage)}, params, [])
    skills = str(base / "clients/x/skills")
    # (a) source_dirs outside roots are discoverable
    assert fs.read_bytes(f"{skills}/s1/SKILL.md") == b"skill" and fs.read_bytes(str(base / "clients/x/AGENTS.md")) == b"agents"
    assert fs.listdir(skills) == ["escape", "s1"]                          # every secret-looking name is invisible
    for n in (".env", ".env.local", "a.pem", "b.key", "id_rsa_x", "credentials.json", "c.p12", ".ssh/config"):
        assert not fs.exists(f"{skills}/{n}")
        with pytest.raises(FileNotFoundError):
            fs.read_bytes(f"{skills}/{n}")
    assert not fs.exists(str(base / ".ssh"))
    # (b) unlisted sibling, `..`, symlink escapes, and out-of-base source_dirs are refused
    for bad in (str(base / "clients/y/private.md"), str(other / "leak.txt"), f"{skills}/escape/leak.txt",
                str(base / "linkdir/leak.txt"), f"{skills}/../../y/private.md", "/etc/hostname"):
        with pytest.raises(PermissionError):
            fs.read_bytes(bad)
    assert fs.exists(str(stage)) and not fs.exists(str(base / "clients/y"))
