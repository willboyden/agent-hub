#!/usr/bin/env python3
"""Deliver a hub-rendered Turnstone stage into a running Turnstone console, through its admin API.

NEVER run by the hub or by any other script. You run it by hand, after reviewing the stage directory the
hub rendered (Changes > plan > apply writes the stage; this pushes it). Standard library only.

  turnstone-deliver.py push --stage DIR [--console URL] [--token-file PATH] [--allow-remote-host HOST]
                            [--instructions] [--mcp] [--prune [--force-empty]] [--dry-run]
  turnstone-deliver.py list [--console URL] [--token-file PATH] [--allow-remote-host HOST]

push
  skills        every DIR/skills/<name>/SKILL.md (+ resource files): parsed by Turnstone, then created or
                updated. Hub-delivered skills carry author "agent-hub" and the tag "agent-hub"; a skill of the
                same name that is NOT hub-owned is a conflict and is left alone. auto_approve and is_default are
                always false and allowed-tools is never delivered, whatever the SKILL.md says. DIR/skills-meta.json,
                when the hub rendered one, labels skills: its `category` is set on the skill, its `tags` are
                added after "agent-hub". A skill it does not list keeps the category Turnstone has.
  --instructions  DIR/instructions.md -> the session.instructions setting (new workstreams pick it up)
  --mcp           DIR/mcp.json ({"mcpServers": {...}}) -> the MCP import endpoint (skips names that exist)
  --prune         delete hub-owned skills that are no longer in the stage (refused for an empty stage unless
                  --force-empty: a wrong --stage must not wipe every hub skill)
  --dry-run       show what would change, change nothing
list    prints JSON {"skills": [...], "hub_skills": [...], "unsafe": [hub skills with auto_approve or is_default on]}
        (used by the adapter's verify)

The console must be loopback (plain http is allowed there); any other host needs https AND
--allow-remote-host naming it. No proxy is used and redirects are refused, so the token only ever goes to
the host you named. The token is read from --token-file (default ~/.config/agent-hub/turnstone.token): a
regular file you own, mode 0600, holding one token. It is never printed and never passed on a command line.
Exit codes: 0 ok, 1 error, 2 conflicts (nothing else failed).
"""
from __future__ import annotations

import argparse
import ipaddress
import json
import os
import re
import stat
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

HUB_AUTHOR = "agent-hub"
HUB_TAG = "agent-hub"
MAX_SKILL_BYTES = 32 * 1024            # Turnstone 1.8's cap for SKILL.md (parse) and skill content
# Turnstone 1.8.4's resource rules (console/server.py admin_create_skill_resource); it refuses anything else with a
# 400, so a resource outside them is skipped with a notice instead of aborting the push halfway.
RESOURCE_DIRS = ("scripts/", "references/", "assets/")
MAX_RESOURCE_CHARS = 100 * 1024        # checked on the decoded text, as Turnstone does
MAX_RESOURCES_PER_SKILL = 10
NAME_OK = set("abcdefghijklmnopqrstuvwxyz0123456789-_.")
META_FILE = "skills-meta.json"         # written by the hub's Turnstone adapter: {"<skill>": {"category", "tags"}}
MAX_LABEL = 64                         # Turnstone keeps at most 64 characters of a category
MAX_TAGS = 20
MAX_META_BYTES = 1024 * 1024
TOKEN_RE = re.compile(r"[A-Za-z0-9._~+/=-]{8,512}")
# Fields copied from Turnstone's own parse of the SKILL.md (a whitelist; anything else it returns is ignored:
# `model` because it names a client's model, `allowed_tools` because together with auto_approve it lets tools
# run unasked). `effort` and `user_invocable` are mapped below; auto_approve/is_default are forced off.
PARSED_FIELDS = ("description", "content", "license", "compatibility", "paths", "arguments", "argument_hint")
LIST_FIELDS = ("paths", "arguments", "tags")


class DeliverError(Exception):
    pass


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args: object, **kwargs: object) -> None:   # type: ignore[override]
        return None                     # urllib then raises HTTPError(3xx): the token never follows a redirect


def check_console(url: str, allow_remote: str | None) -> str:
    try:
        u = urllib.parse.urlsplit(url)
        host, _port = (u.hostname or ""), u.port
    except ValueError:
        raise DeliverError(f"--console {url!r} is not a valid URL") from None
    if u.scheme not in ("http", "https") or not host:
        raise DeliverError("--console must be an http(s) URL with a host")
    if u.username or u.password or u.path not in ("", "/") or u.query or u.fragment:
        raise DeliverError("--console must be a bare base URL (no credentials, path, query or fragment)")
    loopback = host == "localhost"
    if not loopback:
        try:
            loopback = ipaddress.ip_address(host).is_loopback
        except ValueError:
            loopback = False
    if not loopback:
        if u.scheme != "https":
            raise DeliverError(f"--console {host} is not loopback: https is required to send the token there")
        if allow_remote != host:
            raise DeliverError(f"--console {host} is not loopback: pass --allow-remote-host {host} to send the token there")
    return f"{u.scheme}://{u.netloc}"


class Api:
    def __init__(self, console: str, token: str, timeout: float = 30) -> None:
        self.base = console.rstrip("/") + "/v1/api/admin"
        self.token = token
        self.timeout = timeout
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), _NoRedirect())

    def call(self, method: str, path: str, body: object = None) -> tuple[int, object]:
        req = urllib.request.Request(self.base + path, method=method,
                                     data=json.dumps(body).encode() if body is not None else None)
        req.add_header("Content-Type", "application/json")
        req.add_header("Authorization", f"Bearer {self.token}")
        try:
            with self.opener.open(req, timeout=self.timeout) as r:
                raw, status = r.read(), r.status
        except urllib.error.HTTPError as e:
            raw, status = e.read(), e.code
        except (urllib.error.URLError, OSError, ValueError) as e:      # ValueError: never echo (could carry the header)
            raise DeliverError(f"{method} {path}: request failed ({type(e).__name__})") from None
        if 300 <= status < 400:
            raise DeliverError(f"{method} {path}: HTTP {status} redirect refused (the token never follows a redirect)")
        try:
            return status, (json.loads(raw) if raw else None)
        except ValueError:
            return status, raw.decode(errors="replace")[:300]

    def ok(self, method: str, path: str, body: object = None, want: tuple[int, ...] = (200, 201, 204)) -> object:
        status, data = self.call(method, path, body)
        if status not in want:
            detail = data.get("error", data) if isinstance(data, dict) else data
            raise DeliverError(f"{method} {path} -> HTTP {status}: {str(detail)[:300]}")
        return data


def read_token(path: Path) -> str:
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    except FileNotFoundError:
        raise DeliverError(f"token file {path} not found") from None
    except OSError as e:
        raise DeliverError(f"token file {path} cannot be opened ({type(e).__name__}; a symlink is refused)") from None
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode) or st.st_uid != os.geteuid():
            raise DeliverError(f"token file {path} is not a regular file you own")
        if st.st_mode & 0o077:
            raise DeliverError(f"token file {path} is readable by others (mode {stat.S_IMODE(st.st_mode):o}); chmod 600 it")
        raw = os.read(fd, 1024)
    finally:
        os.close(fd)
    token = raw.decode("ascii", errors="replace").strip()
    if not TOKEN_RE.fullmatch(token):
        raise DeliverError(f"token file {path} does not hold a single API token")      # content never echoed
    return token


def say(line: str) -> None:
    print(line, flush=True)            # as it happens: an abort later still shows what already changed


def tags_of(skill: dict) -> list[str]:
    raw = skill.get("tags") or "[]"
    try:
        val = json.loads(raw) if isinstance(raw, str) else raw
    except ValueError:
        return []
    return [str(t) for t in val] if isinstance(val, list) else []


def hub_owned(skill: dict) -> bool:
    return skill.get("author") == HUB_AUTHOR and HUB_TAG in tags_of(skill)


def unsafe(skill: dict) -> bool:
    return bool(skill.get("auto_approve")) or bool(skill.get("is_default"))


def sid_of(skill: dict) -> str:
    return urllib.parse.quote(str(skill.get("template_id") or skill.get("id") or ""), safe="")


def list_skills(api: Api) -> dict[str, dict]:
    data = api.ok("GET", "/skills?limit=10000")
    rows = data.get("skills", []) if isinstance(data, dict) else []
    return {str(r["name"]): r for r in rows if isinstance(r, dict) and "name" in r}


def stage_skills(stage: Path) -> dict[str, dict[str, bytes]]:
    top = stage / "skills"
    out: dict[str, dict[str, bytes]] = {}
    if top.is_symlink():
        raise DeliverError(f"{top} is a symlink")
    if not top.is_dir():
        return out
    for d in sorted(top.iterdir()):
        if d.is_symlink():
            raise DeliverError(f"{d}: symlinks are not delivered")
        if not d.is_dir():
            continue
        if not d.name or set(d.name) - NAME_OK:
            raise DeliverError(f"stage skill directory {d.name!r} has an invalid name")
        files: dict[str, bytes] = {}
        for f in sorted(d.rglob("*")):
            if f.is_symlink():
                raise DeliverError(f"{f}: symlinks are not delivered")
            if f.is_file():
                rel = f.relative_to(d).as_posix()
                if rel.endswith(".pyc") or "/__pycache__/" in f"/{rel}":
                    continue
                files[rel] = f.read_bytes()
        if "SKILL.md" not in files:
            raise DeliverError(f"stage skill {d.name!r} has no SKILL.md")
        out[d.name] = files
    return out


def _label(value: object, where: str) -> str:
    if not isinstance(value, str) or not value or len(value) > MAX_LABEL or set(value) - NAME_OK:
        raise DeliverError(f"{where}: {value!r} is not a valid label (lowercase letters, digits, - _ ., at most {MAX_LABEL})")
    return value


def stage_meta(stage: Path, skills: dict[str, dict[str, bytes]]) -> dict[str, dict]:
    """The labels in DIR/skills-meta.json, checked; {} when the hub rendered none. Refused (nothing pushed) if the file
    is not exactly what the adapter writes: an object of staged skill names to {"category"?: str, "tags"?: [str]}."""
    f = stage / META_FILE
    if f.is_symlink():
        raise DeliverError(f"{f} is a symlink; refused")
    if not f.exists():
        return {}
    if not f.is_file() or f.stat().st_size > MAX_META_BYTES:
        raise DeliverError(f"{f} is not a regular file of at most {MAX_META_BYTES} bytes; refused")
    try:
        doc = json.loads(f.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, ValueError, RecursionError) as e:
        raise DeliverError(f"{f}: not valid JSON ({type(e).__name__})") from None
    if not isinstance(doc, dict):
        raise DeliverError(f"{f}: expected an object of skill name -> labels")
    out: dict[str, dict] = {}
    for name, entry in doc.items():
        if name not in skills:
            raise DeliverError(f"{f}: {name!r} is not a staged skill")
        if not isinstance(entry, dict) or not entry or set(entry) - {"category", "tags"}:
            raise DeliverError(f"{f}: {name}: expected {{'category'?: ..., 'tags'?: [...]}}")
        labels: dict = {}
        if "category" in entry:
            labels["category"] = _label(entry["category"], f"{name} category")
        if "tags" in entry:
            tags = entry["tags"]
            if not isinstance(tags, list) or len(tags) > MAX_TAGS:
                raise DeliverError(f"{f}: {name}: tags must be a list of at most {MAX_TAGS}")
            labels["tags"] = [_label(t, f"{name} tag") for t in tags]
        out[name] = labels
    return out


def stage_file(stage: Path, name: str, flag: str) -> str:
    f = stage / name
    if f.is_symlink():
        raise DeliverError(f"{f} is a symlink; refused")
    if not f.is_file():
        raise DeliverError(f"{flag} given but {f} does not exist (is that concern managed?)")
    try:
        return f.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        raise DeliverError(f"{f} is not UTF-8 text") from None


def desired_body(api: Api, name: str, skill_md: bytes, labels: dict | None = None) -> dict:
    if len(skill_md) > MAX_SKILL_BYTES:
        raise DeliverError(f"{name}: SKILL.md is {len(skill_md)} bytes; Turnstone accepts at most {MAX_SKILL_BYTES}")
    try:
        raw = skill_md.decode("utf-8")
    except UnicodeDecodeError:
        raise DeliverError(f"{name}: SKILL.md is not UTF-8") from None
    fields = api.ok("POST", "/skills/parse", {"raw": raw}, want=(200,))
    if not isinstance(fields, dict):
        raise DeliverError(f"{name}: unexpected parse response")
    if fields.get("name") not in (None, "", name):
        raise DeliverError(f"{name}: SKILL.md declares name {fields.get('name')!r}; the directory name must match")
    body = {k: fields[k] for k in PARSED_FIELDS if k in fields and fields[k] is not None}
    if fields.get("effort"):
        body["reasoning_effort"] = fields["effort"]
    if fields.get("user_invocable") is False:
        body["hidden_from_menu"] = True
    for k in LIST_FIELDS:                                         # Turnstone accepts JSON strings for list fields
        if isinstance(body.get(k), list):
            body[k] = json.dumps(body[k])
    labels = labels or {}
    tags = [HUB_TAG] + [t for t in dict.fromkeys(labels.get("tags", [])) if t != HUB_TAG]   # hub tag first, no repeats
    body.update({"name": name, "author": HUB_AUTHOR, "tags": json.dumps(tags),
                 "auto_approve": False, "is_default": False, "allowed_tools": "[]"})
    if labels.get("category"):          # otherwise no key: Turnstone's PUT changes only the fields it is sent
        body["category"] = labels["category"]
    if not body.get("description"):
        raise DeliverError(f"{name}: SKILL.md has no description")
    return body


def _norm(value: object) -> object:
    if isinstance(value, str) and value[:1] in "[{":
        try:
            return json.loads(value)
        except ValueError:
            return value
    return value


def needs_update(existing: dict, body: dict) -> list[str]:
    """Every delivered field that differs from what Turnstone reports (lists may come back as lists or JSON strings).
    Fields the list response does not include cannot be compared; auto_approve and is_default always are."""
    return sorted(k for k, v in body.items() if k != "name" and k in existing and _norm(existing[k]) != _norm(v))


def deliverable_resources(files: dict[str, bytes], name: str) -> dict[str, str]:
    """The resources Turnstone will accept, as text; every other file is reported as SKIPPED with the reason.

    Past the per-skill cap, files SKILL.md mentions by path come first, then the rest, each in path order."""
    skill_md = files.get("SKILL.md", b"").decode("utf-8", errors="replace")
    ok: dict[str, str] = {}
    for path, raw in sorted(files.items()):
        if path == "SKILL.md":
            continue
        if not path.startswith(RESOURCE_DIRS):
            say(f"  resource SKIPPED {name}/{path}: Turnstone accepts resources only under "
                f"{', '.join(RESOURCE_DIRS)}")
            continue
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError:
            say(f"  resource SKIPPED {name}/{path}: not UTF-8 text (binary resources are not delivered)")
            continue
        if len(text) > MAX_RESOURCE_CHARS:
            say(f"  resource SKIPPED {name}/{path}: {len(text)} characters is over {MAX_RESOURCE_CHARS}")
            continue
        ok[path] = text
    order = sorted(ok, key=lambda p: (p not in skill_md, p))
    for path in order[MAX_RESOURCES_PER_SKILL:]:
        say(f"  resource SKIPPED {name}/{path}: over Turnstone's {MAX_RESOURCES_PER_SKILL} resources per skill")
    return {p: ok[p] for p in order[:MAX_RESOURCES_PER_SKILL]}


def sync_resources(api: Api, sid: str, files: dict[str, bytes], dry: bool, name: str) -> None:
    want = deliverable_resources(files, name)
    have: dict[str, dict] = {}
    if sid:
        data = api.ok("GET", f"/skills/{sid}/resources", want=(200,))
        rows = data.get("resources", []) if isinstance(data, dict) else data if isinstance(data, list) else []
        have = {str(r.get("path")): r for r in rows if isinstance(r, dict)}
    for path in sorted(set(have) - set(want)):
        say(f"  resource remove {name}/{path}")
        if not dry:
            api.ok("DELETE", f"/skills/{sid}/resources/{urllib.parse.quote(path)}")
    for path, text in sorted(want.items()):
        if path in have:
            cur = api.ok("GET", f"/skills/{sid}/resources/{urllib.parse.quote(path)}", want=(200,))
            if isinstance(cur, dict) and cur.get("content") == text:
                continue
            say(f"  resource update {name}/{path}")
            if not dry:                 # delete + add; an interruption here is fixed by the next push
                api.ok("DELETE", f"/skills/{sid}/resources/{urllib.parse.quote(path)}")
        else:
            say(f"  resource add {name}/{path}")
        if not dry:
            api.ok("POST", f"/skills/{sid}/resources", {"path": path, "content": text,
                                                        "content_type": "text/markdown" if path.endswith(".md") else "text/plain"})


def push(api: Api, stage: Path, *, instructions: bool, mcp: bool, prune: bool, force_empty: bool, dry: bool) -> int:
    if stage.is_symlink() or not stage.is_dir():
        raise DeliverError(f"stage {stage} is not a directory")
    wanted = stage_skills(stage)
    meta = stage_meta(stage, wanted)
    if prune and not wanted and not force_empty:
        raise DeliverError("--prune with no skills in the stage would delete every hub skill; "
                           "check --stage, or add --force-empty if that is what you want")
    say(("DRY RUN: " if dry else "") + f"{len(wanted)} staged skill(s)")
    conflicts = 0
    existing = list_skills(api)
    for name, files in wanted.items():
        body = desired_body(api, name, files["SKILL.md"], meta.get(name))
        cur = existing.get(name)
        if cur is not None and not hub_owned(cur):
            conflicts += 1
            say(f"CONFLICT {name}: a skill with this name exists and was not delivered by the hub; left alone")
            continue
        if cur is None:
            say(f"create   {name}")
            sid = ""
            if not dry:
                created = api.ok("POST", "/skills", body)
                sid = sid_of(created) if isinstance(created, dict) else ""
                if not sid:
                    raise DeliverError(f"{name}: create returned no id")
            sync_resources(api, sid, files, dry, name)
            continue
        sid = sid_of(cur)
        changed = needs_update(cur, body)
        if changed:
            say(f"update   {name} ({', '.join(changed)})")
            if not dry:
                api.ok("PUT", f"/skills/{sid}", body)
        else:
            say(f"same     {name}")
        sync_resources(api, sid, files, dry, name)
    if prune:
        for name, cur in sorted(existing.items()):
            if name not in wanted and hub_owned(cur):
                say(f"remove   {name} (hub-owned, no longer in the stage)")
                if not dry:
                    api.ok("DELETE", f"/skills/{sid_of(cur)}")
    if instructions:
        text = stage_file(stage, "instructions.md", "--instructions")
        say(f"setting  session.instructions ({len(text.encode())} bytes)")
        if not dry:
            api.ok("PUT", "/settings/session.instructions", {"value": text})
    if mcp:
        try:
            config = json.loads(stage_file(stage, "mcp.json", "--mcp"))
        except ValueError:
            raise DeliverError("mcp.json is not valid JSON") from None
        if not isinstance(config, dict) or not isinstance(config.get("mcpServers"), dict):
            raise DeliverError('mcp.json must be {"mcpServers": {...}}')
        say(f"mcp      import {len(config['mcpServers'])} server(s) (existing names are skipped)")
        if not dry:
            api.ok("POST", "/mcp-servers/import", {"config": config})
    say("done" + (" (with conflicts)" if conflicts else ""))
    return 2 if conflicts else 0


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="turnstone-deliver.py", description=__doc__.split("\n\n")[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    for name in ("push", "list"):
        sp = sub.add_parser(name)
        sp.add_argument("--console", default="http://127.0.0.1:8796")
        sp.add_argument("--token-file", default=str(Path.home() / ".config" / "agent-hub" / "turnstone.token"))
        sp.add_argument("--allow-remote-host", default=None, metavar="HOST")
        if name == "push":
            sp.add_argument("--stage", required=True)
            sp.add_argument("--instructions", action="store_true")
            sp.add_argument("--mcp", action="store_true")
            sp.add_argument("--prune", action="store_true")
            sp.add_argument("--force-empty", action="store_true")
            sp.add_argument("--dry-run", action="store_true")
    a = p.parse_args(argv)
    try:
        console = check_console(a.console, a.allow_remote_host)
        api = Api(console, read_token(Path(os.path.expanduser(a.token_file))))
        if a.cmd == "list":
            skills = list_skills(api)
            hub = {n: s for n, s in skills.items() if hub_owned(s)}
            print(json.dumps({"skills": sorted(skills), "hub_skills": sorted(hub),
                              "unsafe": sorted(n for n, s in hub.items() if unsafe(s))}))
            return 0
        return push(api, Path(os.path.expanduser(a.stage)), instructions=a.instructions, mcp=a.mcp,
                    prune=a.prune, force_empty=a.force_empty, dry=a.dry_run)
    except DeliverError as exc:
        print(f"turnstone-deliver: {exc}", file=sys.stderr)
        return 1
    except Exception as exc:  # noqa: BLE001 - print the type only: a message could carry request data
        print(f"turnstone-deliver: unexpected {type(exc).__name__}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
