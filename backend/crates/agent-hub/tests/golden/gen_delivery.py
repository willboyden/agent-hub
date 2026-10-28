"""Golden files for WS-B2's delivery modules, produced by the Python reference (backend/src/agent_hub):

  lock_fake1.json        LockStore.save() bytes of one Lock (pydantic model_dump_json(indent=2) + "\\n")
  merge.json             merge_file() results over a matrix of live files / rendered artifacts (json, block, own)
  merge_yaml.json        the same for yaml_keys (bytes depend on the PyYAML emitter: WS-Y)
  planner_unified.json   planner.unified() (difflib + scrub + cap) over live/new pairs

    "$PYREL/backend/.venv/bin/python" -I backend/crates/agent-hub/tests/golden/gen_delivery.py backend/crates/agent-hub/tests/golden
    (from the repo root; PYREL=$(node backend/parity/lib/python-release.mjs) is the last Python backend, see
     backend/parity/lib/python-release.mjs)
"""
import base64
import json
import sys
import tempfile
from pathlib import Path

from agent_hub.services.lock import Lock, LockEntry, LockStore
from agent_hub.services.merge import Desired, merge_file
from agent_hub.services.planner import unified

out = Path(sys.argv[1])

# ---- lock ------------------------------------------------------------------------------------------------------------
with tempfile.TemporaryDirectory() as td:
    st = LockStore(Path(td))
    lk = Lock(client="fake1", applied_at=1761600000.25, content_hash="abc", plan_id="plan_0123456789ab",
              verified_ok=True, verified_at=1761600001.5)
    lk.files["home:CLAUDE.md"] = LockEntry(root="home", path="CLAUDE.md", kind="instruction", sha256="s1", slice_sha256="s2",
                                           merge="block", managed_keys=[], created=True, source_ids=["style"], mode=0o644)
    st.save(lk, touch=False)
    (out / "lock_fake1.json").write_bytes((Path(td) / "fake1.json").read_bytes())


# ---- merge -----------------------------------------------------------------------------------------------------------
def b64(b):
    return None if b is None else base64.b64encode(b).decode()


def case(merge, desired, live, adopt=False, path="f"):
    ds = [Desired("home", path, "mcp", merge, [], 0o644, keys, c) for c, keys in desired]
    try:
        m = merge_file(ds, live, adopt_broken=adopt)
        res = {"new": b64(m.new), "slice_live": m.slice_live, "slice_new": m.slice_new, "unparseable": m.unparseable,
               "empty_after": m.empty_after, "block_style": m.block_style, "error": None}
    except Exception as exc:  # noqa: BLE001
        res = {"error": type(exc).__name__}
    return {"merge": merge, "path": path, "adopt": adopt, "live": b64(live),
            "desired": [{"content": b64(c), "keys": k} for c, k in desired], "result": res}


J = lambda o: json.dumps(o).encode()  # noqa: E731
cases = []
mcp = J({"mcpServers": {"srv": {"command": "x", "args": ["-v"], "url": None}}})
perms = J({"permissions": {"deny": ["path_read:~/.ssh/**"], "ask": [], "allow": ["tool:read"]}})
pkeys = ["permissions.deny", "permissions.ask", "permissions.allow"]
for live in [None, b"", b"   \n", b"null", b"{}", J({"other": {"x": 1}, "mcpServers": {}}),
             J({"mcpServers": {"srv": {"command": "x", "args": ["-v"], "url": None}}, "z": 1}),
             b'{"b": 1, "a": {"x": [1, 2.5, -0, 1e400, NaN, 123456789012345678901234567890]}, "b": 2}',
             "{\"é\": \"😀\", \"esc\": \"\\u00e9\\ud83d\\ude00\\n\"}".encode(),
             b"{ // jsonc\n}", b"[1]", b"\xff\xfe", b'{"mcpServers": 5}', b'{"a": {"b": {}}}']:
    for adopt in (False, True):
        cases.append(case("json_keys", [(mcp, ["mcpServers"])], live, adopt))
        cases.append(case("json_keys", [(None, ["mcpServers"])], live, adopt))
for live in [None, J({"theme": "dark", "permissions": {"defaultMode": "plan"}}), J({"permissions": {"deny": ["old"]}}),
             J({"permissions": {"deny": [], "ask": [], "allow": [], "keep": 1}}), J({"permissions": {"deny": []}})]:
    cases.append(case("json_keys", [(perms, pkeys)], live))
    cases.append(case("json_keys", [(None, pkeys)], live))
    cases.append(case("json_keys", [(J({"permissions": {"deny": ["x"]}}), pkeys)], live))
    cases.append(case("json_keys", [(J({"mcpServers": {}}), ["mcpServers"]),
                                    (J({"permissions": {"deny": ["x"]}}), ["permissions"])], live))
for content in [b"[1]", b"not json", b"", b"null", b'{"a": {"b": 1}}']:
    cases.append(case("json_keys", [(content, ["a.b", "a.c"])], J({"a": {"c": 3, "d": 4}})))
MD = "<!-- agent-hub:begin (managed block; edits inside are overwritten) -->"
blk = f"{MD}\n## Style\n\nBe concise.\n<!-- agent-hub:end -->\n".encode()
hblk = b"# agent-hub:begin (managed)\nkey = 1\n# agent-hub:end\n"
for live in [None, b"", b"# My own notes\n\nkeep me\n", b"top\n", blk, b"before\n\n" + blk + b"\nafter\n", b"x\n" + blk,
             blk.replace(b"Be concise.", b"Be chatty."), f"x\n{MD}\nno end marker\n".encode(),
             b"a\r\n" + blk.replace(b"\n", b"\r\n"), b"\xff\xfe junk\n" + blk, ("é" * 3 + "\n").encode() + blk,
             b"mine\n\n\n" + blk + b"\n\n\ntail\n",
             b"inline " + MD.encode() + b" not at line start\n" + MD.encode() + b"\nbroken\n",
             blk + b"<!-- agent-hub:end -->   \n"]:
    for adopt in (False, True):
        cases.append(case("block", [(blk, [])], live, adopt, "CLAUDE.md"))
        cases.append(case("block", [(blk.rstrip(b"\n"), [])], live, adopt, "CLAUDE.md"))
        cases.append(case("block", [(None, [])], live, adopt, "CLAUDE.md"))
        cases.append(case("block", [(None, [])], live, adopt, "config.toml"))
for live in [None, b"[core]\nx = 1\n", b"[core]\nx = 1", hblk, b"[a]\n" + hblk + b"[b]\n", b"  \n" + hblk]:
    cases.append(case("block", [(hblk, [])], live, False, "config.toml"))
    cases.append(case("block", [(b"\n\n  " + hblk, [])], live, False, "config.toml"))
    cases.append(case("block", [(None, [])], live, False, "config.toml"))
for live in [None, b"", b"x", b"\x00\xff"]:
    cases.append(case("own", [(b"new", [])], live))
    cases.append(case("own", [(None, [])], live))
cases.append(case("nonsense", [(b"x", [])], b""))
(out / "merge.json").write_text(json.dumps({"cases": cases}, indent=0) + "\n")

ycases = []
for live in [None, b"", b"theme: dark\nagent:\n  disabled_toolsets: [old]\n  other: 1\n", b"mcp_servers: {}\n",
             b"mcp_servers:\n  a:\n    command: x\n", b"- list\n", b"x: &a 1\ny: *a\n", b"k: 2024-01-02\nd: {a: 1}\n",
             b"agent:\n  disabled_toolsets: []\n"]:
    for adopt in (False, True):
        ycases.append(case("yaml_keys", [(b"agent:\n  disabled_toolsets: [browser]\n", ["agent.disabled_toolsets"])],
                           live, adopt))
        ycases.append(case("yaml_keys", [(None, ["agent.disabled_toolsets"])], live, adopt))
        ycases.append(case("yaml_keys", [(b"mcp_servers:\n  srv:\n    command: srv\n    args: [-v, 'x y']\n",
                                          ["mcp_servers"])], live, adopt))
(out / "merge_yaml.json").write_text(json.dumps({"cases": ycases}, indent=0) + "\n")

# ---- planner.unified -------------------------------------------------------------------------------------------------
ucases = []
for live, new, path in [(None, b"a\nb\n", "x.md"), (b"a\n", None, "x.md"), (b"a\nb\n", b"a\nc\n", "p/q.md"),
                        (b"x\x00y", b"x", "bin"), (b"api_key = sk-abcdefghijklmnopqrstuv\n", b"token: hunter2-secret\n", "s"),
                        (None, ("line\n" * 30000).encode(), "big"), (b"\xff\n", b"\xfe\n", "bad"), (b"", b"", "e"),
                        (None, None, "n")]:
    ucases.append({"live": b64(live), "new": b64(new), "path": path, "out": unified(live, new, path)})
(out / "planner_unified.json").write_text(json.dumps({"cases": ucases}, indent=0, ensure_ascii=False) + "\n")
print(f"{len(cases)} merge, {len(ycases)} yaml merge, {len(ucases)} unified cases")
