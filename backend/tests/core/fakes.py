"""Test doubles: a FakeAdapter exercising every merge mode, a FakeRunner, and a FakeEvalRunner standing in for SkillEvaluator.
No dependency on the real adapters, and no real SkillEvaluator is ever started."""
from __future__ import annotations

import json
import os
import stat
import threading
from collections.abc import Mapping
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath
from typing import Any

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
from agent_hub.services.skilleval import EvalResult

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


FIXTURES = Path(__file__).parent / "fixtures" / "skilleval"


def report(name: str, **changes: Any) -> dict[str, Any]:
    """A real SkillEvaluator report (tests/core/fixtures/skilleval), with top-level fields replaced."""
    d: dict[str, Any] = json.loads((FIXTURES / name).read_text())
    d.update(changes)
    return d


@dataclass
class Canned:
    """What one fake SkillEvaluator tier does: its exit code, the JSON report it writes (None = none), its Markdown, its stderr."""
    rc: int = 1
    report: dict[str, Any] | None = None
    markdown: str | None = "# SkillEvaluator report\n\nAll good.\n"
    stderr: str = ""
    timed_out: bool = False
    extra: dict[str, str] = field(default_factory=dict)        # more files written into the output dir


class NoEvalRunner:
    """The default for the shared `hub` fixture: any call is a test bug (nothing may start SkillEvaluator unasked)."""

    def __init__(self) -> None:
        self.calls: list[list[str]] = []

    def run(self, argv: list[str], *, cwd: Path, env: Mapping[str, str], timeout: float) -> EvalResult:
        self.calls.append(list(argv))
        return EvalResult(127, "", "no SkillEvaluator in tests")

    def cancel(self) -> None:
        pass


class FakeEvalRunner:
    """Pretends to be SkillEvaluator: records argv/cwd/env, snapshots the copy it was given (including any symlink), then writes
    the canned reports into the `-o` directory. `block` holds a run open (concurrency tests)."""

    def __init__(self, tier1: Canned | None = None, tier2: Canned | None = None, block: threading.Event | None = None,
                 version_out: str = "skillevaluator, version 0.5.0", llm: Canned | None = None) -> None:
        self.script = {"tier1": tier1 or Canned(rc=1, report=report("tier1_incomplete.json")),
                       "tier2": tier2 or Canned(rc=0, report=report("tier2_passed.json"), markdown=None)}
        self.llm = llm                                          # what the second (LLM) tier1 pass writes; default: same as `tier1`
        self.block = block
        self.started = threading.Event()
        self.version_out = version_out
        self.calls: list[dict[str, Any]] = []
        self.seen: list[dict[str, Any]] = []                   # per tier run: {files: {rel: bytes}, symlinks: [rel], modes}
        self.cancelled = 0
        self.write_into_cwd = False

    def run(self, argv: list[str], *, cwd: Path, env: Mapping[str, str], timeout: float) -> EvalResult:
        self.calls.append({"argv": list(argv), "cwd": Path(cwd), "env": dict(env), "timeout": timeout})
        if argv[1:] == ["--version"]:
            return EvalResult(0, self.version_out + "\n", "")
        self.started.set()
        if self.block is not None:
            self.block.wait(10)
        tier, copy = argv[1], Path(argv[2])
        out = Path(argv[argv.index("-o") + 1])
        files: dict[str, bytes] = {}
        links: list[str] = []
        modes: dict[str, int] = {}
        for dirpath, dirnames, filenames in os.walk(copy):
            for n in [*dirnames, *filenames]:
                full = Path(dirpath) / n
                rel = full.relative_to(copy).as_posix()
                st = os.lstat(full)
                if stat.S_ISLNK(st.st_mode):
                    links.append(rel)
                elif stat.S_ISREG(st.st_mode):
                    files[rel] = full.read_bytes()
                    modes[rel] = stat.S_IMODE(st.st_mode)
        self.seen.append({"tier": tier, "files": files, "symlinks": links, "modes": modes})
        if self.write_into_cwd:                                 # a misbehaving tool writing next to itself
            (Path(cwd) / "stray.txt").write_text("written by the evaluator")
            (copy / "SKILL.md").write_text("tampered by the evaluator")
        c = self.script[tier]
        if tier == "tier1" and "--no-llm" not in argv and self.llm is not None:
            c = self.llm
        if c.timed_out:
            return EvalResult(-9, "", "", True)
        if c.report is not None:
            (out / f"skillevaluator-{tier}.json").write_text(json.dumps(c.report).replace("@COPY@", str(copy)))
        if c.markdown is not None and tier == "tier1":
            (out / "skillevaluator-tier1.md").write_text(c.markdown.replace("@COPY@", str(copy)))
        for name, text in c.extra.items():
            (out / name).write_text(text)
        return EvalResult(c.rc, "", c.stderr.replace("@COPY@", str(copy)))

    def cancel(self) -> None:
        self.cancelled += 1
