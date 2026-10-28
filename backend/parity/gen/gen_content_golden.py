"""Golden scenarios for WS-B1's content store, organizer, inbox and suggestions, recorded from the PYTHON services.
Writes backend/crates/agent-hub/tests/golden/b1_content.json; tests/b1_golden.rs replays every step on the Rust services
and compares the results, the errors (class, status, code, detail, extra) and the content tree byte for byte.

    "$PYREL/backend/.venv/bin/python" -I backend/parity/gen/gen_content_golden.py
    (from the repo root; PYREL=$(node backend/parity/lib/python-release.mjs) is the last Python backend, see
     backend/parity/lib/python-release.mjs)

Each scenario runs in its own temp content dir with a fixed attestation key (so MCP digests are reproducible). Steps are
data ({"op": ..., "args": {...}, "save": "<var>"}), interpreted the same way by both sides. Normalisation (both sides):
`updated` mtimes -> "<mtime>", inbox `created` -> "<time>" (also inside inbox files), and every saved value (random inbox
ids) -> "${var}" in results, paths and file contents."""
from __future__ import annotations

import base64
import json
import os
import re
import shutil
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

from pydantic import ValidationError

from agent_hub.adapters import AdapterRegistry
from agent_hub.adapters.base import AdapterCaps, ClientAdapter, ClientConfig, DiscoveredContent, RenderResult
from agent_hub.domain.errors import ProblemError
from agent_hub.domain.models import Member, Sidecar
from agent_hub.services.attest import Attestor
from agent_hub.services.content import ContentStore, validate_endpoints
from agent_hub.services.floor import Floor, load_floor
from agent_hub.services.importer import Importer, ImportGate
from agent_hub.services.inbox import Inbox
from agent_hub.services.organize import Organizer
from agent_hub.services.suggest import suggest

KEY = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
OUT = Path(__file__).resolve().parents[2] / "crates" / "agent-hub" / "tests" / "golden" / "b1_content.json"
# the floor of THIS checkout (not the Python release's copy): the golden must reflect the floor the Rust hub ships with
FLOOR = Path(__file__).resolve().parents[3] / "policy" / "floor.yaml"


def b64(b: bytes) -> str:
    return base64.b64encode(b).decode()


def item_out(it: Any) -> dict[str, Any]:
    return {"kind": it.kind, "name": it.name, "valid": it.valid,
            "meta": it.meta.model_dump(mode="json", exclude_none=True, by_alias=True) if it.meta is not None else None,
            "body": it.body, "files": {k: b64(v) for k, v in it.files.items()}, "file_order": list(it.files),
            "sidecar": it.sidecar.model_dump(mode="json") if it.sidecar is not None else None,
            "issues": [i.model_dump() for i in it.issues], "title": it.title, "description": it.description,
            "tags": it.tags, "groups": it.groups, "source": it.source, "applies_to": it.applies_to,
            "updated": "<mtime>" if it.updated else 0.0}


def catalog_out(cat: Any) -> dict[str, Any]:
    return {"items": [[k, n, item_out(i)] for (k, n), i in cat.items.items()],
            "collections": {k: v.model_dump(mode="json") for k, v in cat.collections.items()},
            "clients": {k: v.model_dump(mode="json", exclude_none=True) for k, v in cat.clients.items()},
            "profiles": {k: v.model_dump(mode="json") for k, v in cat.profiles.items()},
            "endpoints": cat.endpoints, "problems": [p.model_dump() for p in cat.problems]}


def tree(root: Path) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for dirpath, dirnames, filenames in os.walk(root):
        for n in sorted(dirnames + filenames):
            p = Path(dirpath) / n
            rel = p.relative_to(root).as_posix()
            if p.is_symlink():
                out[rel] = {"link": os.path.basename(os.readlink(p))}
            elif p.is_dir():
                out[rel] = {"dir": True}
            else:
                out[rel] = {"b64": b64(p.read_bytes())}
    return dict(sorted(out.items()))


class Canned(ClientAdapter):
    """A client whose discover() returns what the scenario set (no filesystem)."""
    id = "canned"
    display_name = "Canned"

    def __init__(self) -> None:
        self.dc = DiscoveredContent()

    def caps(self) -> AdapterCaps:
        return AdapterCaps(skills=True, agents=True, instructions=True, mcp=True, permissions=True)

    def default_config(self) -> ClientConfig:
        return ClientConfig(id="canned", adapter="canned", display_name="Canned", roots={})

    def render(self, ctx: Any) -> RenderResult:
        return RenderResult()

    def discover(self, cfg: Any, fs: Any) -> DiscoveredContent:
        return self.dc.model_copy(deep=True)

    def verify(self, cfg: Any, expected: Any, fs: Any, run: Any) -> list[Any]:
        return []


class FakeObserve:
    def __init__(self) -> None:
        self.idx: dict[str, Any] | None = None
        self.entries: dict[str, Any] = {}
        self.blobs: dict[str, dict[str, bytes]] = {}

    def index(self, client: str) -> dict[str, Any] | None:
        return self.idx

    def entry(self, client: str, key: str) -> dict[str, Any] | None:
        return self.entries.get(key)

    def files(self, client: str, key: str) -> dict[str, bytes]:
        return dict(self.blobs[key])


def discovered(d: dict[str, Any]) -> DiscoveredContent:
    d = json.loads(json.dumps(d))
    for sk in d.get("skills", []):
        sk["files"] = {k: base64.b64decode(v) for k, v in sk["files"].items()}
    return DiscoveredContent.model_validate(d)


class Runner:
    def __init__(self, base: Path) -> None:
        self.base = base
        self.root = base / "content"
        self.root.mkdir()
        key = base / "attest.key"
        key.write_text(KEY)
        key.chmod(0o600)
        self.store = ContentStore(self.root, max_file=4096, max_skill=16384, attestor=Attestor(key))
        self.org = Organizer(self.store, None, AdapterRegistry())  # type: ignore[arg-type]
        self.inbox = Inbox(self.store, 100, 200)
        self.canned = Canned()
        reg = AdapterRegistry()
        reg.register(self.canned)
        self.importer = Importer(self.store, reg, Floor(load_floor(FLOOR)), self.org, [])
        self.observe = FakeObserve()
        self.audits: list[dict[str, Any]] = []

    def gate(self, g: dict[str, Any] | None) -> ImportGate | None:
        if g is None:
            return None
        res = g.get("result", {"status": "missing"})
        return ImportGate(g["mode"], lambda _h: dict(res), g.get("override_reason"),
                          lambda name, info: self.audits.append({"name": name, "info": info}))

    def run(self, op: str, a: dict[str, Any]) -> Any:
        s, o = self.store, self.org
        files = lambda d: {k: base64.b64decode(v) for k, v in d.items()}  # noqa: E731
        match op:
            case "seed": return s.seed()
            case "put": return item_out(s.put(a["kind"], a["name"], a["payload"], must_exist=a.get("must_exist")))
            case "load": return item_out(s.load(a["kind"], a["name"]))
            case "delete": return s.delete(a["kind"], a["name"])
            case "dup": return item_out(s.duplicate_skill(a["name"], a["new_name"]))
            case "names": return s.names(a["kind"])
            case "names_of": return s.names_of(a["sub"])
            case "list_collections": return [c.model_dump(mode="json") for c in s.list_collections()]
            case "get_collection": return s.get_collection(a["cid"]).model_dump(mode="json")
            case "put_collection": return s.put_collection(a["cid"], a["payload"]).model_dump(mode="json")
            case "delete_collection": return s.delete_collection(a["cid"])
            case "get_client": return s.get_client(a["cid"]).model_dump(mode="json", exclude_none=True)
            case "put_client": return s.put_client(a["cid"], a["payload"]).model_dump(mode="json", exclude_none=True)
            case "delete_client": return s.delete_client(a["cid"])
            case "get_profile": return s.get_profile(a["cid"]).model_dump(mode="json")
            case "put_profile": return s.put_profile(a["cid"], a["payload"]).model_dump(mode="json")
            case "endpoints": return s.endpoints()
            case "put_endpoints": return s.put_endpoints(a["data"])
            case "catalog": return catalog_out(s.catalog())
            case "validate_skill_files": return s.validate_skill_files(files(a["files"]))
            case "write_skill_files": return s.write_skill_files(a["name"], files(a["files"]), Sidecar.model_validate(a["sidecar"]))
            case "swap_skill_files": return s.swap_skill_files(a["name"], files(a["files"]), Sidecar.model_validate(a["sidecar"]))
            case "set_sidecar": return s.set_sidecar(a["name"], Sidecar.model_validate(a["sidecar"]))
            case "attest_mcp": return item_out(s.attest_mcp(a["name"]))
            case "raw_write":
                p = self.root / a["rel"]
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_bytes(base64.b64decode(a["b64"]) if "b64" in a else a["text"].encode())
                return None
            case "raw_symlink":
                (self.root / a["rel"]).symlink_to(self.base / a["target"])
                return None
            case "raw_outside":
                (self.base / a["rel"]).write_text(a["text"])
                return None
            case "raw_mkdir":
                (self.root / a["rel"]).mkdir(parents=True, exist_ok=True)
                return None
            case "create_collection": return o.create_collection(a["payload"]).model_dump(mode="json")
            case "update_collection": return o.update_collection(a["cid"], a["payload"]).model_dump(mode="json")
            case "add_members": return o.add_members(a["cid"], [Member(**m) for m in a["members"]]).model_dump(mode="json")
            case "remove_member": return o.remove_member(a["cid"], a["kind"], a["name"]).model_dump(mode="json")
            case "reorder": return [c.model_dump(mode="json") for c in o.reorder(a["ids"])]
            case "accept_suggestions": return o.accept_suggestions(a["ids"], a.get("overrides", {}))
            case "bulk": return o.bulk([Member(**m) for m in a["items"]], a.get("add_to"), a.get("remove_from"),
                                       a.get("add_tags", []), a.get("remove_tags", []))
            case "enable_for_client": return o.enable_for_client(a["client"], a["kind"], a["names"])
            case "suggest": return suggest(s.catalog())
            case "validate_endpoints":
                try:
                    return validate_endpoints(a["data"])
                except ValueError as exc:
                    return {"value_error": str(exc)}
            case "inbox_add": return self.inbox.add(a["client"], a["title"], a["body"], a["mtype"])
            case "inbox_list": return self.inbox.list(a.get("client"))
            case "inbox_promote": return self.inbox.promote(a["iid"], a.get("edits"))
            case "inbox_reject": return self.inbox.reject(a["iid"])
            case "set_discovered":
                self.canned.dc = discovered(a["dc"])
                return None
            case "discover": return self.importer.discover(a["client"])
            case "import":
                sel = None if a.get("selection") is None else [tuple(x) for x in a["selection"]]
                ov = {(k, n): v for k, n, v in a.get("overrides", [])}
                return self.importer.do_import(a["client"], sel, a.get("on_conflict"), a.get("enable", True), ov,
                                               self.gate(a.get("gate")))
            case "set_observation":
                self.observe.idx = a["index"]
                self.observe.entries = a["entries"]
                self.observe.blobs = {k: {r: base64.b64decode(b) for r, b in f.items()} for k, f in a["files"].items()}
                return None
            case "import_observed":
                return self.importer.import_observed(a["client"], a["items"], a["on_conflict"], a.get("enable", False),
                                                     self.gate(a.get("gate")), self.observe)  # type: ignore[arg-type]
            case "audits": return self.audits
            case "inbox_set":
                self.inbox.rate, self.inbox.max_per_client = a["rate"], a["max"]
                return None
        raise SystemExit(f"unknown op {op}")


def err_out(exc: BaseException) -> dict[str, Any]:
    if isinstance(exc, ProblemError):
        return {"error": type(exc).__name__, "status": exc.status, "code": exc.code, "detail": exc.detail,
                "extra": json.loads(json.dumps(exc.extra, default=str))}
    return {"error": type(exc).__name__}


def fill(v: Any, vars: dict[str, str]) -> Any:
    if isinstance(v, str):
        return re.sub(r"\$\{(\w+)\}", lambda m: vars.get(m.group(1), m.group(0)), v)
    if isinstance(v, list):
        return [fill(x, vars) for x in v]
    if isinstance(v, dict):
        return {fill(k, vars): fill(x, vars) for k, x in v.items()}
    return v


def norm(v: Any, vars: dict[str, str]) -> Any:
    if isinstance(v, str):
        for k, val in vars.items():
            v = v.replace(val, "${" + k + "}")
        return v
    if isinstance(v, list):
        return [norm(x, vars) for x in v]
    if isinstance(v, dict):
        out = {}
        for k, x in v.items():
            if k == "created" and isinstance(x, float):
                x = "<time>"
            if k == "imported_at" and isinstance(x, int):
                x = "<time>"
            if k == "content_hash" and isinstance(x, str) and len(x) == 64:
                x = "<hash>"
            out[norm(k, vars)] = norm(x, vars)
        return out
    return v


def norm_tree(t: dict[str, Any], vars: dict[str, str]) -> dict[str, Any]:
    out = {}
    for rel, ent in t.items():
        if "b64" in ent:
            raw = base64.b64decode(ent["b64"])
            if rel.startswith("inbox/"):
                raw = re.sub(rb"(?m)^created: .*$", b"created: <time>", raw)
            raw = re.sub(rb"imported_at: \d+", b"imported_at: <time>", raw)
            for k, val in vars.items():
                raw = raw.replace(val.encode(), ("${" + k + "}").encode())
            ent = {"b64": b64(raw)}
        out[norm(rel, vars)] = ent
    return out


def scenario(name: str, steps: list[dict[str, Any]], needs: str | None = None) -> dict[str, Any]:
    base = Path(tempfile.mkdtemp(prefix="b1-golden-"))
    try:
        r = Runner(base)
        vars: dict[str, str] = {}
        rec = []
        for st in steps:
            args = fill(st.get("args", {}), vars)
            try:
                res: Any = {"ok": json.loads(json.dumps(r.run(st["op"], args)))}
                if st.get("save"):
                    vars[st["save"]] = res["ok"]["id"]
            except (ProblemError, ValidationError, ValueError, OSError, TypeError, AttributeError, KeyError) as exc:
                res = err_out(exc)
            rec.append({**st, "expect": norm(res, vars)})
        out = {"name": name, "steps": rec, "tree": norm_tree(tree(r.root), vars)}
        if needs:
            out["needs"] = needs
        return out
    finally:
        shutil.rmtree(base, ignore_errors=True)


def P(op: str, save: str | None = None, **args: Any) -> dict[str, Any]:
    d: dict[str, Any] = {"op": op, "args": args}
    if save:
        d["save"] = save
    return d


NATIVE_MD = b"---\nname: native-skill\ndescription: From client\n---\nbody\n"
SKILL_MD = "---\nname: rich\ndescription: Rich\nuser-invocable: true\ndisable-model-invocation: false\nallowed-tools: Read, Bash\nargument-hint: '[x]'\nmodel: sonnet\n---\nBody\n"
MCP = {"transport": "stdio", "command": "srv", "args": ["--x"], "sandbox_profile": "srt", "scan_status": "clean"}

def sk(name: str, files: dict[str, bytes], description: str = "found") -> dict[str, Any]:
    return {"name": name, "description": description, "files": {k: b64(v) for k, v in files.items()}}


NATIVE = sk("native-skill", {"SKILL.md": NATIVE_MD, "x.txt": b"x"})
DC = {
    "skills": [NATIVE, sk("Bad Name", {"SKILL.md": b"---\nname: Bad Name\ndescription: odd\n---\nb\n"}),
               sk("nomd", {"x.txt": b"x"}), sk("!!!", {"SKILL.md": b"---\nname: x\ndescription: d\n---\n"}),
               sk("brokenfm", {"SKILL.md": b"---\nname: brokenfm\ndescription: Use when: y\n---\n"}),
               sk("named-wrong", {"SKILL.md": b"---\nname: other\ndescription: d\nextra: [1, 2]\n---\nB\n"})],
    "agents": [{"name": "helper", "description": "", "capabilities": ["read", "fly"], "body": "helper body"},
               {"name": "Agent X", "description": "An agent", "capabilities": [], "model_tier": "deep", "mode": "primary",
                "read_only": True, "body": "x"}],
    "instructions": [{"id": "native", "title": "Native", "order": 1, "body": "Be native.\n"}],
    "mcp_servers": [{"name": "srv", "transport": "stdio", "command": "run-srv", "args": ["-v"], "scan_status": "clean",
                     "env_names": ["OK_NAME", "bad name"], "egress_hosts": ["a.example.com", "bad host!"],
                     "sandbox_profile": "srt"},
                    {"name": "web", "transport": "http", "url": "https://x.example.com/mcp"},
                    {"name": "nocmd", "transport": "stdio", "command": ""}],
    "notes": ["note about native-skill", "general note", "helper was odd"],
    "suggested_params": {"instructions_mode": "own", "keep": "adapter"},
}
CLIENT = {"adapter": "canned", "display_name": "Canned", "roots": {"home": "/tmp/canned-home"},
          "params": {"keep": "operator"}}
OFF = {"mode": "off"}
LEAK = b"api_key=sk-A1b2C3d4E5f6G7h8I9j0\n"
SECRET_GLOB = "~/." + "ssh/**"


def md(name: str, body: str = "Do it.\n") -> bytes:
    return f"---\nname: {name}\ndescription: A skill\n---\n{body}".encode()


OBS = {
    "index": {"observed_at": 1700000000.5, "complete": True, "skills": [{"key": "k1"}, {"key": "k2"}]},
    "entries": {
        "k1": {"name": "foo", "description": "Foo", "path": "foo", "origin": "agent", "content_hash": "abc"},
        "k2": {"name": "leaky", "description": "L", "path": "dir/leaky", "origin": "client", "content_hash": "def"},
        "k3": {"name": "", "description": "", "path": "k3", "origin": "shipped"},
        "k4": {"name": "fat", "path": "fat", "origin": "agent"},
        "k5": {"name": "foo", "path": "foo2", "origin": "agent"},
        "k6": {"name": "foo", "path": "foo3", "origin": "agent"},
        "k7": {"name": "foo", "path": "foo4", "origin": "agent", "content_hash": "ghi"},
    },
    "files": {
        "k1": {"SKILL.md": b64(md("foo"))},
        "k2": {"SKILL.md": b64(md("leaky")), "ref/env.md": b64(LEAK), "bin/x": b64(b"\x00" + LEAK)},
        "k3": {"SKILL.md": b64(md("k3"))},
        "k4": {"SKILL.md": b64(md("fat")), **{f"f{i}.md": b64(b"x") for i in range(210)}},
        "k5": {"SKILL.md": b64(md("foo", "New.\n")), "hub.yaml": b64(b"x")},
        "k6": {"SKILL.md": b64(md("foo", "Newer.\n"))},
        "k7": {"SKILL.md": b64(md("foo", "Long.\n")), ("n" * 300) + ".md": b64(b"1")},
    },
}


SCENARIOS = [
    scenario("skills", [
        P("seed"),
        P("put", kind="skill", name="alpha", payload={"description": "A", "body": "hello\n", "files": {"ref/a.md": "one", "b.txt": "two"},
                                                     "tags": ["x"], "notes": "n", "source": "manual", "allowed-tools": "Read Grep",
                                                     "user-invocable": True, "zz-null": None}),
        P("load", kind="skill", name="alpha"),
        P("put", kind="skill", name="alpha", payload={"description": "A2", "body": "hello\n"}),
        P("names", kind="skill"),
        P("put", kind="skill", name="alpha", payload={"description": "A2", "body": "hello\n", "files": {"only.txt": "x", "deep/er/f.md": "f"},
                                                     "files_b64": {"bin/x.bin": "AAEC"}}),
        P("put", kind="skill", name="alpha", payload={"description": "A3", "body": "b\n", "files": {"only.txt": "y"}}),
        P("put", kind="skill", name="bad", payload={"description": "d", "body": "", "files": {"../evil": "x"}}),
        P("put", kind="skill", name="bad", payload={"description": "d", "body": "", "files": {"SKILL.md": "x"}}),
        P("put", kind="skill", name="bad", payload={"description": "d", "body": "", "files": {".git/config": "x"}}),
        P("put", kind="skill", name="bad", payload={"description": "d", "body": "", "files": {"a\\b": "x"}}),
        P("put", kind="skill", name="bad", payload={"description": "d", "files_b64": {"a": "AAE"}}),
        P("put", kind="skill", name="bad", payload={"description": "d", "files": "not-a-dict"}),
        P("put", kind="skill", name="bad", payload={"description": "d", "files": {f"f{i}": "x" for i in range(201)}}),
        P("put", kind="skill", name="bad", payload={"description": "d", "files": {"big": "x" * 20000}}),
        P("put", kind="skill", name="Bad_Name", payload={"description": "d"}),
        P("put", kind="skill", name="empty", payload={"description": ""}),
        P("put", kind="skill", name="mismatch", payload={"name": "other", "description": "d"}),
        P("put", kind="skill", name="side-bad", payload={"description": "d", "tags": ["NOT OK"]}),
        P("put", kind="skill", name="alpha", payload={"description": "d"}, must_exist=False),
        P("put", kind="skill", name="ghost", payload={"description": "d"}, must_exist=True),
        P("dup", name="alpha", new_name="alpha-copy"),
        P("dup", name="alpha", new_name="alpha-copy"),
        P("dup", name="alpha", new_name="Bad"),
        P("dup", name="ghost", new_name="g2"),
        P("raw_write", rel="skills/rich/SKILL.md", text=SKILL_MD),
        P("raw_write", rel="skills/broken/SKILL.md", text="---\nname: broken\ndescription: Use when: something\n---\nB\n"),
        P("raw_write", rel="skills/nomd/x.txt", text="x"),
        P("raw_write", rel="skills/crlf/SKILL.md", text="---\r\nname: crlf\r\ndescription: CR\r\n---\r\nbody\r\n"),
        P("raw_write", rel="skills/binmd/SKILL.md", b64=b64(b"---\nname: binmd\ndescription: \xff\n---\n")),
        P("raw_write", rel="skills/sidebad/SKILL.md", text="---\nname: sidebad\ndescription: S\ntags: [a, b]\n---\nB\n"),
        P("raw_write", rel="skills/sidebad/hub.yaml", text="groups: nope\nmystery: 1\n"),
        P("raw_write", rel="skills/wrongname/SKILL.md", text="---\nname: other\ndescription: W\n---\n"),
        P("load", kind="skill", name="rich"),
        P("load", kind="skill", name="broken"),
        P("load", kind="skill", name="nomd"),
        P("load", kind="skill", name="crlf"),
        P("load", kind="skill", name="binmd"),
        P("load", kind="skill", name="sidebad"),
        P("load", kind="skill", name="wrongname"),
        P("put", kind="skill", name="rich", payload={"description": "Rich2", "body": "B\n", "user-invocable": True, "model": "sonnet"}),
        P("delete", kind="skill", name="alpha-copy"),
        P("delete", kind="skill", name="alpha-copy"),
        P("load", kind="skill", name="alpha-copy"),
        P("catalog"),
    ]),
    scenario("kinds", [
        P("seed"),
        P("put", kind="agent", name="thing", payload={"description": "d", "capabilities": ["read", "shell"], "model_tier": "fast", "body": "b"}),
        P("put", kind="instruction", name="thing", payload={"title": "T", "order": 5, "applies_to": ["fake1"], "body": "b"}),
        P("put", kind="mcp", name="thing", payload={"transport": "stdio", "command": "srv", "args": ["--x"], "env_names": ["API_TOKEN"], "egress_hosts": ["api.example.com"]}),
        P("put", kind="rule", name="thing", payload={"title": "R", "kind": "path_read", "match": "/data/**", "decision": "ask", "reason": "why"}),
        P("put", kind="memory", name="thing", payload={"type": "user", "title": "M", "body": "text"}),
        P("put", kind="memory", name="num", payload={"type": "user", "title": "M", "body": 5}),
        P("put", kind="agent", name="bad", payload={"description": "d", "capabilities": ["teleport"]}),
        P("put", kind="agent", name="bad", payload={"description": "d", "surprise": 1}),
        P("put", kind="instruction", name="bad", payload={"title": "T", "order": "x"}),
        P("put", kind="instruction", name="bad", payload={"title": "T", "order": "7"}),
        P("put", kind="mcp", name="bad", payload={"transport": "stdio"}),
        P("put", kind="mcp", name="bad", payload={"transport": "http", "url": "ftp://x"}),
        P("put", kind="mcp", name="bad", payload={"transport": "stdio", "command": "c", "env_names": ["TOKEN=hunter2"]}),
        P("put", kind="mcp", name="bad", payload={"transport": "stdio", "command": "c", "egress_hosts": ["bad host!"]}),
        P("put", kind="rule", name="bad", payload={"title": "R", "kind": "nonsense", "match": "x", "decision": "allow"}),
        P("put", kind="rule", name="bad2", payload={"title": "R", "kind": "tool", "match": "x\n", "decision": "maybe"}),
        P("put", kind="memory", name="bad", payload={"type": "secret", "title": "M"}),
        P("put", kind="instruction", name="one", payload={"id": "two", "title": "T"}),
        P("put", kind="widget", name="x", payload={}),
        P("names", kind="widget"),
        P("load", kind="agent", name="../x"),
        P("load", kind="agent", name="A"),
        P("put", kind="agent", name="a b", payload={"description": "d"}),
        P("load", kind="agent", name="x" * 65),
        P("load", kind="agent", name="nope"),
        P("names", kind="agent"), P("names", kind="instruction"), P("names", kind="mcp"), P("names", kind="rule"), P("names", kind="memory"),
        P("load", kind="instruction", name="thing"),
        P("delete", kind="rule", name="thing"),
        # MCP attestation: clean is bound to the scanned fields
        P("put", kind="mcp", name="srv", payload=MCP),
        P("load", kind="mcp", name="srv"),
        P("put", kind="mcp", name="srv", payload={**MCP, "command": "evil"}),
        P("load", kind="mcp", name="srv"),
        P("put", kind="mcp", name="srv", payload={**MCP, "scan_status": "unscanned", "scan_digest": "forged"}),
        P("put", kind="mcp", name="srv", payload={**MCP, "command": "new"}),
        P("load", kind="mcp", name="srv"),
        P("put", kind="mcp", name="srv", payload={**MCP, "scan_digest": "given"}),
        P("attest_mcp", name="srv"),
        P("put", kind="mcp", name="srv2", payload={**MCP, "args": "ab"}),
        P("put", kind="mcp", name="srv3", payload={**MCP, "args": 5}),
        P("attest_mcp", name="nope"),
        P("raw_write", rel="agents/crlf.md", text="---\r\nname: crlf\r\ndescription: d\r\n---\r\nline1\r\nline2\rline3\n"),
        P("load", kind="agent", name="crlf"),
        P("catalog"),
    ]),
    scenario("invalid-files", [
        P("seed"),
        P("raw_write", rel="agents/broken.md", text="---\nname: broken\nbogus: 1\n---\nbody"),
        P("raw_write", rel="agents/mismatch.md", text="---\nname: other\ndescription: d\n---\nbody"),
        P("raw_write", rel="agents/nofm.md", text="no frontmatter"),
        P("raw_write", rel="agents/unterminated.md", text="---\nname: x\n"),
        P("raw_write", rel="agents/listfm.md", text="---\n- a\n---\n"),
        P("raw_write", rel="agents/latin.md", b64=b64(b"---\nname: latin\ndescription: \xe9\n---\n")),
        P("raw_write", rel="rules/bomb.yaml", text="id: bomb\ntitle: &a t\nkind: tool\nmatch: *a\ndecision: deny\n"),
        P("raw_write", rel="rules/big.yaml", text="id: big\n" + "x: " + "a" * 5000),
        P("raw_write", rel="rules/dup.yaml", text="id: dup\nid: dup\n"),
        P("raw_write", rel="rules/empty.yaml", text=""),
        P("raw_outside", rel="secret.yaml", text="id: link\ntitle: t\nkind: tool\nmatch: x\ndecision: deny\n"),
        P("raw_symlink", rel="rules/link.yaml", target="secret.yaml"),
        P("raw_write", rel="rules/Bad Name.yaml", text="id: x\n"),
        P("raw_write", rel="collections/c1.yaml", text="id: c1\ntitle: C\nmembers: [{kind: skill, name: x}, {kind: skill, name: x}]\n"),
        P("raw_write", rel="collections/c2.yaml", text="id: other\ntitle: C\n"),
        P("raw_write", rel="collections/c3.yaml", text="id: c3\ntitle: [\n"),
        P("raw_write", rel="clients/k1.yaml", text="id: k1\nadapter: generic\ndisplay_name: K\n"),
        P("raw_write", rel="profiles/p1.yaml", text="mystery: []\n"),
        P("raw_write", rel="profiles/p2.yaml", text=""),
        P("raw_write", rel="endpoints.yaml", text="Bad: http://x\n"),
        P("raw_write", rel="hub.yaml", text="version: 2\n"),
        P("load", kind="rule", name="bomb"),
        P("load", kind="rule", name="big"),
        P("load", kind="rule", name="dup"),
        P("load", kind="rule", name="empty"),
        P("load", kind="rule", name="link"),
        P("names", kind="rule"),
        P("list_collections"),
        P("get_collection", cid="c2"),
        P("get_collection", cid="c3"),
        P("get_client", cid="k1"),
        P("get_profile", cid="p1"),
        P("get_profile", cid="p2"),
        P("get_profile", cid="none-yet"),
        P("get_profile", cid="Bad"),
        P("endpoints"),
        P("catalog"),
    ]),
    scenario("docs", [
        P("seed"),
        P("endpoints"),
        P("put_endpoints", data={"router": "file:///etc/passwd"}),
        P("put_endpoints", data={"Bad Key": "http://x"}),
        P("put_endpoints", data=["x"]),
        P("put_endpoints", data={"router": "http://127.0.0.1:4000/v1", "kb_2": "https://k"}),
        P("validate_endpoints", data={"1a": "http://x"}),
        P("validate_endpoints", data={"a" * 33: "http://x"}),
        P("validate_endpoints", data={"ok": "http://" + "x" * 300}),
        P("validate_endpoints", data={"ok": 5}),
        P("put_client", cid="fake1", payload={"adapter": "fake", "display_name": "Fake 1", "roots": {"home": "~/x"}, "params": {"a": [1, 2.5, None]}, "manage": {"mcp": True}}),
        P("put_client", cid="bad", payload={"adapter": "generic", "display_name": "x"}),
        P("put_client", cid="bad", payload={"adapter": "fake", "display_name": "x", "manage": {"nonsense": True}}),
        P("put_client", cid="bad", payload={"id": "other", "adapter": "fake", "display_name": "x"}),
        P("put_client", cid="Bad", payload={"adapter": "fake", "display_name": "x"}),
        P("put_client", cid="gen", payload={"adapter": "generic", "display_name": "G", "spec": {"skills": {"root": "home", "path": "s"}}, "icon": None}),
        P("get_client", cid="fake1"),
        P("put_profile", cid="fake1", payload={"enable": {"skills": ["../x"]}}),
        P("put_profile", cid="fake1", payload={"mystery": []}),
        P("put", kind="skill", name="alpha", payload={"description": "Alpha", "body": "Do alpha.\n", "files": {"ref/notes.md": "notes"}, "tags": ["core"]}),
        P("put", kind="agent", name="reviewer", payload={"description": "Reviews code", "capabilities": ["read"], "body": "Review carefully.\n"}),
        P("put", kind="instruction", name="style", payload={"title": "Style", "order": 10, "body": "Be concise.\n"}),
        P("put", kind="rule", name="no-web", payload={"title": "No web", "kind": "tool", "match": "web_fetch", "decision": "deny"}),
        P("put", kind="memory", name="fact1", payload={"type": "reference", "title": "A fact", "body": "The sky is blue.\n"}),
        P("put_collection", cid="core", payload={"id": "core", "title": "Core", "members": [{"kind": "skill", "name": "alpha"}, {"kind": "instruction", "name": "style"}, {"kind": "skill", "name": "alpha"}]}),
        P("put_collection", cid="two", payload={"title": "Two", "order": 5, "color": "#fff", "members": [{"kind": "skill", "name": "alpha"}]}),
        P("put_collection", cid="bad", payload={"id": "x", "title": "T"}),
        P("put_collection", cid="bad", payload={"title": "T", "color": "red;"}),
        P("put_profile", cid="fake1", payload={"collections": ["core", "two"], "enable": {"agents": ["reviewer"], "rules": ["no-web"], "skills": ["alpha"]}, "disable": {"skills": ["alpha"]}}),
        P("list_collections"),
        P("delete", kind="skill", name="alpha"),
        P("get_collection", cid="core"),
        P("get_profile", cid="fake1"),
        P("delete", kind="agent", name="reviewer"),
        P("get_profile", cid="fake1"),
        P("delete_collection", cid="two"),
        P("delete_collection", cid="two"),
        P("get_profile", cid="fake1"),
        P("delete_client", cid="fake1"),
        P("delete_client", cid="fake1"),
        P("names_of", sub="profiles"),
        P("catalog"),
    ]),
    scenario("skill-files", [
        P("seed"),
        P("validate_skill_files", files={"x.md": b64(b"x")}),
        P("validate_skill_files", files={"SKILL.md": b64(b"x"), "a": b64(b"1"), "a/b": b64(b"2")}),
        P("validate_skill_files", files={"SKILL.md": b64(b"x"), "hub.yaml": b64(b"1")}),
        P("validate_skill_files", files={"SKILL.md": b64(b"x"), ".git/hooks/x": b64(b"1")}),
        P("validate_skill_files", files={"SKILL.md": b64(b"x" * 17000)}),
        P("validate_skill_files", files={"SKILL.md": b64(b"x"), **{f"f{i}": b64(b"1") for i in range(200)}}),
        P("write_skill_files", name="imp", files={"SKILL.md": b64(b"---\nname: imp\ndescription: I\n---\nB\n"), "s/run.sh": b64(b"#!/bin/sh\n")},
          sidecar={"source": "import:x", "provenance": {"client": "x", "imported_at": 1}}),
        P("load", kind="skill", name="imp"),
        P("swap_skill_files", name="imp", files={"SKILL.md": b64(b"---\nname: imp\ndescription: I2\n---\nB2\n")}, sidecar={"source": "import:y"}),
        P("load", kind="skill", name="imp"),
        P("swap_skill_files", name="imp", files={"SKILL.md": b64(b"x"), "hub.yaml": b64(b"1")}, sidecar={}),
        P("swap_skill_files", name="imp", files={"SKILL.md": b64(b"x"), ("n" * 300) + ".md": b64(b"1")}, sidecar={}),
        P("load", kind="skill", name="imp"),
        P("set_sidecar", name="imp", sidecar={"tags": ["t1"], "groups": ["g"]}),
        P("set_sidecar", name="ghost", sidecar={}),
        P("raw_write", rel="skills/many/SKILL.md", text="---\nname: many\ndescription: M\n---\n"),
        *[P("raw_write", rel=f"skills/many/f{i:03}.txt", text="x") for i in range(200)],
        P("load", kind="skill", name="many"),
        P("raw_write", rel="skills/fat/SKILL.md", text="---\nname: fat\ndescription: F\n---\n"),
        P("raw_write", rel="skills/fat/big.bin", text="x" * 17000),
        P("load", kind="skill", name="fat"),
        P("raw_write", rel="skills/order/SKILL.md", text="---\nname: order\ndescription: O\n---\n"),
        P("raw_write", rel="skills/order/a.txt", text="1"),
        P("raw_write", rel="skills/order/a/b.txt", text="2"),
        P("raw_write", rel="skills/order/A.txt", text="3"),
        P("raw_write", rel="skills/order/.hidden", text="4"),
        P("raw_outside", rel="out.txt", text="secret"),
        P("raw_symlink", rel="skills/order/link.txt", target="out.txt"),
        P("load", kind="skill", name="order"),
        P("catalog"),
    ]),
    scenario("organize", [
        P("seed"),
        *[P("put", kind="skill", name=n, payload={"description": f"{n} thing", "body": "b\n", "tags": ["gpu"] if n.startswith("payments") else []})
          for n in ("payments-a", "payments-b", "payments-c", "search-index-x", "search-index-y", "search-index-z", "solo")],
        P("put", kind="skill", name="audit-x", payload={"description": "Audit secrets in the sandbox", "body": "b\n", "source": "import:x"}),
        P("put", kind="skill", name="imp-two", payload={"description": "two", "body": "b\n", "source": "import:x"}),
        P("put", kind="skill", name="imp-three", payload={"description": "three", "body": "b\n", "source": "observed:x"}),
        P("put", kind="agent", name="harden-y", payload={"description": "harden egress", "capabilities": [], "body": "b\n"}),
        P("put", kind="mcp", name="scan-security", payload={"transport": "stdio", "command": "c", "notes": "n"}),
        P("put", kind="rule", name="imp-fake1-aaaa", payload={"title": "x", "kind": "tool", "match": "a", "decision": "deny"}),
        P("put", kind="rule", name="imp-fake1-bbbb", payload={"title": "x", "kind": "tool", "match": "b", "decision": "deny"}),
        P("put", kind="rule", name="imp-fake1-cccc", payload={"title": "x", "kind": "tool", "match": "c", "decision": "deny"}),
        P("suggest"),
        P("accept_suggestions", ids=["payments", "search-index"], overrides={"payments": {"title": "Dyn", "members": [{"kind": "skill", "name": "payments-a"}]}}),
        P("accept_suggestions", ids=[]),
        P("accept_suggestions", ids=["payments"]),
        P("accept_suggestions", ids=["nope"]),
        P("accept_suggestions", ids=["security", "security"]),
        P("accept_suggestions", ids=["security"], overrides={"solo": {}}),
        P("accept_suggestions", ids=["security"], overrides={"security": {"icon": "x"}}),
        P("suggest"),
        P("create_collection", payload={"id": "second", "title": "Second", "members": [{"kind": "skill", "name": "solo"}]}),
        P("create_collection", payload={"id": "second", "title": "dup"}),
        P("create_collection", payload={"id": "../x", "title": "bad"}),
        P("create_collection", payload={"id": 5, "title": "bad"}),
        P("create_collection", payload={"id": "third", "title": "T", "members": [{"kind": "widget", "name": "x"}]}),
        P("create_collection", payload={"id": "third", "title": "T", "members": [{"kind": "skill", "name": "ghost"}]}),
        P("create_collection", payload={"id": "third", "title": "T", "members": "ab"}),
        P("create_collection", payload={"id": "third", "title": "T", "members": 5}),
        P("create_collection", payload={"id": "third", "title": "T", "members": [{"kind": "skill"}]}),
        P("add_members", cid="second", members=[{"kind": "skill", "name": "payments-a"}, {"kind": "skill", "name": "payments-a"}]),
        P("add_members", cid="second", members=[{"kind": "skill", "name": "ghost"}]),
        P("add_members", cid="nope", members=[]),
        P("remove_member", cid="second", kind="skill", name="payments-a"),
        P("remove_member", cid="second", kind="skill", name="payments-a"),
        P("reorder", ids=["second", "payments"]),
        P("reorder", ids=["second", "second"]),
        P("reorder", ids=["ghost"]),
        P("list_collections"),
        P("update_collection", cid="second", payload={"id": "second", "title": "Renamed", "members": [{"kind": "agent", "name": "harden-y"}]}),
        P("update_collection", cid="ghost", payload={"title": "x"}),
        P("update_collection", cid="second", payload={"id": "second", "title": "R", "members": [{"kind": "agent", "name": "ghost"}]}),
        P("bulk", items=[{"kind": "skill", "name": "solo"}, {"kind": "agent", "name": "harden-y"}, {"kind": "skill", "name": "ghost"},
                         {"kind": "mcp", "name": "scan-security"}, {"kind": "rule", "name": "imp-fake1-aaaa"}], add_to="second", add_tags=["blue", "blue"]),
        P("bulk", items=[{"kind": "skill", "name": "solo"}, {"kind": "agent", "name": "harden-y"}], remove_from="second", remove_tags=["blue"]),
        P("bulk", items=[{"kind": "skill", "name": "solo"}], add_tags=["NOT VALID"]),
        P("bulk", items=[{"kind": "skill", "name": "solo"}], add_to="ghost"),
        P("raw_write", rel="agents/broken.md", text="nofm"),
        P("bulk", items=[{"kind": "agent", "name": "broken"}], add_tags=["x"]),
        P("put_client", cid="fake1", payload={"adapter": "fake", "display_name": "F", "roots": {"home": "/tmp/x"}}),
        P("enable_for_client", client="fake1", kind="skill", names=["solo", "payments-a", "solo"]),
        P("enable_for_client", client="fake1", kind="agent", names=["harden-y"]),
        P("get_profile", cid="fake1"),
        P("load", kind="skill", name="solo"),
        P("load", kind="agent", name="harden-y"),
        P("load", kind="mcp", name="scan-security"),
        P("catalog"),
    ]),
    scenario("import", [
        P("seed"),
        P("put_client", cid="canned1", payload=CLIENT),
        P("set_discovered", dc=DC),
        P("discover", client="canned1"),
        P("discover", client="ghost"),
        P("import", client="canned1", on_conflict="skip", gate=OFF),
        P("load", kind="skill", name="native-skill"),
        P("load", kind="skill", name="bad-name"),
        P("load", kind="skill", name="named-wrong"),
        P("load", kind="agent", name="helper"),
        P("load", kind="agent", name="agent-x"),
        P("load", kind="mcp", name="srv"),
        P("load", kind="mcp", name="web"),
        P("load", kind="instruction", name="native"),
        P("get_client", cid="canned1"),
        P("get_profile", cid="canned1"),
        P("import", client="canned1", on_conflict="rename", gate=OFF),
        P("import", client="canned1", gate=OFF),
        P("set_discovered", dc={**DC, "skills": [sk("native-skill", {"SKILL.md": NATIVE_MD, "x.txt": b"changed", "new.txt": b"n"})]}),
        P("discover", client="canned1"),
        P("import", client="canned1", selection=[["skill", "native-skill"]], on_conflict="skip", gate=OFF),
        P("import", client="canned1", selection=[["skill", "native-skill"]], gate=OFF),
        P("import", client="canned1", selection=[["skill", "native-skill"]], on_conflict="rename", gate=OFF),
        P("import", client="canned1", selection=[["skill", "native-skill"]], on_conflict="link", gate=OFF),
        P("import", client="canned1", selection=[["skill", "native-skill"]], on_conflict="replace", gate=OFF),
        P("load", kind="skill", name="native-skill"),
        P("set_discovered", dc={**DC, "agents": [{"name": "helper", "description": "Changed", "capabilities": ["shell"],
                                                   "body": "other body"}]}),
        P("import", client="canned1", selection=[["agent", "helper"], ["skill", "does-not-exist"]], gate=OFF),
        P("import", client="canned1", selection=[["agent", "helper"]], overrides=[["agent", "helper", "replace"]],
          on_conflict="skip", gate=OFF),
        P("load", kind="agent", name="helper"),
        P("import", client="canned1", overrides=[["agent", "helper", "obliterate"]], gate=OFF),
        P("import", client="canned1", on_conflict="overwrite-everything", gate=OFF),
        P("import", client="ghost", gate=OFF),
        P("import", client="canned1", selection=[["instruction", "native"]], on_conflict="rename", enable=False, gate=OFF),
        P("get_profile", cid="canned1"),
        P("catalog"),
    ]),
    scenario("import-gate", [
        P("seed"),
        P("put_client", cid="canned1", payload=CLIENT),
        P("set_discovered", dc={"skills": [NATIVE, sk("two", {"SKILL.md": md("two")})]}),
        P("import", client="canned1", gate={"mode": "require"}),
        P("import", client="canned1", gate={"mode": "require", "result": {
            "status": "blocked", "run_id": "r1", "verdict": "fail", "reasons": ["Code Risk: bad", "Secrets: x", "three", "four"]}}),
        P("import", client="canned1", selection=[["skill", "two"]], gate={
            "mode": "require", "override_reason": "reviewed by hand, fine",
            "result": {"status": "blocked", "run_id": "r1", "verdict": "fail", "reasons": ["Code Risk: bad"]}}),
        P("import", client="canned1", selection=[["skill", "native-skill"]],
          gate={"mode": "advisory", "result": {"status": "warn", "run_id": "r2", "verdict": "warn"}}),
        P("import", client="canned1", selection=[["skill", "native-skill"]], on_conflict="rename",
          gate={"mode": "require", "result": {"status": "ok", "run_id": "r3", "verdict": "pass"}}),
        P("load", kind="skill", name="two"),
        P("load", kind="skill", name="native-skill"),
        P("audits"),
    ]),
    scenario("import-observed", [
        P("seed"),
        P("put_client", cid="obs1", payload=CLIENT),
        P("import_observed", client="obs1", items=[{"key": "k1"}], on_conflict="skip", gate=OFF),
        P("set_observation", **OBS),
        P("import_observed", client="obs1", items=[{"key": "k1"}], on_conflict="nope", gate=OFF),
        P("import_observed", client="ghost", items=[{"key": "k1"}], on_conflict="skip", gate=OFF),
        P("import_observed", client="obs1", items=[{"key": "k1"}, {"key": "nope"}, {"key": "k2"}, {"key": "k3", "name": None},
                                                   {"key": "k4"}], on_conflict="skip", enable=True, gate=OFF),
        P("load", kind="skill", name="foo"),
        P("import_observed", client="obs1", items=[{"key": "k1"}], on_conflict="skip", gate=OFF),
        P("import_observed", client="obs1", items=[{"key": "k5"}], on_conflict="replace", gate=OFF),
        P("load", kind="skill", name="foo"),
        P("import_observed", client="obs1", items=[{"key": "k6"}, {"key": "k6", "name": "foo-named"}], on_conflict="rename",
          gate=OFF),
        P("import_observed", client="obs1", items=[{"key": "k2"}], on_conflict="skip", enable=True,
          gate={"mode": "off", "override_reason": "the value is a documented dummy"}),
        P("import_observed", client="obs1", items=[{"key": "k6"}], on_conflict="replace",
          gate={"mode": "require", "override_reason": "known false positive",
                "result": {"status": "blocked", "run_id": "r9", "verdict": "fail", "reasons": ["x"]}}),
        P("import_observed", client="obs1", items=[{"key": "k7"}], on_conflict="replace",
          gate={"mode": "require", "override_reason": "known false positive, reviewed by hand",
                "result": {"status": "blocked", "run_id": "r10", "verdict": "fail", "reasons": ["y"]}}),
        P("import_observed", client="obs1", items=[{"key": "k7", "name": "fresh"}], on_conflict="skip",
          gate={"mode": "off", "override_reason": "not used here, only recorded"}),
        P("load", kind="skill", name="leaky"),
        P("load", kind="skill", name="foo"),
        P("names", kind="skill"),
        P("get_profile", cid="obs1"),
        P("audits"),
        P("catalog"),
    ]),
    scenario("import-rules", [
        P("seed"),
        P("put_client", cid="canned1", payload=CLIENT),
        P("set_discovered", dc={"permissions": {"rules": [
            {"kind": "tool", "match": "web_fetch", "decision": "deny"},
            {"kind": "path_read", "match": SECRET_GLOB, "decision": "allow"},
            {"kind": "command", "match": "git *", "decision": "ask", "reason": "careful " * 50}]}, "notes": []}),
        P("discover", client="canned1"),
        P("import", client="canned1", gate=OFF),
        P("catalog"),
    ], needs="WS-F"),
    scenario("yaml-exceptions", [
        P("seed"),
        P("raw_write", rel="agents/baddate.md", text="---\nname: baddate\ndescription: d\nwhen: 2024-02-30\n---\n"),
        P("load", kind="agent", name="baddate"),
        P("raw_write", rel="rules/kerr.yaml", text="id: kerr\ntitle: t\nkind: tool\nmatch: !!bool maybe\ndecision: deny\n"),
        P("load", kind="rule", name="kerr"),
        P("raw_write", rel="skills/sdate/SKILL.md", text="---\nname: sdate\ndescription: d\nwhen: 2024-02-30\n---\n"),
        P("load", kind="skill", name="sdate"),
        P("raw_write", rel="skills/okside/SKILL.md", text="---\nname: okside\ndescription: d\n---\n"),
        P("raw_write", rel="skills/okside/hub.yaml", text="notes: 2024-02-30\n"),
        P("load", kind="skill", name="okside"),
        P("put", kind="skill", name="okside", payload={"description": "d2"}),
        P("raw_write", rel="collections/cdate.yaml", text="id: cdate\ntitle: T\nx: 2024-02-30\n"),
        P("get_collection", cid="cdate"),
        P("list_collections"),
        P("dup", name="sdate", new_name="sdate2"),
        P("put_client", cid="canned1", payload=CLIENT),
        P("set_discovered", dc={"skills": [sk("sdate", {"SKILL.md": b"---\nname: sdate\ndescription: d\nwhen: 2024-02-31\n---\n"}),
                                           sk("fresh", {"SKILL.md": b"---\nname: fresh\ndescription: d\nwhen: 2024-02-31\n---\n"})]}),
        P("discover", client="canned1"),
        P("import", client="canned1", selection=[["skill", "fresh"]], gate=OFF),
        P("catalog"),
    ], needs="WS-Y"),
    scenario("yaml-exceptions-endpoints", [
        P("seed"),
        P("raw_write", rel="endpoints.yaml", text="router: 2024-02-30\n"),
        P("endpoints"),
        P("catalog"),
        P("raw_write", rel="endpoints.yaml", text="router: !!bool maybe\n"),
        P("catalog"),
    ], needs="WS-Y"),
    scenario("inbox", [
        P("seed"),
        P("put_client", cid="fake1", payload={"adapter": "fake", "display_name": "F", "roots": {"home": "/tmp/x"}}),
        P("inbox_add", save="a", client="fake1", title="Remember the milk", body="Buy milk\n", mtype="user"),
        P("inbox_add", save="b", client="fake1", title="  Other  ", body="text", mtype="feedback"),
        P("inbox_add", save="c", client="fake1", title="!!!", body="x", mtype="project"),
        P("inbox_list"),
        P("inbox_list", client="fake1"),
        P("inbox_list", client="ghost"),
        P("inbox_promote", iid="${a}", edits={"title": "Milk fact"}),
        P("load", kind="memory", name="${a}"),
        P("inbox_promote", iid="${b}", edits={"id": "../x"}),
        P("put", kind="memory", name="taken", payload={"type": "user", "title": "T", "body": "b"}),
        P("inbox_promote", iid="${b}", edits={"id": "taken"}),
        P("inbox_promote", iid="../../etc"),
        P("inbox_promote", iid="${c}", edits={"id": "c-mem", "type": "nope"}),
        P("inbox_reject", iid="${b}"),
        P("inbox_reject", iid="${b}"),
        P("inbox_add", client="ghost", title="t", body="b", mtype="user"),
        P("inbox_add", client="fake1", title="", body="b", mtype="user"),
        P("inbox_add", client="fake1", title="t" * 201, body="b", mtype="user"),
        P("inbox_add", client="fake1", title="t", body="  ", mtype="user"),
        P("inbox_add", client="fake1", title="t", body="é" * 9000, mtype="user"),
        P("inbox_add", client="fake1", title="t", body="b", mtype="secret"),
        P("raw_write", rel="inbox/fake1/broken.md", text="no frontmatter"),
        P("raw_write", rel="inbox/other/x1.md", text="---\nid: x1\ntype: user\ntitle: X\nclient: other\ncreated: 5\n---\nbody"),
        P("inbox_list"),
        P("inbox_set", rate=100, max=2),
        P("inbox_add", client="fake1", title="full", body="b", mtype="user"),
        P("inbox_set", rate=1, max=200),
        P("inbox_add", save="d", client="fake1", title="one", body="b", mtype="user"),
        P("inbox_add", client="fake1", title="two", body="b", mtype="user"),
        P("catalog"),
    ]),
]


def main() -> int:
    OUT.parent.mkdir(parents=True, exist_ok=True)
    doc = {"generator": "backend/parity/gen/gen_content_golden.py", "key": KEY, "max_file": 4096, "max_skill": 16384,
           "scenarios": SCENARIOS}
    OUT.write_text(json.dumps(doc, indent=1, ensure_ascii=False) + "\n")
    print(f"wrote {OUT} ({sum(len(s['steps']) for s in SCENARIOS)} steps)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
