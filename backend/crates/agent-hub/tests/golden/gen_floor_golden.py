"""Golden decisions of the policy-safety code (services/floor.py, targets.py, contentguard.py, doctor.py), computed by
the PYTHON implementation on a large seeded set of random and hand-picked inputs. tests/test_floor.rs replays every
case against the Rust port and uses the table as the oracle of its proptests.

The floor depends on the home directory (`~` expansion, captured when agent_hub.services.floor is imported), so the
generator refuses to run unless HOME is the fixed golden home; the Rust test re-runs itself with the same HOME.

    env HOME=/home/goldenuser "$PYREL/backend/.venv/bin/python" -I \
        backend/crates/agent-hub/tests/golden/gen_floor_golden.py backend/crates/agent-hub/tests/golden/floor_decisions.json
    (from the repo root; PYREL=$(node backend/parity/lib/python-release.mjs) is the last Python backend, see
     backend/parity/lib/python-release.mjs)
"""
from __future__ import annotations

import json
import random
import sys
from pathlib import Path
from typing import Any

GOLDEN_HOME = "/home/goldenuser"
assert str(Path.home()) == GOLDEN_HOME, f"run with HOME={GOLDEN_HOME} (got {Path.home()})"

from agent_hub.adapters.base import McpItem, PermissionSet, Rule  # noqa: E402
from agent_hub.config import APP_ROOT  # noqa: E402
from agent_hub.services import contentguard, doctor  # noqa: E402
from agent_hub.services.floor import Floor, FloorPolicy, host_matches, load_floor, norm_path, overlaps  # noqa: E402
from agent_hub.services.targets import TargetGuard  # noqa: E402

SEED = 20261027
rng = random.Random(SEED)

KINDS = ["tool", "command", "path_read", "path_write", "egress_host", "mcp_server"]
DECISIONS = ["allow", "ask", "deny"]


# ---- floors ------------------------------------------------------------------------------------------------------------
def real_floor() -> FloorPolicy:
    p = load_floor(APP_ROOT / "policy" / "floor.yaml")
    for lst in (p.deny_path_read, p.deny_path_write):          # what Hub() appends for its data dir
        lst.append("/srv/hub-state/**")
    return p


FLOORS: dict[str, FloorPolicy] = {
    "real": real_floor(),
    "synthetic": FloorPolicy.model_validate({
        "network_default": "deny",
        "egress": {"forbid_allow_patterns": ["*"], "forbid_allow_probes": ["x.y", "Example.COM"], "loopback_hosts": []},
        "deny_path_read": ["", "~/.ssh/**", "/etc/**", "~/a/../b/**", "[ab]*/secret"],
        "deny_path_write": ["**/.git/**", "~"],
        "tools": {"shell": {"default": "deny", "max": "deny"}, "web_fetch": {"default": "ask", "max": "weird"},
                  "x": {"default": "deny", "max": "allow"}, "mcp": {"default": "deny", "max": "ask"}},
        "default_tool_decision": "deny",
        "command_allow_forbid_patterns": [" * "],
        "command_allow_probes": ["a", "A B"],
        "command_allow_warn_probes": ["git x", "ß x"],
        "mcp": {"require_scan_clean": False, "require_explicit_egress": False, "require_sandbox_stdio": False},
    }),
    "defaults": FloorPolicy(),
}

# ---- random inputs -----------------------------------------------------------------------------------------------------
PATH_SEGS = ["~", "", ".", "..", ".ssh", ".aws", "x", "**", "*", "?", "[a-z]", "[!a]", "[]", "[", ".env", "admin.key",
             "home", "goldenuser", "goldenusers", ".config", "gcloud", "agent-hub", ".local", "share", "srv", "hub-state",
             "bootstrap-admin.key", "a", "b", "secret", "id_rsa", ".netrc", "é", "*.key", ".git", "etc", "x*y", "?a"]
CMD_TOKS = ["git", "*", "?", "a", "b", "sh", "-c", "x", "docker", "compose", "up", "python", "x.py", "[a-z]*", "sudo",
            "rm", "-rf", "/", "curl", "node", "npx", "uv", "run", "status", "**", "?*", ".*", "A", "B", "ß", "env",
            "bash", "python3", "find", "[!x]*", "\t", " ", "git*", "c*l"]
HOST_TOKS = ["*", "**", "?", "a", "b", "example", "com", "io", "org", "[a-c]", "API", "Example", "COM", "x", "y", "c",
             "d", "dev", "net", "[!a]*", "é", "İ", "ſ", "K", "", "-", "1", "127", "0"]
TOOLS = ["web_fetch", "web_search", "shell", "read", "mcp", "mcp__x", "mcp__x__y", "mcp__*", "mcp__", "mcp____t", "x",
         "write", "Bash", "mcp__s?", "mcp__[ab]"]


def rand_path() -> str:
    lead = rng.choice(["", "/", "~/", "**/", "//", "~", "/home/goldenuser/", "$HOME/", "./"])
    segs = [rng.choice(PATH_SEGS) for _ in range(rng.randint(0, 5))]
    tail = rng.choice(["", "", "/", "/**", "/*"])
    return lead + "/".join(segs) + tail


def rand_cmd() -> str:
    return rng.choice(["", " ", "  "]) + " ".join(rng.choice(CMD_TOKS) for _ in range(rng.randint(0, 4))) + \
        rng.choice(["", " ", "\n"])


def rand_host() -> str:
    return ".".join(rng.choice(HOST_TOKS) for _ in range(rng.randint(1, 4)))


def rand_match(kind: str) -> str:
    if kind in ("path_read", "path_write"):
        return rand_path()
    if kind == "command":
        return rand_cmd()
    if kind == "egress_host":
        return rand_host()
    if kind == "mcp_server":
        return rng.choice(["s", "srv", "*", "s*", "", "shady", "x", "[ab]", "a?"])
    return rng.choice(TOOLS)


def rand_rule() -> Rule:
    kind = rng.choice(KINDS)
    return Rule(kind=kind, match=rand_match(kind), decision=rng.choice(DECISIONS))  # type: ignore[arg-type]


def finding(f: Any) -> Any:
    """A Finding as [code, severity, message, item, concern, rule] (compact: the table is large)."""
    return None if f is None else [f.code, f.severity, f.message, f.item, f.concern, f.rule]


def findings(fs: list[Any]) -> list[Any]:
    return [finding(f) for f in fs]


def rule_json(r: Rule) -> list[Any]:
    """A Rule as [kind, match, decision, reason]."""
    return [r.kind, r.match, r.decision, r.reason]


# ---- cases -------------------------------------------------------------------------------------------------------------
def check_rule_cases() -> list[dict[str, Any]]:
    out = []
    fixed = [("path_read", m) for m in ["~/.ssh/id_ed25519", "~/**", "**", "/home/**", "~/x/../.aws/credentials", "~//.netrc",
                                        "/home/../home/goldenuser/.gnupg/x", ".env", "**/.env", "/proj/.env", "~/src/**"]]
    fixed += [("command", m) for m in ["*", "**", "  ", "?*", "*.*.*", "*a*", "* *", "sh *", "git status*", "git *", "npm test"]]
    fixed += [("egress_host", m) for m in ["*", "**", "*.*", "*.com", "?", "api.example.com", "*.example.com", "*.io"]]
    fixed += [("tool", m) for m in ["web_fetch", "web_search", "shell", "read", "x"]]
    for floor in FLOORS:
        for kind, m in fixed:
            for dec in DECISIONS:
                out.append((floor, Rule(kind=kind, match=m, decision=dec), "permissions", "x"))  # type: ignore[arg-type]
        for _ in range(1800):
            out.append((floor, rand_rule(), rng.choice(["permissions", "mcp", "rendered"]), rng.choice([None, "r1", "check-0"])))
    return [{"floor": f, "rule": rule_json(r), "concern": c, "rid": rid,
             "finding": finding(Floor(FLOORS[f]).check_rule(r, c, rid))} for f, r, c, rid in out]


def overlap_cases() -> list[dict[str, Any]]:
    pairs = [("~/**", "~/.ssh/**"), ("~/a/**", "~/b/**"), (".env", "**/.env"), ("/proj/.env", "**/.env"), ("**", "x"),
             ("~", "~/.ssh/**"), ("[a-z]", "b"), ("", ""), ("/", "/x")]
    pairs += [(rand_path(), rand_path()) for _ in range(4000)]
    pairs += [(rand_path(), g) for g in FLOORS["real"].deny_path_read for _ in range(40)]
    return [{"a": a, "b": b, "overlaps": overlaps(a, b)} for a, b in pairs]


def norm_cases() -> list[dict[str, Any]]:
    ps = [rand_path() for _ in range(1500)] + ["~", "~/", "~x", "/a/./b", "/a//b", "/a/../..", "a/../b", "//x/../y"]
    return [{"p": p, "norm": norm_path(p)} for p in ps]


def host_cases() -> list[dict[str, Any]]:
    pairs = [(rand_host(), rand_host()) for _ in range(2000)] + [("*.Example.COM", "api.example.com"), ("İ", "i̇"),
                                                                 ("K", "k"), ("ſ", "s")]
    return [{"pattern": p, "host": h, "matches": host_matches(p, h)} for p, h in pairs]


def enforce_cases() -> list[dict[str, Any]]:
    out = []
    for i in range(400):
        floor = rng.choice(list(FLOORS))
        rules = [(f"r{j}", rand_rule()) for j in range(rng.randint(0, 8))]
        if rng.random() < 0.5:
            mcp_clean = None
        else:
            mcp_clean = {s: rng.choice([True, False]) for s in rng.sample(["s", "srv", "x", "shady", "a?"], rng.randint(0, 4))}
        fl = Floor(FLOORS[floor])
        res = fl.enforce(rules, mcp_clean)
        n_floor = len(fl.floor_rules())                     # the floor's own rules are stored once per floor
        assert [rule_json(r) for r in res.permissions.rules[:n_floor]] == [rule_json(r) for r in fl.floor_rules()]
        out.append({"floor": floor, "rules": [[rid, rule_json(r)] for rid, r in rules], "mcp_clean": mcp_clean,
                    "rules_after_floor": [rule_json(r) for r in res.permissions.rules[n_floor:]],
                    "default_tool_decision": res.permissions.default_tool_decision,
                    "network_default": res.permissions.network_default, "findings": findings(res.findings),
                    "allow_hosts": res.allow_hosts, "kept": [rule_json(r) for r in res.kept], "kept_ids": res.kept_ids,
                    "case": i})
    return out


def permission_set_cases() -> list[dict[str, Any]]:
    out = []
    for _ in range(300):
        floor = rng.choice(list(FLOORS))
        ps = PermissionSet(rules=[rand_rule() for _ in range(rng.randint(0, 6))], default_tool_decision=rng.choice(DECISIONS),
                           network_default=rng.choice(DECISIONS))  # type: ignore[arg-type]
        concern = rng.choice(["permissions", "live"])
        fl = Floor(FLOORS[floor])
        missing = fl.missing_denies(ps)
        assert all(f.code == "floor_deny_missing" and f.severity == "warn" and f.concern == "permissions" and f.item is None
                   and f.message == f"live config has no {f.rule.split(':', 1)[0]} deny covering {f.rule.split(':', 1)[1]!r}"
                   for f in missing)
        out.append({"floor": floor, "rules": [rule_json(r) for r in ps.rules], "default_tool_decision": ps.default_tool_decision,
                    "network_default": ps.network_default, "concern": concern,
                    "check": findings(fl.check_permission_set(ps, concern)),
                    "missing_rules": [f.rule for f in missing]})
    return out


SKILL_TOKS = ["Read", "Bash", "*", "mcp__*", "Bash(*)", "Bash(git status:*)", "Bash( * )", "Bash(:*)", "Bash(*:*)",
              "mcp__srv__search", "mcp__ghost__run", "mcp____x", "mcp__", "Grep", "Write", "Bash()", "Bash(*))", "mcp__srv",
              "Bash(　*　)", "Bash(\x1c*)", "mcp__a__b__c", "Edit"]


def rand_tools() -> Any:
    r = rng.random()
    if r < 0.55:
        seps = [", ", " ", ",", "\t", "  ,", " ", "\n"]
        s = ""
        for _ in range(rng.randint(0, 5)):
            s += rng.choice(SKILL_TOKS) + rng.choice(seps)
        return s.strip() if rng.random() < 0.7 else s
    if r < 0.85:
        return [rng.choice(SKILL_TOKS + ["", " ", 5, 1.5, True, None, ["x"]]) for _ in range(rng.randint(0, 5))]
    return rng.choice([5, None, {"a": 1}, True, 2.0])


def skill_cases() -> list[dict[str, Any]]:
    out = []
    for floor in ("real", "synthetic"):
        for _ in range(750):
            extra: dict[str, Any] = {}
            if rng.random() < 0.85:
                extra["allowed-tools"] = rand_tools()
            if rng.random() < 0.15:
                extra["hooks"] = rng.choice([{"PreToolUse": []}, None, []])
            if rng.random() < 0.2:
                extra["other"] = "x"
            clean = rng.choice([None, [], ["srv"], ["srv", "ghost", "a"]])
            name = rng.choice(["s", "sk", "it's", "naïve"])
            fs = Floor(FLOORS[floor]).check_skill(name, extra, set(clean) if clean is not None else None)
            out.append({"floor": floor, "name": name, "extra": extra, "clean": clean, "findings": findings(fs)})
    return out


URLS = ["https://evil.example.net/mcp", "http://127.0.0.1:9000/mcp", "http://localhost/x", "http://[::1]:8/x",
        "http://[::1/x", "http://::1]/x", "http://[1.2.3.4]/", "http://[v1.x]/", "http://[vz.x]/", "http://[fe80::1%eth0]/",
        "http://user@Example.COM:80/", "HTTP://API.EXAMPLE.COM", "http://a℀b.com/", "http://x／y/", "http:///x",
        "http://", "https://[::ffff:1.2.3.4]/", "https://[1:2:3:4:5:6:7:8]/", "https://[1:2:3:4:5:6:7::]/",
        "https://[::]/", "https://[:1]/", "https://[1::2::3]/", "https://[12345::]/", "https://[::01.2.3.4]/",
        "https://[::1.2.3.04]/", "https://a@b@c.com/", "https://h:p@[::1]:x/", "https://[::1]x/", "  http://x.com/",
        "http://x.com\t/", "http://\nx.com/", "x.com/y", "//x.com/y", "http:x.com", "http://[::1]%25/", "http://é.com/",
        "http://ﬁ.com/", "http://[v1.]/", "http://[V1.x]/", "http://[v.x]/", "https://[1:2:3:4:5:6:7:8:9]/",
        "https://[1:2:3:4:5:6:1.2.3.4]/", "https://[%x]/", "https://[::1%]/", "https://[::1%a%b]/", "https://x.com#frag@y",
        "https://x.com?q=@y", "https://x.com:80@evil.com/", "1http://x.com", "+http://x.com", "h ttp://x.com"]


def mcp_cases() -> list[dict[str, Any]]:
    out = []
    host_pool = ["api.example.com", "*.example.com", "other.com", "x.com", "evil.example.net", "127.0.0.1", "localhost",
                 "EXAMPLE.COM", "*", "b.c"]
    for floor in ("real", "synthetic", "defaults"):
        for _ in range(400):
            transport = rng.choice(["stdio", "http", "sse"])
            url = rng.choice(URLS + [None, None, ""])
            m = McpItem(name=rng.choice(["s", "srv", "it's"]), transport=transport, command=rng.choice([None, "c"]),
                        url=url, sandbox_profile=rng.choice([None, "", "srt"]),
                        egress_hosts=rng.sample(host_pool, rng.randint(0, 3)),
                        scan_status=rng.choice(["unscanned", "clean", "findings"]))  # type: ignore[arg-type]
            rules = [Rule(kind="egress_host", match=rng.choice(host_pool), decision=rng.choice(DECISIONS))  # type: ignore[arg-type]
                     for _ in range(rng.randint(0, 3))] + [rand_rule() for _ in range(rng.randint(0, 2))]
            concern = rng.choice(["mcp", "rendered"])
            try:
                res: Any = {"findings": findings(Floor(FLOORS[floor]).check_mcp(m, rules, concern))}
            except ValueError as exc:
                res = {"error": type(exc).__name__}
            out.append({"floor": floor, "mcp": m.model_dump(), "rules": [rule_json(r) for r in rules], "concern": concern, **res})
    return out


def url_cases() -> list[dict[str, Any]]:
    from urllib.parse import urlsplit
    pool = URLS + [rng.choice(["http://", "https://", "", "//"]) + rng.choice(["", "u@", "u:p@"]) +
                   rng.choice(["[", ""]) + rand_host() + rng.choice(["]", ""]) + rng.choice(["", ":80", ":x"]) +
                   rng.choice(["", "/p", "?q", "#f"]) for _ in range(1500)]
    out = []
    for u in pool:
        try:
            out.append({"url": u, "hostname": urlsplit(u).hostname or ""})
        except ValueError:
            out.append({"url": u, "error": "ValueError"})
    return out


def sensitive_cases() -> list[dict[str, Any]]:
    out = []
    rels = ["security/x.sh", "a/security/x", ".git/hooks/x", "x/.git", ".claude/settings.json", "a/.claude/settings.json",
            ".claude/settings.local.json", "AGENTS.md", "sub/AGENTS.md", "CLAUDE.md", "notes.md", "agents.md", "AGENTS.md/x",
            "", "security", "x.claude/settings.json"]
    for _ in range(1000):
        adapter = rng.choice(["claude-code", "opencode", "generic", "hermes-agent", "turnstone", "Claude-Code"])
        manage = {k: rng.choice([True, False]) for k in rng.sample(["permissions", "instructions", "skills", "mcp"], rng.randint(0, 3))}
        kind = rng.choice(["rule", "instruction", "skill", "mcp", "agent"])
        rel = rng.choice(rels)
        out.append({"adapter": adapter, "manage": manage, "kind": kind, "rel": rel,
                    "problem": TargetGuard.sensitive_target(adapter, manage, kind, rel)})
    return out


CFG_LINES = ["[core]", "[Core]", "[core \"x\"]", "[filter \"x\"]", "[ core ]", "[core]x = 1", "[core] fsmonitor = /x",
             "[include]", "[]", "[", "[ ]", "[user]", "\tname = a", "\temail = b", "\trepositoryformatversion = 0",
             "\tfilemode = true", "\tbare = false", "\tlogallrefupdates = true", "\tfsmonitor = x", "\thooksPath = y",
             "\tsshCommand = z", "# comment", "; comment", "", "   ", "\tIgnoreCase = true", "novalue", "\t= x", "[url \"a\"]",
             "\tinsteadOf = b", "[alias]", "\tst = !x", "[core]\tpager = x", "[merge \"m\"]", "\tdriver = x", "ÄÖ = 1",
             "[sec.sub]", "\tkey = a = b", "[Section With Space]"]


def config_cases() -> list[dict[str, Any]]:
    out = []
    for _ in range(1500):
        text = rng.choice(["\n", "\r\n", "\r"]).join(rng.choice(CFG_LINES) for _ in range(rng.randint(0, 8)))
        out.append({"text": text, "keys": sorted(contentguard.parse_config_keys(text))})
    return out


def sanitize_cases() -> list[dict[str, Any]]:
    """sanitize_git on real temp repos made of regular files (one non-sample hook at most, so scandir order cannot matter)."""
    import tempfile
    out = []
    for _ in range(300):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d) / "repo"
            git = root / ".git"
            (git / "hooks").mkdir(parents=True)
            (git / "info").mkdir()
            text = rng.choice(["\n", "\r\n"]).join(rng.choice(CFG_LINES) for _ in range(rng.randint(0, 8)))
            (git / "config").write_bytes(text.encode())
            hooks = rng.choice([[], ["pre-commit.sample"], ["post-checkout"], ["pre-commit.sample", "post-checkout"]])
            for h in hooks:
                (git / "hooks" / h).write_text("#!/bin/sh\n")
            attrs, ga, sub = rng.choice(["", "* diff=x\n", None]), rng.choice([None, "", "* filter=x\n"]), rng.choice([None, "", "y\n"])
            if attrs is not None:
                (git / "info" / "attributes").write_text(attrs)
            if ga is not None:
                (root / ".gitattributes").write_text(ga)
            if sub is not None:
                (root / "sub").mkdir()
                (root / "sub" / ".gitattributes").write_text(sub)
            removed = contentguard.sanitize_git(root)
            out.append({"config": text, "hooks": hooks, "attrs": attrs, "ga": ga, "sub": sub, "removed": removed,
                        "config_after": (git / "config").read_bytes().decode(),
                        "hooks_after": sorted(h.name for h in (git / "hooks").iterdir()),
                        "attrs_after": (git / "info" / "attributes").read_text() if attrs is not None else None,
                        "ga_after": (root / ".gitattributes").read_text() if ga is not None else None,
                        "sub_after": (root / "sub" / ".gitattributes").read_text() if sub is not None else None,
                        "hostile_after": contentguard.hostile_findings(root)})
    return out


def doctor_cases() -> dict[str, Any]:
    home = Path("/home/tester")
    pats = ["~/.ssh", "~/.ssh/**", "/home/tester/.aws/", "$HOME/x/*", "${HOME}/y/**", "~/.config", "**/.env", "**/.env.*",
            "~/.mozilla*", "/", "///", "~", "/home/tester", "/home/testers/x", "~/a/**/*", "*", ""]
    norm = [{"p": p, "norm": doctor._norm(p, home)} for p in pats + [rand_path() for _ in range(800)]]
    covers = []
    for _ in range(1500):
        a, b = doctor._norm(rand_path(), home), doctor._norm(rand_path(), home)
        covers.append({"pattern": a, "target": b, "covers": doctor._covers(a, b)})
    units = []
    toks = ["ReadWritePaths", "=", " ", "\t", "\n", "-/x", "+%h/y", "~/z", "/a/b", "ReadOnlyPaths", "　", "\x1c", "x",
            " ", "#", "[Service]", "%h"]
    for _ in range(700):
        text = "".join(rng.choice(toks) for _ in range(rng.randint(0, 14)))
        units.append({"text": text, "values": [m.group(1) for m in
                                               __import__("re").finditer(r"^\s*ReadWritePaths\s*=\s*(.+)$", text, __import__("re").M)]})
    caps = []
    pool = ["a", "B", "ß", "ǆ", "ǅ", "ა", "ﬁ", "Σ", "σ", "İ", "i", "ŉ", "ᾳ", " ", "1", "Then", "RUN", "é", "Ⅻ", "ǈ"]
    for _ in range(600):
        s = "".join(rng.choice(pool) for _ in range(rng.randint(0, 6)))
        caps.append({"s": s, "cap": s.capitalize()})
    js = []
    jtoks = ["{", "}", "[", "]", ":", ",", '"a"', '"b"', "1", "-0", "1.5", "1e400", "NaN", "Infinity", "-Infinity", "null",
             "true", "false", " ", "\n", '"\\u00e9"', '"\\ud83d\\ude00"', "01", "1.", ".5", "-", '"x\ty"', "﻿", "nul",
             "123456789012345678901234567890", '"\\/"', '"\\q"']
    for _ in range(1500):
        text = "".join(rng.choice(jtoks) for _ in range(rng.randint(0, 9)))
        try:
            js.append({"text": text, "repr": repr(json.loads(text))})
        except (ValueError, RecursionError):
            js.append({"text": text, "repr": None})
    gaps = []
    for _ in range(200):
        doc: Any = rng.choice([None, 5, [], {}, "x"])
        if rng.random() < 0.8:
            doc = {}
            if rng.random() < 0.7:
                doc["permissions"] = rng.choice([{"deny": [rng.choice([f"{t}({p}/**)" for t in ("Read", "Write", "Edit")
                                                                         for p in doctor.STATE_PATHS] + ["Write(**/hub/policy/**)",
                                                                                                         "Edit(x/hub/policy)", 1, None])
                                                          for _ in range(rng.randint(0, 12))]}, 5, {"deny": "x"}])
            if rng.random() < 0.7:
                doc["sandbox"] = {"credentials": {"files": [rng.choice([{"path": p, "mode": "deny"} for p in doctor.STATE_PATHS]
                                                                       + [{"path": 3}, 1, None, {"x": 1}])
                                                            for _ in range(rng.randint(0, 5))]}}
        gaps.append({"doc": doc, "gaps": doctor.claude_settings_gaps(doc)})
    return {"norm": norm, "covers": covers, "units": units, "capitalize": caps, "json": js, "gaps": gaps}


def main() -> int:
    out = {
        "_generator": "backend/crates/agent-hub/tests/golden/gen_floor_golden.py",
        "home": GOLDEN_HOME,
        "seed": SEED,
        "floors": {k: v.model_dump(mode="json") for k, v in FLOORS.items()},
        "floor_rules": {k: [rule_json(r) for r in Floor(v).floor_rules()] for k, v in FLOORS.items()},
        "check_rule": check_rule_cases(),
        "overlaps": overlap_cases(),
        "norm_path": norm_cases(),
        "host_matches": host_cases(),
        "enforce": enforce_cases(),
        "permission_set": permission_set_cases(),
        "check_skill": skill_cases(),
        "check_mcp": mcp_cases(),
        "url_hostname": url_cases(),
        "sensitive_target": sensitive_cases(),
        "config_keys": config_cases(),
        "sanitize": sanitize_cases(),
        "doctor": doctor_cases(),
    }
    Path(sys.argv[1]).write_text(json.dumps(out, ensure_ascii=False, separators=(",", ":")) + "\n")
    print({k: len(v) for k, v in out.items() if isinstance(v, list)})
    return 0


if __name__ == "__main__":
    sys.exit(main())
