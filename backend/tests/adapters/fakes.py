"""Offline fakes + sample content shared by the adapter tests (no real file, process or network access)."""
from __future__ import annotations

import json
import os
import posixpath
from pathlib import Path
from typing import Any

from agent_hub.adapters.base import (
    AgentItem,
    ClientConfig,
    InstructionItem,
    McpItem,
    MemoryItem,
    PermissionSet,
    RenderContext,
    RenderResult,
    Rule,
    SkillItem,
)

GOLDEN = Path(__file__).parent / "golden"


class FakeFS:
    """Dict-backed FileSystem: keys are absolute posix paths."""

    def __init__(self, files: dict[str, bytes] | None = None) -> None:
        self.files = dict(files or {})

    def read_bytes(self, path: str) -> bytes:
        if path not in self.files:
            raise FileNotFoundError(path)
        return self.files[path]

    def exists(self, path: str) -> bool:
        return path in self.files or self.is_dir(path)

    def is_dir(self, path: str) -> bool:
        p = path.rstrip("/") + "/"
        return any(k.startswith(p) for k in self.files)

    def listdir(self, path: str) -> list[str]:
        p = path.rstrip("/") + "/"
        return sorted({k[len(p):].split("/")[0] for k in self.files if k.startswith(p)})


class FakeRunner:
    def __init__(self, responses: list[tuple[int, str, str]] | None = None, raises: bool = False) -> None:
        self.calls: list[list[str]] = []
        self.responses = list(responses or [])
        self.raises = raises

    def run(self, argv: list[str], *, timeout: float) -> tuple[int, str, str]:
        self.calls.append(argv)
        if self.raises:
            raise OSError("no docker")
        return self.responses.pop(0) if self.responses else (0, "", "")


def skill(name: str, desc: str = "Does a thing. Use when asked.", extra: dict[str, bytes] | None = None) -> SkillItem:
    files = {"SKILL.md": f"---\nname: {name}\ndescription: {desc}\n---\n\n# {name}\n\nSteps.\n".encode()}
    files.update(extra or {})
    return SkillItem(name=name, description=desc, files=files)


def sample_ctx(cfg: ClientConfig, **over: Any) -> RenderContext:
    base: dict[str, Any] = dict(
        skills=[skill("example-skill", extra={"scripts/run.sh": b"#!/bin/sh\necho hi\n"}), skill("example-other-skill")],
        agents=[
            AgentItem(name="reviewer", description="Reviews changes; read-only.", capabilities=["read", "shell"],
                      model_tier="standard", read_only=True, body="You review.\n"),
            AgentItem(name="builder", description="Builds things.", capabilities=["read", "write", "edit", "shell", "web_fetch"],
                      model_tier="fast", body="You build.\n"),
        ],
        instructions=[InstructionItem(id="b-second", title="Second", order=2, body="Two."),
                      InstructionItem(id="a-first", title="First", order=1, body="One.")],
        mcp_servers=[
            McpItem(name="serena", transport="stdio", command="serena", args=["start-mcp-server"], env_names=["SERENA_HOME"],
                    sandbox_profile="sandbox", scan_status="clean"),
            McpItem(name="docs", transport="http", url="http://mcpo:8000/docs"),
        ],
        memory=[MemoryItem(id="m1", type="project", title="Fact", body="Body.")],
        permissions=PermissionSet(rules=[
            Rule(kind="path_read", match="**/private-keys/**", decision="deny"),
            Rule(kind="command", match="docker compose *", decision="allow"),
            Rule(kind="command", match="sudo *", decision="ask"),
            Rule(kind="tool", match="web_fetch", decision="ask"),
            Rule(kind="egress_host", match="pypi.org", decision="allow"),
        ]),
        endpoints={"router": "http://127.0.0.1:4000/v1"},
    )
    base.update(over)
    return RenderContext(client=cfg, **base)


def shuffled(ctx: RenderContext) -> RenderContext:
    return ctx.model_copy(update=dict(skills=list(reversed(ctx.skills)), agents=list(reversed(ctx.agents)),
                                      instructions=list(reversed(ctx.instructions)), mcp_servers=list(reversed(ctx.mcp_servers)),
                                      memory=list(reversed(ctx.memory)),
                                      permissions=PermissionSet(rules=list(ctx.permissions.rules))))


def snapshot(res: RenderResult) -> dict[str, Any]:
    return {
        "artifacts": [{"root": a.root, "path": a.path, "kind": a.kind, "merge": a.merge, "managed_keys": a.managed_keys,
                       "source_ids": a.source_ids, "content": a.content.decode()} for a in res.artifacts],
        "diagnostics": [{"severity": d.severity, "code": d.code, "item": d.item, "message": d.message}
                        for d in sorted(res.diagnostics, key=lambda d: (d.severity, d.code, d.item or "", d.message))],
    }


def assert_golden(name: str, res: RenderResult) -> None:
    """Compare with tests/adapters/golden/<name>.json. Regenerate deliberately with UPDATE_GOLDEN=1 and review the diff."""
    path = GOLDEN / f"{name}.json"
    text = json.dumps(snapshot(res), indent=2, sort_keys=True, ensure_ascii=False) + "\n"
    if os.environ.get("UPDATE_GOLDEN") == "1":
        path.write_text(text, encoding="utf-8")
    assert path.exists(), f"golden {path} missing (run UPDATE_GOLDEN=1 once and review it)"
    assert path.read_text(encoding="utf-8") == text


def tree(res: RenderResult, base: str) -> dict[str, bytes]:
    """Render result -> fake filesystem entries under ``base`` (own artifacts only)."""
    return {posixpath.join(base, a.path): a.content for a in res.artifacts if a.merge == "own"}
