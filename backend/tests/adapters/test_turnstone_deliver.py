"""deploy/turnstone-deliver.py against an in-memory fake of Turnstone's admin API (real HTTP on a loopback port)."""
from __future__ import annotations

import importlib.util
import json
import re
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

import pytest

from agent_hub.adapters.turnstone import DELIVER_SCRIPT

TOKEN = "ts_" + "f" * 40
_spec = importlib.util.spec_from_file_location("turnstone_deliver", DELIVER_SCRIPT)
assert _spec and _spec.loader
deliver = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(deliver)


class FakeTurnstone:
    def __init__(self) -> None:
        self.skills: dict[str, dict[str, Any]] = {}
        self.resources: dict[str, dict[str, str]] = {}
        self.settings: dict[str, Any] = {}
        self.mcp_imports: list[Any] = []
        self.calls: list[tuple[str, str]] = []

    def add(self, name: str, **fields: Any) -> str:
        sid = uuid.uuid4().hex
        self.skills[sid] = {"template_id": sid, "name": name, "description": "", "content": "", "tags": "[]",
                            "author": "", "auto_approve": False, "is_default": False, **fields}
        self.resources[sid] = {}
        return sid

    def by_name(self, name: str) -> dict[str, Any]:
        return next(s for s in self.skills.values() if s["name"] == name)

    @staticmethod
    def parse(raw: str) -> dict[str, Any]:
        m = re.match(r"---\n(.*?)\n---\n?(.*)", raw, re.S)
        fm, body = (m.group(1), m.group(2)) if m else ("", raw)
        meta = dict(line.split(": ", 1) for line in fm.splitlines() if ": " in line)
        return {"name": meta.get("name", ""), "description": meta.get("description", ""), "content": body.strip(),
                "tags": [], "author": meta.get("author", ""), "model": meta.get("model", ""),
                "allowed_tools": [t for t in meta.get("allowed-tools", "").split(",") if t],
                "effort": meta.get("effort", ""), "user_invocable": meta.get("user-invocable") != "false",
                "paths": [], "arguments": [], "argument_hint": ""}

    def handle(self, method: str, path: str, body: Any) -> tuple[int, Any]:
        self.calls.append((method, path))
        p = path.split("?")[0].removeprefix("/v1/api/admin")
        if p == "/skills" and method == "GET":         # like Turnstone 1.8: these fields are not in the list response
            hidden = {"license", "compatibility", "paths", "arguments", "argument_hint", "hidden_from_menu", "allowed_tools"}
            return 200, {"skills": [{k: v for k, v in s.items() if k not in hidden} for s in self.skills.values()],
                         "total": len(self.skills)}
        if p == "/skills/parse" and method == "POST":
            return 200, self.parse(body["raw"])
        if p == "/skills" and method == "POST":
            if any(s["name"] == body["name"] for s in self.skills.values()):
                return 409, {"error": "exists"}
            sid = self.add(body["name"])
            self.skills[sid].update(body)
            return 201, self.skills[sid]
        if m := re.fullmatch(r"/skills/(\w+)", p):
            sid = m.group(1)
            if method == "PUT":
                self.skills[sid].update(body)
                return 200, self.skills[sid]
            if method == "DELETE":
                self.skills.pop(sid)
                return 204, None
        if m := re.fullmatch(r"/skills/(\w+)/resources", p):
            sid = m.group(1)
            if method == "GET":
                return 200, {"resources": [{"path": k} for k in self.resources[sid]]}
            if body["path"] in self.resources[sid]:
                return 409, {"error": "Resource path already exists"}
            self.resources[sid][body["path"]] = body["content"]
            return 201, {"path": body["path"]}
        if m := re.fullmatch(r"/skills/(\w+)/resources/(.+)", p):
            sid, rp = m.group(1), m.group(2)
            if method == "GET":
                return 200, {"path": rp, "content": self.resources[sid][rp]}
            self.resources[sid].pop(rp)
            return 204, None
        if m := re.fullmatch(r"/settings/([\w.]+)", p):
            self.settings[m.group(1)] = body["value"]
            return 200, {"key": m.group(1)}
        if p == "/mcp-servers/import":
            self.mcp_imports.append(body["config"])
            return 200, {"imported": len(body["config"]["mcpServers"])}
        return 404, {"error": f"no route {method} {p}"}


@pytest.fixture
def ts():
    fake = FakeTurnstone()

    class H(BaseHTTPRequestHandler):
        def _go(self) -> None:
            if self.headers.get("Authorization") != f"Bearer {TOKEN}":
                self._send(401, {"error": "unauthorized"})
                return
            n = int(self.headers.get("Content-Length") or 0)
            body = json.loads(self.rfile.read(n)) if n else None
            code, data = fake.handle(self.command, self.path, body)
            self._send(code, data)

        def _send(self, code: int, data: Any) -> None:
            raw = json.dumps(data).encode() if data is not None else b""
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        do_GET = do_POST = do_PUT = do_DELETE = _go

        def log_message(self, *a: Any) -> None:
            pass

    srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    fake.url = f"http://127.0.0.1:{srv.server_address[1]}"  # type: ignore[attr-defined]
    yield fake
    srv.shutdown()


@pytest.fixture
def token_file(tmp_path: Path) -> Path:
    f = tmp_path / "ts.token"
    f.write_text(TOKEN + "\n")
    f.chmod(0o600)
    return f


def make_stage(root: Path, skills: dict[str, dict[str, str | bytes]]) -> Path:
    root.mkdir(parents=True, exist_ok=True)
    for name, files in skills.items():
        for rel, content in files.items():
            p = root / "skills" / name / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(content if isinstance(content, bytes) else content.encode())
    return root


def md(name: str, body: str = "Do the thing.", extra: str = "") -> str:
    return f"---\nname: {name}\ndescription: {name} helper. Use when asked.\n{extra}---\n\n{body}\n"


def push(ts, token_file, stage, *extra: str) -> int:
    return int(deliver.main(["push", "--stage", str(stage), "--console", ts.url, "--token-file", str(token_file), *extra]))


def test_creates_hub_owned_skills_with_resources_and_never_auto_approve(ts, token_file, tmp_path, capsys) -> None:
    stage = make_stage(tmp_path / "stage", {"alpha": {"SKILL.md": md("alpha", extra="effort: high\n"),
                                                      "refs/notes.md": "notes"},
                                            "beta": {"SKILL.md": md("beta", extra="user-invocable: false\n")}})
    assert push(ts, token_file, stage) == 0
    a, b = ts.by_name("alpha"), ts.by_name("beta")
    for s in (a, b):
        assert s["author"] == "agent-hub" and json.loads(s["tags"]) == ["agent-hub"]
        assert s["auto_approve"] is False and s["is_default"] is False and "model" not in s or s.get("model") in ("", None)
    assert a["reasoning_effort"] == "high" and b["hidden_from_menu"] is True and "hidden_from_menu" not in a
    assert ts.resources[a["template_id"]] == {"refs/notes.md": "notes"}
    out = capsys.readouterr()
    assert TOKEN not in out.out + out.err and "create   alpha" in out.out


def test_second_push_is_a_no_op_and_an_edit_is_an_update(ts, token_file, tmp_path, capsys) -> None:
    stage = make_stage(tmp_path / "stage", {"alpha": {"SKILL.md": md("alpha")}})
    assert push(ts, token_file, stage) == 0
    ts.calls.clear()
    assert push(ts, token_file, stage) == 0
    assert not any(m in ("POST", "PUT", "DELETE") and p != "/v1/api/admin/skills/parse" for m, p in ts.calls)
    make_stage(stage, {"alpha": {"SKILL.md": md("alpha", body="Do it differently.")}})
    assert push(ts, token_file, stage) == 0
    assert ts.by_name("alpha")["content"] == "Do it differently." and "update   alpha" in capsys.readouterr().out


def test_a_skill_the_hub_did_not_deliver_is_a_conflict_and_untouched(ts, token_file, tmp_path, capsys) -> None:
    sid = ts.add("alpha", description="mine", content="my own", author="someone")
    stage = make_stage(tmp_path / "stage", {"alpha": {"SKILL.md": md("alpha")}, "beta": {"SKILL.md": md("beta")}})
    assert push(ts, token_file, stage) == 2
    assert ts.skills[sid]["content"] == "my own" and ts.by_name("beta")["author"] == "agent-hub"
    assert "CONFLICT alpha" in capsys.readouterr().out


def test_existing_hub_skill_with_auto_approve_is_reset(ts, token_file, tmp_path) -> None:
    stage = make_stage(tmp_path / "stage", {"alpha": {"SKILL.md": md("alpha")}})
    push(ts, token_file, stage)
    ts.by_name("alpha")["auto_approve"] = True                          # someone flipped it in the console
    assert push(ts, token_file, stage) == 0
    assert ts.by_name("alpha")["auto_approve"] is False


def test_prune_removes_only_hub_owned_skills_missing_from_the_stage(ts, token_file, tmp_path) -> None:
    stage = make_stage(tmp_path / "stage", {"alpha": {"SKILL.md": md("alpha")}, "beta": {"SKILL.md": md("beta")}})
    push(ts, token_file, stage)
    theirs = ts.add("theirs", author="someone")
    import shutil
    shutil.rmtree(stage / "skills" / "beta")
    assert push(ts, token_file, stage) == 0
    assert {s["name"] for s in ts.skills.values()} == {"alpha", "beta", "theirs"}      # no --prune: kept
    assert push(ts, token_file, stage, "--prune") == 0
    assert {s["name"] for s in ts.skills.values()} == {"alpha", "theirs"} and theirs in ts.skills


def test_resources_are_synced_and_binary_or_huge_ones_skipped(ts, token_file, tmp_path, capsys) -> None:
    stage = make_stage(tmp_path / "stage", {"alpha": {"SKILL.md": md("alpha"), "a.md": "one", "b.md": "two",
                                                      "logo.png": b"\x89PNG\x00\xff"}})
    push(ts, token_file, stage)
    sid = ts.by_name("alpha")["template_id"]
    assert ts.resources[sid] == {"a.md": "one", "b.md": "two"}
    (stage / "skills/alpha/b.md").unlink()
    (stage / "skills/alpha/a.md").write_text("ONE")
    push(ts, token_file, stage)
    assert ts.resources[sid] == {"a.md": "ONE"}
    assert "not UTF-8" in capsys.readouterr().out


def test_dry_run_changes_nothing(ts, token_file, tmp_path, capsys) -> None:
    stage = make_stage(tmp_path / "stage", {"alpha": {"SKILL.md": md("alpha"), "x.md": "x"}})
    (stage / "instructions.md").write_text("Be careful.\n")
    assert push(ts, token_file, stage, "--dry-run", "--instructions", "--prune") == 0
    assert ts.skills == {} and ts.settings == {} and "DRY RUN" in capsys.readouterr().out


def test_instructions_and_mcp_only_with_their_flags(ts, token_file, tmp_path) -> None:
    stage = make_stage(tmp_path / "stage", {})
    (stage / "instructions.md").write_text("Be careful.\n")
    (stage / "mcp.json").write_text(json.dumps({"mcpServers": {"docs": {"type": "streamable-http", "url": "http://x/mcp"}}}))
    assert push(ts, token_file, stage) == 0
    assert ts.settings == {} and ts.mcp_imports == []
    assert push(ts, token_file, stage, "--instructions", "--mcp") == 0
    assert ts.settings == {"session.instructions": "Be careful.\n"} and ts.mcp_imports[0]["mcpServers"]["docs"]["url"] == "http://x/mcp"


def test_missing_flagged_files_and_bad_skills_are_errors(ts, token_file, tmp_path, capsys) -> None:
    empty = make_stage(tmp_path / "empty", {})
    assert push(ts, token_file, empty, "--instructions") == 1
    assert "--instructions given but" in capsys.readouterr().err
    renamed = make_stage(tmp_path / "renamed", {"alpha": {"SKILL.md": md("other-name")}})
    assert push(ts, token_file, renamed) == 1
    big = make_stage(tmp_path / "big", {"alpha": {"SKILL.md": md("alpha", body="x" * 40000)}})
    assert push(ts, token_file, big) == 1
    assert "directory name must match" in capsys.readouterr().err and ts.skills == {}


@pytest.mark.parametrize("mode", [0o644, 0o640])
def test_a_token_file_others_can_read_is_refused(ts, token_file, tmp_path, capsys, mode) -> None:
    token_file.chmod(mode)
    assert push(ts, token_file, make_stage(tmp_path / "s", {"alpha": {"SKILL.md": md("alpha")}})) == 1
    err = capsys.readouterr().err
    assert "readable by others" in err and TOKEN not in err and ts.calls == []


def test_wrong_token_and_unreachable_console_fail_cleanly(ts, tmp_path, capsys) -> None:
    bad = tmp_path / "bad.token"
    bad.write_text("ts_wrong\n")
    bad.chmod(0o600)
    stage = make_stage(tmp_path / "s", {"alpha": {"SKILL.md": md("alpha")}})
    assert push(ts, bad, stage) == 1 and "HTTP 401" in capsys.readouterr().err
    assert deliver.main(["list", "--console", "http://127.0.0.1:9", "--token-file", str(bad)]) == 1
    assert "ts_wrong" not in capsys.readouterr().err


def test_list_reports_all_and_hub_owned_names(ts, token_file, tmp_path, capsys) -> None:
    push(ts, token_file, make_stage(tmp_path / "s", {"alpha": {"SKILL.md": md("alpha")}}))
    ts.add("theirs", author="someone")
    capsys.readouterr()
    assert deliver.main(["list", "--console", ts.url, "--token-file", str(token_file)]) == 0
    assert json.loads(capsys.readouterr().out) == {"skills": ["alpha", "theirs"], "hub_skills": ["alpha"], "unsafe": []}


def test_allowed_tools_is_never_delivered(ts, token_file, tmp_path) -> None:
    push(ts, token_file, make_stage(tmp_path / "s", {"alpha": {"SKILL.md": md("alpha", extra="allowed-tools: bash,edit_file\n")}}))
    assert ts.by_name("alpha")["allowed_tools"] == "[]"


def test_a_hub_skill_switched_to_default_is_reset_and_listed_as_unsafe(ts, token_file, tmp_path, capsys) -> None:
    stage = make_stage(tmp_path / "s", {"alpha": {"SKILL.md": md("alpha")}})
    push(ts, token_file, stage)
    ts.by_name("alpha")["is_default"] = True
    capsys.readouterr()
    assert deliver.main(["list", "--console", ts.url, "--token-file", str(token_file)]) == 0
    assert json.loads(capsys.readouterr().out)["unsafe"] == ["alpha"]
    assert push(ts, token_file, stage) == 0 and ts.by_name("alpha")["is_default"] is False
    assert "update   alpha (is_default)" in capsys.readouterr().out


def test_prune_refuses_an_empty_stage_unless_forced(ts, token_file, tmp_path, capsys) -> None:
    push(ts, token_file, make_stage(tmp_path / "s", {"alpha": {"SKILL.md": md("alpha")}}))
    empty = make_stage(tmp_path / "empty", {})
    assert push(ts, token_file, empty, "--prune") == 1 and ts.by_name("alpha")
    assert "would delete every hub skill" in capsys.readouterr().err
    assert push(ts, token_file, empty, "--prune", "--force-empty") == 0 and ts.skills == {}


def test_a_redirect_is_refused_and_the_token_never_follows_it(token_file, tmp_path, capsys) -> None:
    seen: list[str | None] = []

    class Catch(BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            seen.append(self.headers.get("Authorization"))
            self.send_response(200)
            self.end_headers()

        def log_message(self, *a: Any) -> None:
            pass

    catcher = ThreadingHTTPServer(("127.0.0.1", 0), Catch)
    threading.Thread(target=catcher.serve_forever, daemon=True).start()

    class Redirect(BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            self.send_response(302)
            self.send_header("Location", f"http://127.0.0.1:{catcher.server_address[1]}/steal")
            self.end_headers()

        def log_message(self, *a: Any) -> None:
            pass

    redir = ThreadingHTTPServer(("127.0.0.1", 0), Redirect)
    threading.Thread(target=redir.serve_forever, daemon=True).start()
    try:
        rc = deliver.main(["list", "--console", f"http://127.0.0.1:{redir.server_address[1]}", "--token-file", str(token_file)])
    finally:
        redir.shutdown()
        catcher.shutdown()
    assert rc == 1 and seen == [] and "redirect refused" in capsys.readouterr().err


@pytest.mark.parametrize(("console", "fragment"), [
    ("http://example.com", "https is required"),
    ("https://example.com", "--allow-remote-host example.com"),
    ("http://user:pw@127.0.0.1:1", "bare base URL"),
    ("http://127.0.0.1:1/path", "bare base URL"),
    ("ftp://127.0.0.1", "http(s)"),
])
def test_consoles_that_could_leak_the_token_are_refused(token_file, capsys, console, fragment) -> None:
    assert deliver.main(["list", "--console", console, "--token-file", str(token_file)]) == 1
    assert fragment in capsys.readouterr().err


@pytest.mark.parametrize("content", [TOKEN + "\nsecond line", "two words " + TOKEN, "short", "x" * 600])
def test_a_token_file_that_is_not_one_token_is_refused_without_echo(ts, tmp_path, capsys, content) -> None:
    f = tmp_path / "t.token"
    f.write_text(content)
    f.chmod(0o600)
    assert deliver.main(["list", "--console", ts.url, "--token-file", str(f)]) == 1
    err = capsys.readouterr().err
    assert "does not hold a single API token" in err and TOKEN not in err and "second line" not in err and ts.calls == []


def test_symlinked_or_malformed_stage_files_are_clean_errors(ts, token_file, tmp_path, capsys) -> None:
    stage = make_stage(tmp_path / "s", {})
    (tmp_path / "secret.txt").write_text("not for turnstone")
    (stage / "instructions.md").symlink_to(tmp_path / "secret.txt")
    assert push(ts, token_file, stage, "--instructions") == 1 and "symlink" in capsys.readouterr().err
    (stage / "mcp.json").write_text("{not json")
    assert push(ts, token_file, stage, "--mcp") == 1 and "not valid JSON" in capsys.readouterr().err
    bad = make_stage(tmp_path / "b", {"alpha": {"SKILL.md": b"---\nname: alpha\n---\n\xff\xfe"}})
    assert push(ts, token_file, bad) == 1 and "not UTF-8" in capsys.readouterr().err and ts.settings == {}
