"""Client-adapter port. LOCKED: the core (planner, applier, API, UI) depends only on this module.

A *client* is any agent harness that reads instructions/skills/agents/MCP/permissions from somewhere
(claude-code, opencode, hermes-agent, a future Cursor/Codex/Gemini CLI, ...). Supporting a new client
means EITHER writing a declarative spec (see `GenericSpecAdapter`) OR implementing `ClientAdapter`.
No core or frontend change is required for either.

Adapters are PURE with respect to the client: `render` performs no I/O and returns artifacts. Reading
the client's current state (`discover`) and asking it what it loaded (`verify`) go through the injected
`FileSystem` / `CommandRunner` ports so they are testable offline.
"""
from __future__ import annotations

from abc import ABC, abstractmethod
from typing import Any, Literal, Protocol

from pydantic import BaseModel, Field

# ---- canonical vocabularies -------------------------------------------------------------------------------------------
# Tool names differ per client (Bash / bash / terminal). Content is written against these; adapters map them.
Capability = Literal["read", "write", "edit", "shell", "web_fetch", "web_search", "mcp", "subagent", "browser",
                     "notebook", "todo", "image"]
CAPABILITIES: tuple[str, ...] = ("read", "write", "edit", "shell", "web_fetch", "web_search", "mcp", "subagent",
                                 "browser", "notebook", "todo", "image")
ModelTier = Literal["fast", "standard", "deep"]      # adapters map a tier to a concrete model id (or omit)
Decision = Literal["allow", "ask", "deny"]
Severity = Literal["info", "warn", "error"]
Merge = Literal["own", "json_keys", "yaml_keys", "block"]
ItemKind = Literal["skill", "agent", "instruction", "mcp", "rule", "memory"]


# ---- content shapes handed to adapters (already resolved for ONE client: enabled items only) --------------------------
class SkillItem(BaseModel):
    name: str
    description: str
    files: dict[str, bytes]                     # relpath -> bytes; always contains "SKILL.md" (the shared standard)
    groups: list[str] = []
    tags: list[str] = []


class AgentItem(BaseModel):
    name: str
    description: str
    capabilities: list[str]                     # canonical Capability names
    model_tier: ModelTier | None = None
    mode: Literal["subagent", "primary"] = "subagent"
    read_only: bool = False
    body: str                                   # instructions text (markdown)


class InstructionItem(BaseModel):
    id: str
    title: str
    order: int
    body: str


class McpItem(BaseModel):
    name: str
    transport: Literal["stdio", "http", "sse"]
    command: str | None = None
    args: list[str] = []
    url: str | None = None
    env_names: list[str] = []                   # names only; VALUES never live in the hub (secrets stay with brokers/env files)
    sandbox_profile: str | None = None          # e.g. "srt" -> adapter wraps the command in the sandbox runner
    egress_hosts: list[str] = []
    scan_status: Literal["unscanned", "clean", "findings"] = "unscanned"
    pinned_ref: str | None = None


class MemoryItem(BaseModel):
    id: str
    type: Literal["user", "feedback", "project", "reference"]
    title: str
    body: str


class Rule(BaseModel):
    """One abstract permission/egress/approval rule after floor enforcement."""
    kind: Literal["tool", "command", "path_read", "path_write", "egress_host", "mcp_server"]
    match: str                                  # capability name | command glob | path glob | host glob | server name
    decision: Decision
    reason: str | None = None


class PermissionSet(BaseModel):
    rules: list[Rule] = []
    default_tool_decision: Decision = "ask"
    network_default: Decision = "deny"          # egress is deny-by-default; the floor forbids weaker


class ClientConfig(BaseModel):
    """content/clients/<id>.yaml"""
    id: str
    adapter: str                                # adapter id, or "generic" (then `spec` is required)
    display_name: str
    description: str = ""
    roots: dict[str, str] = {}                  # named delivery roots -> absolute paths (core expands ~ and validates)
    params: dict[str, Any] = {}
    spec: dict[str, Any] | None = None          # declarative spec for GenericSpecAdapter
    strict: bool = True                         # unsupported feature => error (True) or warning (False)
    manage: dict[str, bool] = {}                # per concern: True = hub writes it (managed), False = advisory. Missing key =
                                                # the adapter's documented default (skills/agents/instructions on, mcp/permissions off)
    icon: str | None = None
    color: str | None = None


class RenderContext(BaseModel):
    client: ClientConfig
    skills: list[SkillItem] = []
    agents: list[AgentItem] = []
    instructions: list[InstructionItem] = []
    mcp_servers: list[McpItem] = []
    memory: list[MemoryItem] = []
    permissions: PermissionSet = Field(default_factory=PermissionSet)
    endpoints: dict[str, str] = {}              # named URLs (router, knowledge, ...) from content/endpoints.yaml


# ---- what adapters return -----------------------------------------------------------------------------------------------
class Artifact(BaseModel):
    """One file (or one hub-managed slice of a file) an adapter wants delivered."""
    root: str                                   # key into ClientConfig.roots
    path: str                                   # relative; core rejects absolute, `..`, NUL, backslash, symlink escape
    content: bytes
    mode: int = 0o644
    kind: ItemKind
    source_ids: list[str] = []                  # item names/ids this came from (for the UI "why is this here")
    merge: Merge = "own"                        # own: whole file is hub-owned. *_keys: merge only `managed_keys` into an
                                                # existing file, preserving everything else. block: replace only the region
                                                # between hub markers.
    managed_keys: list[str] = []                # dotted key paths owned by the hub (json_keys/yaml_keys)


class Diag(BaseModel):
    severity: Severity
    code: str                                   # e.g. "unsupported_capability", "no_model_mapping"
    message: str
    item: str | None = None


class RenderResult(BaseModel):
    artifacts: list[Artifact] = []
    diagnostics: list[Diag] = []                # errors block apply when ClientConfig.strict


class AdapterCaps(BaseModel):
    """Declares what the client can consume, so the UI can grey out what does not apply and the planner can warn."""
    skills: bool = False
    agents: bool = False
    instructions: bool = False
    mcp: bool = False
    permissions: bool = False
    egress: bool = False
    memory: bool = False
    tool_map: dict[str, str | None] = {}        # canonical capability -> native tool name; None = unsupported
    model_tiers: dict[str, str] = {}            # tier -> native model id
    notes: str = ""


class DiscoveredContent(BaseModel):
    """Reverse direction: what the client already has natively (used by import)."""
    skills: list[SkillItem] = []
    agents: list[AgentItem] = []
    instructions: list[InstructionItem] = []
    mcp_servers: list[McpItem] = []
    permissions: PermissionSet | None = None
    notes: list[str] = []
    suggested_params: dict[str, Any] = {}       # merged by the core into the client's params on import when not already set
                                                # (e.g. {"instructions_mode": "own"} when the instructions came from a whole file)


class Expected(BaseModel):
    root: str
    path: str
    sha256: str
    kind: ItemKind
    source_ids: list[str] = []
    merge: Merge = "own"                        # for json_keys/yaml_keys/block verify checks only the managed slice
    managed_keys: list[str] = []


class Check(BaseModel):
    name: str
    ok: bool
    detail: str = ""


# ---- injected I/O ports (fakes in tests) --------------------------------------------------------------------------------
class FileSystem(Protocol):
    def read_bytes(self, path: str) -> bytes: ...
    def exists(self, path: str) -> bool: ...
    def listdir(self, path: str) -> list[str]: ...
    def is_dir(self, path: str) -> bool: ...
    def size(self, path: str) -> int: ...          # stat only, so callers can cap before reading


class CommandRunner(Protocol):
    def run(self, argv: list[str], *, timeout: float) -> tuple[int, str, str]: ...   # rc, stdout, stderr


# ---- the port -----------------------------------------------------------------------------------------------------------
class ClientAdapter(ABC):
    id: str
    display_name: str

    @abstractmethod
    def caps(self) -> AdapterCaps: ...

    @abstractmethod
    def default_config(self) -> ClientConfig:
        """A starter ClientConfig (roots as documented defaults) used by the UI's 'add client' wizard."""

    @abstractmethod
    def render(self, ctx: RenderContext) -> RenderResult:
        """PURE. Map resolved content to native artifacts; emit Diag for anything unsupported or lossy."""

    @abstractmethod
    def discover(self, cfg: ClientConfig, fs: FileSystem) -> DiscoveredContent:
        """Read what the client natively has, for import. Read-only."""

    @abstractmethod
    def verify(self, cfg: ClientConfig, expected: list[Expected], fs: FileSystem, run: CommandRunner) -> list[Check]:
        """Prove the client actually sees what was delivered (files present with the right hash, and where the client
        offers one, ask it: e.g. a `skills list`). Never assume a copy means it loaded."""
