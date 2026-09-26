"""Content document schemas (ARCHITECTURE section 3) and result models. Every content model forbids unknown keys."""
from __future__ import annotations

from typing import Annotated, Any, Literal

from pydantic import AfterValidator, BaseModel, ConfigDict, Field, ValidationError, field_validator, model_validator

from agent_hub.adapters.base import CAPABILITIES
from agent_hub.domain.ids import CONCERNS, ENV_NAME_RE, HOST_RE, check_ident

Ident = Annotated[str, AfterValidator(check_ident)]
Kind = Literal["skill", "agent", "instruction", "mcp", "rule", "memory"]


class Strict(BaseModel):
    model_config = ConfigDict(extra="forbid", populate_by_name=True)


def _short(v: str, n: int = 300) -> str:
    if len(v) > n:
        raise ValueError(f"longer than {n} characters")
    return v


ShortStr = Annotated[str, AfterValidator(_short)]


class Issue(BaseModel):
    path: str
    message: str
    fix_hint: str | None = None


def issues_from(exc: ValidationError, path: str = "") -> list[Issue]:
    return [Issue(path=path or ".".join(str(x) for x in e["loc"]), message=str(e["msg"]))
            for e in exc.errors(include_input=False, include_url=False)]


# ---- item documents ------------------------------------------------------------------------------------------------
class HubMeta(Strict):
    version: Literal[1] = 1
    name: ShortStr = "agent-hub-content"


class SkillMeta(BaseModel):
    """SKILL.md frontmatter. SKILL.md is the client-owned shared standard: only name+description are required and
    validated; every other key (user-invocable, allowed-tools, argument-hint, model, ...) is preserved verbatim."""
    model_config = ConfigDict(extra="allow", populate_by_name=True)
    name: Ident
    description: Annotated[str, Field(min_length=1, max_length=1024)]


class Sidecar(Strict):
    """skills/<name>/hub.yaml: hub bookkeeping kept out of SKILL.md so that file stays standard-clean."""
    groups: list[Ident] = []
    tags: list[Ident] = []
    notes: Annotated[str, Field(max_length=2000)] = ""
    source: ShortStr = ""
    provenance: dict[str, Any] = {}


class InstructionMeta(Strict):
    id: Ident
    title: ShortStr
    order: Annotated[int, Field(ge=-100000, le=100000)] = 100
    applies_to: list[Ident] = []
    tags: list[Ident] = []              # additive: lets bulk tagging work for every kind


class AgentMeta(Strict):
    name: Ident
    description: Annotated[str, Field(min_length=1, max_length=1024)]
    capabilities: list[str] = []
    model_tier: Literal["fast", "standard", "deep"] | None = None
    mode: Literal["subagent", "primary"] = "subagent"
    read_only: bool = False
    tags: list[Ident] = []

    @field_validator("capabilities")
    @classmethod
    def _caps(cls, v: list[str]) -> list[str]:
        bad = [c for c in v if c not in CAPABILITIES]
        if bad:
            raise ValueError(f"{len(bad)} unknown capability name(s); allowed: {sorted(CAPABILITIES)}")
        return v


class McpDoc(Strict):
    name: Ident
    transport: Literal["stdio", "http", "sse"]
    command: ShortStr | None = None
    args: list[ShortStr] = []
    url: ShortStr | None = None
    env_names: list[str] = []
    sandbox_profile: ShortStr | None = None
    egress_hosts: list[str] = []
    scan_status: Literal["unscanned", "clean", "findings"] = "unscanned"
    pinned_ref: ShortStr | None = None
    scan_digest: str | None = None      # HMAC of what was scanned (services/attest.py); `clean` only holds while it matches
    groups: list[Ident] = []
    tags: list[Ident] = []
    notes: Annotated[str, Field(max_length=2000)] = ""

    @field_validator("env_names")
    @classmethod
    def _env(cls, v: list[str]) -> list[str]:
        for n in v:
            if ENV_NAME_RE.fullmatch(n) is None:
                raise ValueError("env var NAMES expected, like API_TOKEN (values never live in the hub)")
        return v

    @field_validator("egress_hosts")
    @classmethod
    def _hosts(cls, v: list[str]) -> list[str]:
        for h in v:
            if HOST_RE.fullmatch(h) is None:
                raise ValueError("invalid host pattern")
        return v

    @model_validator(mode="after")
    def _shape(self) -> McpDoc:
        if self.transport == "stdio" and not self.command:
            raise ValueError("stdio transport needs `command`")
        if self.transport != "stdio" and not self.url:
            raise ValueError(f"{self.transport} transport needs `url`")
        if self.url and not self.url.startswith(("http://", "https://")):
            raise ValueError("url must be http(s)")
        return self


class RuleDoc(Strict):
    id: Ident
    title: ShortStr
    kind: Literal["tool", "command", "path_read", "path_write", "egress_host", "mcp_server"]
    match: Annotated[str, Field(min_length=1, max_length=300)]
    decision: Literal["allow", "ask", "deny"]
    reason: ShortStr | None = None
    applies_to: list[Ident] = []
    tags: list[Ident] = []

    @field_validator("match")
    @classmethod
    def _nonul(cls, v: str) -> str:
        if any(ord(c) < 32 for c in v):
            raise ValueError("control characters not allowed")
        return v


class MemoryMeta(Strict):
    id: Ident
    type: Literal["user", "feedback", "project", "reference"]
    title: ShortStr
    tags: list[Ident] = []


class InboxMeta(Strict):
    id: Ident
    type: Literal["user", "feedback", "project", "reference"]
    title: ShortStr
    client: Ident
    created: float = 0.0


class Member(Strict):
    kind: Kind
    name: Ident


class CollectionDoc(Strict):
    id: Ident
    title: ShortStr
    description: Annotated[str, Field(max_length=2000)] = ""
    icon: Annotated[str, Field(max_length=16)] = ""
    color: Annotated[str, Field(max_length=32, pattern=r"^[#A-Za-z0-9-]*$")] = ""
    order: int = 100
    members: list[Member] = []

    @field_validator("members")
    @classmethod
    def _uniq(cls, v: list[Member]) -> list[Member]:
        seen: set[tuple[str, str]] = set()
        out = []
        for m in v:
            if (m.kind, m.name) not in seen:
                seen.add((m.kind, m.name))
                out.append(m)
        return out


class ClientDoc(Strict):
    id: Ident
    adapter: Ident
    display_name: ShortStr
    description: ShortStr = ""
    roots: dict[Ident, ShortStr] = {}
    params: dict[str, Any] = {}
    spec: dict[str, Any] | None = None
    strict: bool = True
    icon: ShortStr | None = None
    color: ShortStr | None = None
    manage: dict[str, bool] = {}

    @field_validator("manage")
    @classmethod
    def _manage(cls, v: dict[str, bool]) -> dict[str, bool]:
        bad = [k for k in v if k not in CONCERNS]
        if bad:
            raise ValueError(f"unknown concern name(s); allowed: {list(CONCERNS)}")
        return v

    @model_validator(mode="after")
    def _spec(self) -> ClientDoc:
        if self.adapter == "generic" and not self.spec:
            raise ValueError("adapter 'generic' requires `spec`")
        return self


class Selection(Strict):
    skills: list[Ident] = []
    agents: list[Ident] = []
    mcp: list[Ident] = []
    instructions: list[Ident] = []
    rules: list[Ident] = []
    memory: list[Ident] = []


class ProfileDoc(Strict):
    collections: list[Ident] = []
    enable: Selection = Field(default_factory=Selection)
    disable: Selection = Field(default_factory=Selection)


# ---- findings, plans and results -----------------------------------------------------------------------------------
class Finding(BaseModel):
    """A structured floor/audit finding."""
    code: str
    severity: Literal["info", "warn", "error"]
    message: str
    item: str | None = None
    concern: str | None = None
    rule: str | None = None


PlanAction = Literal["add", "change", "remove", "unchanged", "conflict", "advisory"]


class PlanFile(BaseModel):
    root: str
    path: str
    action: PlanAction
    kind: str
    source_ids: list[str] = []
    diff: str = ""
    reason: str = ""
    merge: str = "own"
    managed: bool = True


class ClientPlan(BaseModel):
    client: str
    files: list[PlanFile] = []
    diagnostics: list[dict[str, Any]] = []
    floor_violations: list[Finding] = []
    summary: dict[str, int] = {}
    blocked: bool = False
    blocked_reasons: list[str] = []
    digest: str = ""


class Plan(BaseModel):
    id: str
    content_hash: str
    created_at: float
    clients: list[ClientPlan]
    pending_changes: int = 0            # uncommitted content changes NOT included in this plan
    preview: bool = False               # rendered from the working tree; can never be applied
