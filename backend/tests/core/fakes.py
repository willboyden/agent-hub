"""Test doubles: a FakeAdapter exercising every merge mode, and a FakeRunner. No dependency on the real adapters."""
from __future__ import annotations

import json
from pathlib import PurePosixPath

from agent_hub.adapters._util import slice_digest
from agent_hub.adapters.base import (
    AdapterCaps,
    AgentItem,
    Artifact,
    Check,
    ClientAdapter,
    ClientConfig,
    CommandRunner,
    Diag,
    DiscoveredContent,
    Expected,
    FileSystem,
    InstructionItem,
    McpItem,
    PermissionSet,
    RenderContext,
    RenderResult,
    Rule,
    SkillItem,
)
from agent_hub.services.common import sha256

BEGIN = "<!-- agent-hub:begin (managed block; edits inside are overwritten) -->"
END = "<!-- agent-hub:end -->"


class FakeAdapter(ClientAdapter):
    id = "fake"
    display_name = "Fake client"
    tool_map = {"read": "Read", "shell": "Bash", "browser": None}

    def __init__(self, memory: bool = False) -> None:
        self.memory = memory

    def caps(self) -> AdapterCaps:
        return AdapterCaps(skills=True, agents=True, instructions=True, mcp=True, permissions=True, egress=True,
                           memory=self.memory, tool_map=self.tool_map, notes="fake")

    def default_config(self) -> ClientConfig:
        return ClientConfig(id="fake", adapter="fake", display_name="Fake", roots={"home": "~/.fake"})

    def render(self, ctx: RenderContext) -> RenderResult:
        arts: list[Artifact] = []
        diags: list[Diag] = []
        for s in ctx.skills:
            for rel, blob in sorted(s.files.items()):
                arts.append(Artifact(root="home", path=f"skills/{s.name}/{rel}", content=blob, kind="skill", source_ids=[s.name]))
        for a in ctx.agents:
            for c in a.capabilities:
                if self.tool_map.get(c, "x") is None:
                    diags.append(Diag(severity="error", code="unsupported_capability", message=f"{c} unsupported", item=a.name))
            arts.append(Artifact(root="home", path=f"agents/{a.name}.md", content=f"# {a.name}\n{a.body}".encode(),
                                 kind="agent", source_ids=[a.name]))
        if ctx.instructions:
            body = "\n\n".join(f"## {i.title}\n\n{i.body.strip()}" for i in ctx.instructions)
            arts.append(Artifact(root="home", path="CLAUDE.md", content=f"{BEGIN}\n{body}\n{END}\n".encode(), kind="instruction",
                                 merge="block", source_ids=[i.id for i in ctx.instructions]))
        servers = {m.name: {"command": m.command, "args": m.args, "url": m.url} for m in ctx.mcp_servers}
        arts.append(Artifact(root="home", path=".mcp.json", content=json.dumps({"mcpServers": servers}).encode(),
                             kind="mcp", merge="json_keys", managed_keys=["mcpServers"], source_ids=sorted(servers)))
        perms: dict[str, list[str]] = {"deny": [], "ask": [], "allow": []}
        for r in ctx.permissions.rules:
            perms[r.decision].append(f"{r.kind}:{r.match}")
        arts.append(Artifact(root="home", path="settings.json", content=json.dumps({"permissions": perms}).encode(),
                             kind="rule", merge="json_keys",
                             managed_keys=["permissions.deny", "permissions.ask", "permissions.allow"], source_ids=["floor"]))
        return RenderResult(artifacts=arts, diagnostics=diags)

    def discover(self, cfg: ClientConfig, fs: FileSystem) -> DiscoveredContent:
        home = cfg.roots["home"]
        dc = DiscoveredContent()
        sk = f"{home}/skills"
        if fs.is_dir(sk):
            for name in fs.listdir(sk):
                d = f"{sk}/{name}"
                if not fs.is_dir(d):
                    continue
                files = {}
                for f in fs.listdir(d):
                    files[f] = fs.read_bytes(f"{d}/{f}")
                if "SKILL.md" in files:
                    dc.skills.append(SkillItem(name=name, description="found", files=files))
        ag = f"{home}/agents"
        if fs.is_dir(ag):
            for f in fs.listdir(ag):
                if f.endswith(".md"):
                    dc.agents.append(AgentItem(name=PurePosixPath(f).stem, description="found agent", capabilities=["read"],
                                               body=fs.read_bytes(f"{ag}/{f}").decode()))
        if fs.exists(f"{home}/.mcp.json"):
            doc = json.loads(fs.read_bytes(f"{home}/.mcp.json"))
            for n, v in (doc.get("mcpServers") or {}).items():
                dc.mcp_servers.append(McpItem(name=n, transport="stdio", command=v.get("command") or "x", args=v.get("args", []),
                                              scan_status="clean"))   # a foreign claim of `clean` must be ignored on import
        if fs.exists(f"{home}/settings.json"):
            doc = json.loads(fs.read_bytes(f"{home}/settings.json"))
            rules = []
            for dec, items in (doc.get("permissions") or {}).items():
                for it in items:
                    k, _, m = it.partition(":")
                    rules.append(Rule(kind=k, match=m, decision=dec))  # type: ignore[arg-type]
            dc.permissions = PermissionSet(rules=rules)
        dc.suggested_params = {"instructions_mode": "own", "keep": "adapter"}
        if fs.exists(f"{home}/CLAUDE.md"):
            dc.instructions.append(InstructionItem(id="native", title="Native", order=1,
                                                   body=fs.read_bytes(f"{home}/CLAUDE.md").decode()))
        return dc

    def verify(self, cfg: ClientConfig, expected: list[Expected], fs: FileSystem, run: CommandRunner) -> list[Check]:
        out = []
        for e in expected:
            p = f"{cfg.roots[e.root]}/{e.path}"
            if e.merge == "own":
                ok = fs.exists(p) and sha256(fs.read_bytes(p)) == e.sha256
            else:                                            # key/block merges: only the managed slice is compared
                ok = fs.exists(p) and slice_digest(fs.read_bytes(p), e.merge, e.managed_keys) == e.sha256
            out.append(Check(name=f"{e.root}:{e.path}", ok=ok, detail="" if ok else "missing or hash differs"))
        rc, out_s, _ = run.run(["fake-client", "--list"], timeout=5)
        out.append(Check(name="fake-client --list", ok=rc == 0, detail=out_s.strip()))
        return out


ADAPTER = FakeAdapter


class FakeRunner:
    def __init__(self, rc: int = 0) -> None:
        self.rc = rc
        self.calls: list[list[str]] = []

    def run(self, argv: list[str], *, timeout: float) -> tuple[int, str, str]:
        self.calls.append(argv)
        return self.rc, "ok", ""
