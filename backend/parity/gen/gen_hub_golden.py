"""Hub-level golden scenarios for WS-B1, recorded from the PYTHON Hub with the test FakeAdapter (backend/tests/core/fakes.py,
copied below so the generator is self-contained). Writes backend/crates/agent-hub/tests/golden/b1_hub.json;
tests/b1_hub_golden.rs replays every step on the Rust Hub (with its port of the FakeAdapter, tests/b1_support) and
compares results and errors.

    "$PYREL/backend/.venv/bin/python" -I backend/parity/gen/gen_hub_golden.py
    (from the repo root; PYREL=$(node backend/parity/lib/python-release.mjs) is the last Python backend, see
     backend/parity/lib/python-release.mjs)

Each scenario runs in its own temp dir (content repo initialised by `init_content`, the client's home with native content
as in test_organize.py's `native` fixture). Normalisation (both sides): the temp dir -> "<base>", `*_at`/`updated`/`created`
numbers -> "<time>", `scan_digest` -> "<digest>" (each hub has its own random attest.key), saved values (random inbox ids) -> "${var}"."""
from __future__ import annotations

import json
import os
import re
import shutil
import sys
import tempfile
from pathlib import Path
from typing import Any

from agent_hub import config as config_mod
from agent_hub.adapters import AdapterRegistry
from agent_hub.adapters._util import slice_digest
from agent_hub.adapters.base import (AdapterCaps, AgentItem, Artifact, Check, ClientAdapter, ClientConfig, Diag,
                                     DiscoveredContent, InstructionItem, McpItem, PermissionSet, RenderContext, RenderResult,
                                     Rule, SkillItem)
from agent_hub.config import Settings
from agent_hub.domain.errors import ProblemError
from agent_hub.services.common import sha256
from agent_hub.services.hub import Hub
from agent_hub.services.skilleval import EvalResult

OUT = Path(__file__).resolve().parents[2] / "crates" / "agent-hub" / "tests" / "golden" / "b1_hub.json"
# the floor of THIS checkout (not the Python release's copy): the golden must reflect the floor the Rust hub ships with
FLOOR = Path(__file__).resolve().parents[3] / "policy" / "floor.yaml"
BEGIN = "<!-- agent-hub:begin (managed block; edits inside are overwritten) -->"
END = "<!-- agent-hub:end -->"


class FakeAdapter(ClientAdapter):
    """backend/tests/core/fakes.py FakeAdapter (verbatim behaviour)."""
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

    def discover(self, cfg: ClientConfig, fs: Any) -> DiscoveredContent:
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
                    dc.agents.append(AgentItem(name=f[:-3], description="found agent", capabilities=["read"],
                                               body=fs.read_bytes(f"{ag}/{f}").decode()))
        if fs.exists(f"{home}/.mcp.json"):
            doc = json.loads(fs.read_bytes(f"{home}/.mcp.json"))
            for n, v in (doc.get("mcpServers") or {}).items():
                dc.mcp_servers.append(McpItem(name=n, transport="stdio", command=v.get("command") or "x", args=v.get("args", []),
                                              scan_status="clean"))
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

    def verify(self, cfg: ClientConfig, expected: Any, fs: Any, run: Any) -> list[Check]:
        out = []
        for e in expected:
            p = f"{cfg.roots[e.root]}/{e.path}"
            if e.merge == "own":
                ok = fs.exists(p) and sha256(fs.read_bytes(p)) == e.sha256
            else:
                ok = fs.exists(p) and slice_digest(fs.read_bytes(p), e.merge, e.managed_keys) == e.sha256
            out.append(Check(name=f"{e.root}:{e.path}", ok=ok, detail="" if ok else "missing or hash differs"))
        rc, out_s, _ = run.run(["fake-client", "--list"], timeout=5)
        out.append(Check(name="fake-client --list", ok=rc == 0, detail=out_s.strip()))
        return out


class FakeRunner:
    def run(self, argv: list[str], *, timeout: float) -> tuple[int, str, str]:
        return 0, "ok", ""


class NoEvalRunner:
    def run(self, argv: list[str], *, cwd: Path, env: Any, timeout: float) -> EvalResult:
        return EvalResult(127, "", "no SkillEvaluator in tests")

    def cancel(self) -> None:
        pass


def native_home(home: Path) -> None:
    """test_organize.py `native`: a client dir that already has content."""
    (home / "skills/native-skill").mkdir(parents=True)
    (home / "skills/native-skill/SKILL.md").write_text("---\nname: native-skill\ndescription: From client\n---\nbody\n")
    (home / "skills/native-skill/x.txt").write_text("x")
    (home / "skills/Bad Name").mkdir(parents=True)
    (home / "skills/Bad Name/SKILL.md").write_text("---\nname: Bad Name\ndescription: odd\n---\nb\n")
    (home / "agents").mkdir()
    (home / "agents/helper.md").write_text("helper body")
    (home / ".mcp.json").write_text(json.dumps({"mcpServers": {"srv": {"command": "run-srv"}}}))
    (home / "settings.json").write_text(json.dumps({"permissions": {"deny": ["tool:web_fetch"], "ask": ["command:git *"]}}))
    (home / "CLAUDE.md").write_text("Native instructions.\n")


class Runner:
    def __init__(self, base: Path) -> None:
        self.base = base
        fe = base / "frontend"
        fe.mkdir()
        (fe / "index.html").write_text("<!doctype html>")
        cfg = Settings(content_dir=base / "content", data_dir=base / "state", policy_file=FLOOR,
                       frontend_dir=fe, trust_loopback=False, knowledge_admin_key_file=base / "no-knowledge-key",
                       inbox_rate_per_min=100)
        reg = AdapterRegistry()
        reg.register(FakeAdapter())
        self.hub = Hub(cfg, registry=reg, runner=FakeRunner(), eval_runner=NoEvalRunner())  # type: ignore[arg-type]
        self.hub.skilleval.which = lambda _name, _path: None
        self.hub.init_content()
        self.home = base / "client-home"
        self.home.mkdir()
        native_home(self.home)

    def run(self, op: str, a: dict[str, Any]) -> Any:
        h = self.hub
        match op:
            case "put_client": return h.put_client(a["cid"], a["payload"], create=a["create"])
            case "client_view": return h.client_view(a["cid"])
            case "list_clients": return h.list_clients()
            case "delete_client": return h.delete_client(a["cid"])
            case "client_status": return h.client_status(a["cid"])
            case "work_put": return h.work.put(a["kind"], a["name"], a["payload"]) and None
            case "work_put_collection": return h.work.put_collection(a["cid"], a["payload"]).model_dump(mode="json")
            case "work_put_profile": return h.work.put_profile(a["cid"], a["payload"]).model_dump(mode="json")
            case "list_items": return h.list_items(a["kind"], **a.get("filters", {}))
            case "item_detail": return h.item_detail(a["kind"], a["name"])
            case "validate_content": return h.validate_content()
            case "matrix": return h.organizer.matrix()
            case "toggle": return h.organizer.toggle(a["client"], a["kind"], a["name"], a["enabled"])
            case "bulk_toggle":
                from agent_hub.domain.models import Member
                return h.organizer.bulk_toggle(a["clients"], [Member(**m) for m in a["items"]], a["enabled"])
            case "effective": return h.effective(a["cid"], a["source"])
            case "commit":
                h.commit(a["message"], "t")
                return None
            case "rendered": return h.rendered(a["cid"], a["kind"], a["name"], a["source"])
            case "discover": return h.importer.discover(a["cid"])
            case "import_items":
                sel = None if a.get("selection") is None else [tuple(x) for x in a["selection"]]
                ov = {(k, n): v for k, n, v in a.get("overrides", [])}
                return h.import_items(a["client"], sel, a.get("on_conflict"), a.get("enable", True), ov,
                                      a.get("override_reason"), "t")
            case "validate_spec": return h.validate_spec(a["body"])
            case "inbox_add": return h.inbox.add(a["client"], a["title"], a["body"], a["mtype"])
            case "inbox_list": return h.inbox.list(a.get("client"))
            case "inbox_promote": return h.inbox.promote(a["iid"], a.get("edits"))
            case "inbox_reject": return h.inbox.reject(a["iid"])
            case "get_profile": return h.work.get_profile(a["cid"]).model_dump(mode="json")
            case "get_client": return h.work.get_client(a["cid"]).model_dump(mode="json", exclude_none=True)
            case "names": return h.work.names(a["kind"])
        raise SystemExit(f"unknown op {op}")


TIME_KEYS = {"adopted_at", "last_applied_at", "last_verified_at", "updated", "created", "imported_at", "observed_at"}


def norm(v: Any, base: str, vars: dict[str, str]) -> Any:
    if isinstance(v, str):
        v = v.replace(base, "<base>")
        for k, val in vars.items():
            v = v.replace(val, "${" + k + "}")
        return v
    if isinstance(v, list):
        return [norm(x, base, vars) for x in v]
    if isinstance(v, dict):
        out = {}
        for k, x in v.items():
            if k in TIME_KEYS and isinstance(x, (int, float)) and not isinstance(x, bool) and x:
                x = "<time>"
            if k == "scan_digest" and isinstance(x, str):
                x = "<digest>"  # HMAC under each hub's random attest.key
            out[norm(k, base, vars)] = norm(x, base, vars)
        return out
    return v


def fill(v: Any, base: str, vars: dict[str, str]) -> Any:
    if isinstance(v, str):
        v = v.replace("<base>", base)
        return re.sub(r"\$\{(\w+)\}", lambda m: vars.get(m.group(1), m.group(0)), v)
    if isinstance(v, list):
        return [fill(x, base, vars) for x in v]
    if isinstance(v, dict):
        return {fill(k, base, vars): fill(x, base, vars) for k, x in v.items()}
    return v


def scenario(name: str, steps: list[dict[str, Any]]) -> dict[str, Any]:
    base = Path(tempfile.mkdtemp(prefix="b1-hub-golden-")).resolve()
    old_env = {k: os.environ.pop(k) for k in list(os.environ) if k.startswith("HUB_") or k == "AGENT_HUB_ENV_FILE"}
    config_mod.ENV_FILE = base / "no-env-file"  # type: ignore[attr-defined]
    try:
        r = Runner(base)
        vars: dict[str, str] = {}
        rec = []
        for st in steps:
            args = fill(st.get("args", {}), str(base), vars)
            try:
                res: Any = {"ok": json.loads(json.dumps(r.run(st["op"], args)))}
                if st.get("save"):
                    vars[st["save"]] = res["ok"]["id"]
            except ProblemError as exc:
                res = {"error": type(exc).__name__, "status": exc.status, "code": exc.code, "detail": exc.detail,
                       "extra": json.loads(json.dumps(exc.extra, default=str))}
            rec.append({**st, "expect": norm(res, str(base), vars)})
        r.hub.close()
        return {"name": name, "steps": rec}
    finally:
        os.environ.update(old_env)
        shutil.rmtree(base, ignore_errors=True)


def P(op: str, save: str | None = None, **args: Any) -> dict[str, Any]:
    d: dict[str, Any] = {"op": op, "args": args}
    if save:
        d["save"] = save
    return d


HOME = "<base>/client-home"
CLIENT = {"adapter": "fake", "display_name": "Fake 1", "roots": {"home": HOME}}
SEED = [
    P("work_put", kind="skill", name="alpha", payload={"description": "Alpha skill", "body": "Do alpha.\n",
                                                      "files": {"ref/notes.md": "notes"}, "tags": ["core"]}),
    P("work_put", kind="agent", name="reviewer", payload={"description": "Reviews code", "capabilities": ["read", "browser"],
                                                          "body": "Review carefully.\n"}),
    P("work_put", kind="instruction", name="style", payload={"title": "Style", "order": 10, "body": "Be concise.\n"}),
    P("work_put", kind="rule", name="no-web", payload={"title": "No web", "kind": "tool", "match": "web_fetch", "decision": "deny"}),
    P("work_put", kind="memory", name="fact1", payload={"type": "reference", "title": "A fact", "body": "The sky is blue.\n"}),
    P("work_put", kind="skill", name="beta", payload={"description": "Beta", "body": "b\n", "source": "import:x"}),
    P("work_put", kind="mcp", name="dirty", payload={"transport": "stdio", "command": "x", "scan_status": "unscanned"}),
    P("work_put", kind="mcp", name="clean", payload={"transport": "stdio", "command": "c", "sandbox_profile": "srt",
                                                     "scan_status": "clean"}),
    P("work_put", kind="instruction", name="only-other", payload={"title": "O", "applies_to": ["someone-else"], "body": "x"}),
    P("work_put_collection", cid="core", payload={"id": "core", "title": "Core", "members": [
        {"kind": "skill", "name": "alpha"}, {"kind": "instruction", "name": "style"}]}),
    P("work_put_collection", cid="extra", payload={"id": "extra", "title": "Extra", "order": 5, "members": [
        {"kind": "mcp", "name": "clean"}, {"kind": "skill", "name": "ghost"}]}),
    P("work_put_profile", cid="fake1", payload={"collections": ["core", "missing-coll"], "enable": {
        "agents": ["reviewer"], "rules": ["no-web"], "skills": ["nope"], "memory": ["fact1"], "mcp": ["dirty"]}}),
]

SCENARIOS = [
    scenario("clients", [
        P("list_clients"),
        P("put_client", cid="fake1", payload=CLIENT, create=True),
        P("put_client", cid="fake1", payload=CLIENT, create=True),
        P("put_client", cid="ghost", payload=CLIENT, create=False),
        P("put_client", cid="hub", payload=CLIENT, create=True),
        P("put_client", cid="a:b", payload=CLIENT, create=True),
        P("put_client", cid="Bad", payload=CLIENT, create=True),
        P("put_client", cid="nope", payload={**CLIENT, "adapter": "nope"}, create=True),
        P("put_client", cid="rootless", payload={**CLIENT, "roots": {"home": "/"}}, create=True),
        P("put_client", cid="relroot", payload={**CLIENT, "roots": {"home": "relative/path"}}, create=True),
        P("put_client", cid="dotdot", payload={**CLIENT, "roots": {"home": "/tmp/../etc"}}, create=True),
        P("put_client", cid="incontent", payload={**CLIENT, "roots": {"home": "<base>/content/skills"}}, create=True),
        P("put_client", cid="invalid", payload={"adapter": "fake"}, create=True),
        P("put_client", cid="extra", payload={**CLIENT, "surprise": 1, "manage": {"bogus": True}}, create=True),
        P("put_client", cid="sy1", payload={**CLIENT, "spec_yaml": "a: [\n"}, create=True),
        P("put_client", cid="sy2", payload={**CLIENT, "spec_yaml": "- x\n"}, create=True),
        P("put_client", cid="sy3", payload={**CLIENT, "spec_yaml": 5}, create=True),
        P("put_client", cid="sy4", payload={**CLIENT, "spec_yaml": "a: 1\n", "spec": {"b": 2}}, create=True),
        P("put_client", cid="sy5", payload={**CLIENT, "spec_yaml": "a: &x 1\nb: *x\n"}, create=True),
        P("put_client", cid="sy6", payload={**CLIENT, "spec_yaml": "when: 2024-01-02\nlist: [1, 2.5]\n"}, create=True),
        P("put_client", cid="fake2", payload={**CLIENT, "roots": {"home": "<base>/client-home2/"}, "manage": {"mcp": True},
                                               "params": {"manage": {"memory": True, "skills": "yes"}, "x": [1, None]},
                                               "icon": "star", "color": None, "description": "two"}, create=True),
        P("put_client", cid="fake1", payload={**CLIENT, "display_name": "Fake One", "strict": False}, create=False),
        P("client_view", cid="fake1"),
        P("client_view", cid="fake2"),
        P("client_view", cid="ghost"),
        P("client_view", cid="Bad"),
        P("list_clients"),
        P("get_client", cid="sy6"),
        P("client_status", cid="fake1"),
        P("validate_spec", body={"spec": {}}),
        P("validate_spec", body={"spec_yaml": "a: ["}),
        P("delete_client", cid="fake2"),
        P("delete_client", cid="fake2"),
        P("list_clients"),
    ]),
    scenario("listing", [
        P("put_client", cid="fake1", payload=CLIENT, create=True),
        *SEED,
        P("list_items", kind="skill"),
        P("list_items", kind="skill", filters={"q": "ALP"}),
        P("list_items", kind="skill", filters={"q": "core"}),
        P("list_items", kind="skill", filters={"tag": "core"}),
        P("list_items", kind="skill", filters={"group": "core"}),
        P("list_items", kind="skill", filters={"client": "fake1", "enabled": True}),
        P("list_items", kind="skill", filters={"client": "fake1", "enabled": False}),
        P("list_items", kind="skill", filters={"client": "fake1"}),
        P("list_items", kind="skill", filters={"enabled": True}),
        P("list_items", kind="skill", filters={"enabled": False}),
        P("list_items", kind="skill", filters={"issues": False}),
        P("list_items", kind="skill", filters={"source": "import:"}),
        P("list_items", kind="skill", filters={"sort": "title"}),
        P("list_items", kind="skill", filters={"sort": "nonsense"}),
        P("list_items", kind="skill", filters={"client": "ghost"}),
        P("list_items", kind="skill", filters={"limit": 1}),
        P("list_items", kind="skill", filters={"limit": 1, "cursor": "1"}),
        P("list_items", kind="agent"), P("list_items", kind="instruction"), P("list_items", kind="mcp"),
        P("list_items", kind="rule"), P("list_items", kind="memory"),
        P("item_detail", kind="skill", name="alpha"),
        P("item_detail", kind="agent", name="reviewer"),
        P("item_detail", kind="mcp", name="clean"),
        P("item_detail", kind="skill", name="ghost"),
        P("item_detail", kind="skill", name="Bad"),
        P("validate_content"),
        P("matrix"),
        P("effective", cid="fake1", source="working"),
        P("effective", cid="fake1", source="bogus"),
        P("effective", cid="ghost", source="working"),
        P("effective", cid="fake1", source="committed"),
        P("commit", message="base"),
        P("effective", cid="fake1", source="committed"),
        P("rendered", cid="fake1", kind="skill", name="alpha", source="committed"),
        P("rendered", cid="fake1", kind="instruction", name="style", source="committed"),
        P("rendered", cid="fake1", kind="agent", name="reviewer", source="working"),
        P("rendered", cid="fake1", kind="skill", name="beta", source="committed"),
        P("rendered", cid="fake1", kind="mcp", name="dirty", source="working"),
        P("rendered", cid="fake1", kind="bogus", name="x", source="working"),
        P("client_view", cid="fake1"),
    ]),
    scenario("toggles", [
        P("put_client", cid="fake1", payload=CLIENT, create=True),
        P("put_client", cid="fake2", payload={**CLIENT, "roots": {"home": "<base>/client-home2"}}, create=True),
        *SEED,
        P("toggle", client="fake1", kind="skill", name="beta", enabled=True),
        P("toggle", client="fake1", kind="skill", name="beta", enabled=False),
        P("toggle", client="fake1", kind="skill", name="alpha", enabled=False),
        P("toggle", client="fake1", kind="skill", name="alpha", enabled=True),
        P("toggle", client="fake1", kind="mcp", name="dirty", enabled=True),
        P("toggle", client="fake1", kind="memory", name="fact1", enabled=True),
        P("toggle", client="fake1", kind="instruction", name="only-other", enabled=True),
        P("toggle", client="nobody", kind="skill", name="beta", enabled=True),
        P("toggle", client="fake1", kind="skill", name="ghost", enabled=True),
        P("toggle", client="fake1", kind="widget", name="x", enabled=True),
        P("bulk_toggle", clients=["fake1", "fake2", "nobody"], items=[{"kind": "skill", "name": "beta"},
                                                                     {"kind": "mcp", "name": "dirty"}], enabled=True),
        P("get_profile", cid="fake1"),
        P("get_profile", cid="fake2"),
        P("matrix"),
    ]),
    scenario("import", [
        P("put_client", cid="fake1", payload={**CLIENT, "params": {"keep": "operator"}}, create=True),
        P("discover", cid="fake1"),
        P("discover", cid="ghost"),
        P("import_items", client="fake1", on_conflict="skip"),
        P("import_items", client="fake1", override_reason="short"),
        P("import_items", client="fake1", on_conflict="nope"),
        P("import_items", client="fake1", selection=[["skill", "native-skill"], ["skill", "zzz"]], on_conflict="rename"),
        P("get_client", cid="fake1"),
        P("get_profile", cid="fake1"),
        P("names", kind="skill"),
        P("client_view", cid="fake1"),
        P("item_detail", kind="skill", name="native-skill"),
        P("commit", message="imported"),
        P("client_view", cid="fake1"),
        P("effective", cid="fake1", source="committed"),
    ]),
    scenario("inbox", [
        P("put_client", cid="fake1", payload=CLIENT, create=True),
        P("inbox_add", save="a", client="fake1", title="Remember the milk", body="Buy milk\n", mtype="user"),
        P("inbox_add", save="b", client="fake1", title="Other", body="text", mtype="feedback"),
        P("inbox_list"),
        P("inbox_list", client="fake1"),
        P("inbox_list", client="../.."),
        P("inbox_promote", iid="${a}", edits={"title": "Milk fact"}),
        P("item_detail", kind="memory", name="${a}"),
        P("inbox_reject", iid="${b}"),
        P("inbox_reject", iid="${b}"),
        P("inbox_list"),
    ]),
]


def main() -> int:
    OUT.write_text(json.dumps({"generator": "backend/parity/gen/gen_hub_golden.py", "scenarios": SCENARIOS},
                              indent=1, ensure_ascii=False) + "\n")
    print(f"wrote {OUT} ({sum(len(s['steps']) for s in SCENARIOS)} steps)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
