"""Golden corpus for hub-core's PyYAML 6.0.3 port (backend/crates/hub-core/tests/golden/yaml_*.json).

    env -u LITELLM_API_KEY -u OPENAI_API_KEY -u ANTHROPIC_API_KEY -u HF_TOKEN PYTHONHASHSEED=0 \\
        "$PYREL/backend/.venv/bin/python" backend/parity/gen/gen_yaml_golden.py [--capture CAPTURE.jsonl ...]
    (from the repo root; PYREL=$(node backend/parity/lib/python-release.mjs) is the last Python backend, see
     backend/parity/lib/python-release.mjs)

Inputs:
  * backend/parity/gen/yaml_captured.json: every distinct text `yaml.load` saw and every value `yaml.safe_dump` saw while
    the backend's own pytest suite ran (refresh with --capture: run the suite with `-p yaml_capture`, see
    yaml_capture.py; new records are merged, deduplicated and sorted);
  * the repo's YAML files (policy/floor.yaml, examples/clients/*.yaml, deploy/knowledge/container-config.yaml) and the
    YAML/frontmatter files inside backend/tests/adapters/golden/*.json;
  * a matrix of scalars and adversarial strings (YAML 1.1 special words, numbers-as-strings, dates, indicators,
    quotes, leading/trailing spaces, unicode, control characters, line breaks, long lines around width 100) in every
    syntactic position, and a corpus of malformed documents (one per PyYAML error site);
  * a seeded random batch (values and texts) that also serves as the frozen oracle batch for the property tests.

Outputs (all values in the yaml_codec.py encoding):
  yaml_load.json         [{text, safe, strict, yamlsafe}]  each {"ok": value} | {"err": error} | {"cyclic": true}
  yaml_dump.json         [{value, out: {content, frontmatter, adapter[, flow, ascii]}}]
  yaml_frontmatter.json  {"split": [{text, ok|err}], "join": [{meta, body, out}]}
  yaml_random.json       {"dumps": [...same as yaml_dump...], "loads": [...same as yaml_load...]}
"""
from __future__ import annotations

import argparse
import datetime
import json
import random
import sys
from pathlib import Path
from typing import Any

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))
from yaml_codec import Unencodable, dec, enc, error_record  # noqa: E402

from agent_hub.domain import yamlsafe  # noqa: E402

GEN = Path(__file__).resolve().parent
REPO = GEN.parent.parent.parent
GOLDEN = REPO / "backend" / "crates" / "hub-core" / "tests" / "golden"
CAPTURED = GEN / "yaml_captured.json"

OPTS: dict[str, dict[str, Any]] = {
    "content": dict(sort_keys=False, allow_unicode=True, default_flow_style=False, width=100),
    "frontmatter": dict(sort_keys=False, allow_unicode=True, default_flow_style=False, width=100000),
    "adapter": dict(sort_keys=True, allow_unicode=True, default_flow_style=False, width=100000),
}
EXTRA_OPTS: dict[str, dict[str, Any]] = {
    "flow": dict(sort_keys=False, allow_unicode=True, default_flow_style=True, width=80),
    "ascii": dict(sort_keys=False, allow_unicode=False, default_flow_style=False, width=100),
}


# ---- outcome recorders -------------------------------------------------------------------------------------------

def load_outcome(text: str, loader: Any) -> dict[str, Any]:
    try:
        v = yaml.load(text, Loader=loader)  # noqa: S506 - SafeLoader and its strict subclass only
    except RecursionError as exc:
        return {"err": error_record(exc)}
    except Exception as exc:  # noqa: BLE001 - every exception is part of the golden
        return {"err": error_record(exc)}
    try:
        return ok_record(enc(v))
    except Unencodable as exc:
        return {"cyclic": True} if "recursive" in str(exc) else {"unencodable": str(exc)}


def nesting(j: Any) -> int:
    """JSON nesting depth, iteratively (the values can be deeper than Python's recursion limit allows)."""
    best = 0
    stack = [(j, 0)]
    while stack:
        x, d = stack.pop()
        if isinstance(x, dict):
            best = max(best, d + 1)
            stack.extend((y, d + 1) for y in x.values())
        elif isinstance(x, list):
            best = max(best, d + 1)
            stack.extend((y, d + 1) for y in x)
    return best


DEEP = 100  # serde_json's default recursion limit is 128 levels


def ok_record(e: Any) -> dict[str, Any]:
    """Values nested deeper than serde_json reads are stored as compact JSON text and compared as text."""
    if nesting(e) > DEEP:
        return {"ok_json": json.dumps(e, ensure_ascii=False, separators=(",", ":"))}
    return {"ok": e}


def yamlsafe_outcome(text: str) -> dict[str, Any]:
    try:
        v = yamlsafe.load(text)
    except yamlsafe.YamlError as exc:
        return {"err": {"message": str(exc)}}
    except Exception as exc:  # noqa: BLE001
        return {"err": {"py_exc": type(exc).__name__, "message": str(exc)}}
    try:
        return ok_record(enc(v))
    except Unencodable as exc:
        return {"unencodable": str(exc)}


def load_case(text: str) -> dict[str, Any]:
    return {
        "text": text,
        "safe": load_outcome(text, yaml.SafeLoader),
        "strict": load_outcome(text, yamlsafe._StrictLoader),  # noqa: SLF001
        "yamlsafe": yamlsafe_outcome(text),
    }


def dump_case(value: Any, extra: bool = False) -> dict[str, Any] | None:
    """`value` is a yaml_codec encoding; the case records the re-encoded value (Python's own set order)."""
    if nesting(value) > DEEP:
        return None
    try:
        py = dec(value)
        canon = enc(py)
    except (Unencodable, ValueError, TypeError, OverflowError):
        return None
    out: dict[str, Any] = {}
    for name, kw in {**OPTS, **(EXTRA_OPTS if extra else {})}.items():
        try:
            out[name] = yaml.safe_dump(py, **kw)
        except Exception as exc:  # noqa: BLE001
            out[name] = {"error": f"{type(exc).__name__}: {exc}"}
    return {"value": canon, "out": out}


# ---- inputs ------------------------------------------------------------------------------------------------------

def merge_capture(paths: list[str]) -> None:
    data = json.loads(CAPTURED.read_text(encoding="utf-8")) if CAPTURED.exists() else {"loads": [], "dumps": []}
    loads = {(r["text"], r["loader"]) for r in data["loads"]}
    dumps = {json.dumps(r["data"], sort_keys=True, ensure_ascii=False): r for r in data["dumps"]}
    for p in paths:
        for line in Path(p).read_text(encoding="utf-8").splitlines():
            r = json.loads(line)
            if r["op"] == "load":
                loads.add((r["text"], r["loader"]))
            else:
                dumps.setdefault(json.dumps(r["data"], sort_keys=True, ensure_ascii=False),
                                 {"data": r["data"], "kwds": r["kwds"]})
    out = {
        "_comment": "Inputs captured from the backend pytest suite (yaml_capture.py); see gen_yaml_golden.py.",
        "loads": [{"text": t, "loader": ld} for t, ld in sorted(loads)],
        "dumps": [dumps[k] for k in sorted(dumps)],
    }
    CAPTURED.write_text(json.dumps(out, ensure_ascii=False, indent=0) + "\n", encoding="utf-8")


def repo_texts() -> list[str]:
    texts = []
    for p in [REPO / "policy" / "floor.yaml", *sorted((REPO / "examples" / "clients").glob("*.yaml")),
              REPO / "deploy" / "knowledge" / "container-config.yaml"]:
        if p.exists():
            texts.append(p.read_text(encoding="utf-8"))
    return texts


def adapter_golden_files() -> list[tuple[str, str]]:
    out = []
    for g in sorted((GOLDEN / "python-adapter-golden").glob("*.json")):   # the Python adapter goldens, kept at cutover
        doc = json.loads(g.read_text(encoding="utf-8"))
        for art in doc.get("artifacts", []):
            content = art.get("content")
            if isinstance(content, str):
                out.append((art.get("path", ""), content))
    return out


STRINGS: list[str] = [
    "", " ", "  ", "x", "abc", "hello world", "Hello, World!", " x", "x ", " x ", "x  y", "x   y", "\t", "x\t", "\tx",
    "x\ty",
    # YAML 1.1 special words and numbers-as-strings
    "yes", "Yes", "YES", "no", "No", "NO", "on", "On", "ON", "off", "Off", "OFF", "y", "Y", "n", "N", "true", "True",
    "TRUE", "false", "False", "FALSE", "null", "Null", "NULL", "~", "nULL", "tRUE", "yES",
    "0", "1", "-1", "+1", "01", "007", "08", "09", "0o17", "0x1F", "0X1F", "0b101", "0b12", "1_000", "1__0", "_1",
    "1:30", "1:30:00", "-1:30", "1:60", "1:5", "190:20:30", "1.0", "1.", ".5", "-.5", "+.5", "1.5", "1e5", "1E5",
    "1.0e5", "1.0e+5", "1.0E-5", "1e+5", "6.8523015e+5", "685.230_15e+03", "685_230.15", "190:20:30.15", ".inf",
    "-.inf", "+.inf", ".Inf", ".INF", "inf", ".nan", ".NaN", ".NAN", "nan", "-.nan", "12345678901234567890",
    "3.14159265358979323846", "1_", "0_", "0x_", "0b_", "0.", "00", "-0", "+0", "-0.0",
    "2024-01-02", "2024-1-2", "2024-01-02 10:20:30", "2024-01-02T10:20:30Z", "2024-01-02t10:20:30.5+01:00",
    "2024-01-02 10:20:30 -05:30", "2001-12-14t21:59:43.10-05:00", "2024-02-30", "2024-13-01", "0000-01-01",
    "2024-01-02 25:00:00", "20240102", "2024/01/02", "10:20:30",
    # indicators and quoting
    "<<", "=", "!", "!x", "&a", "*a", "|", ">", "|-", ">+", "@x", "`x", "%x", "#x", "x #y", "x#y", "x # y", "x: y",
    "x:y", ":x", ": x", "x:", "x: ", "- x", "-x", "-", "--", "---", "--- x", "...", "... x", "?", "? x", "?x", "x?",
    "[x]", "[", "]", "{x}", "{", "}", "x, y", "x,y", ",x", "x,", "'x'", '"x"', "x'y", 'x"y', "'", '"', "''", "x'",
    "a\\b", "\\", "a/b", "http://x.y/z?a=b&c=d#frag", "key=value", "x=y", "C:\\path\\to", "a|b", "a>b", "a{b}",
    "a[b]", "a!b", "a&b", "a*b", "a%b", "a@b", "a`b", "a,b",
    # unicode
    "é", "café", "日本語", "😀", "x😀y", "\u00a0", "x\u00a0y", "\ufeff", "x\ufeffy", "\u200b", "\u2028", "x\u2029y",
    "\u0085", "x\u0085y", "\ud7ff", "\ue000", "\ufffd", "\ufffe", "\uffff", "\U0010ffff", "\U0001f600", "Ω≈ç√",
    # control characters
    "\x00", "\x01", "\x07", "\x08", "\x0b", "\x0c", "\x1b", "\x1f", "\x7f", "\x80", "\x9f", "a\x00b", "\r", "x\ry",
    "\r\n", "x\r\ny",
    # line breaks
    "\n", "a\n", "\na", "a\nb", "a\n\nb", "a\n\n\nb", "a\nb\n", "a\n b", "a \nb", "a  \n  b", " a\nb", "a\n\n", "\n\n",
    "line one\nline two\nline three\n", "  indented\n  text\n", "a\tb\nc",
    "# heading\n\nSome text with: colons, #hashes and 'quotes'.\n- item\n",
]

LONG_WORD = "x" * 120
LONG_STRINGS: list[str] = [
    ("word " * 30).strip(),
    "word " * 30,
    ("lorem ipsum dolor sit amet " * 8).strip(),
    "a" * 99, "a" * 100, "a" * 101, "a" * 127, "a" * 128, "a" * 129, "a" * 200,
    ("ab " * 33).strip(), ("ab " * 34).strip(), ("ab  " * 30).strip(),
    LONG_WORD, LONG_WORD + " " + LONG_WORD, "short " + LONG_WORD,
    "Use when: " + "the user asks for something rather specific " * 4,
    "'quoted' and long " + "padding " * 20,
    '"double" ' + "padding text " * 12,
    "x: " + "y " * 60,
    "tab\there " + "more " * 25,
    "unicode é " + "très long texte " * 10,
    "\x01 control " + "and more words " * 10,
    "line1\n" + "a long second line " * 8 + "\nline3",
    ("multi line " * 12 + "\n") * 3,
    " leading space " + "words " * 25,
    "trailing space " + "words " * 25 + " ",
    "  two  spaces  " * 10,
    "a" * 98 + " b",
    "a " * 49 + "bbb",
    "x" * 95 + " " + "y" * 10 + " " + "z" * 10,
]

SCALARS: list[Any] = [
    None, True, False, 0, 1, -1, 7, 2**31, 2**63 - 1, -(2**63), 2**63, 2**64 + 5, -(2**70), 10**30,
    0.0, -0.0, 1.0, -1.5, 0.1, 1 / 3, 1e16, 1e17, 1.5e17, 1e-5, 1e-7, 1.25e-7, 123456789.123, 5e-324, 1e300,
    1.7976931348623157e308, float("inf"), float("-inf"), float("nan"), 100.0, 1e22,
    b"", b"abc", bytes(range(256)), b"x" * 100,
    datetime.date(2024, 1, 2), datetime.date(1, 1, 1), datetime.date(9999, 12, 31),
    datetime.datetime(2024, 1, 2, 3, 4, 5), datetime.datetime(2024, 1, 2, 3, 4, 5, 6),
    datetime.datetime(2024, 1, 2, 3, 4, 5, tzinfo=datetime.timezone.utc),
    datetime.datetime(2024, 1, 2, 3, 4, 5, 123000, tzinfo=datetime.timezone(datetime.timedelta(hours=1))),
    datetime.datetime(2024, 1, 2, tzinfo=datetime.timezone(-datetime.timedelta(hours=5, minutes=30))),
    [], {}, set(), [[]], [{}], {"a": []}, {"a": {}}, [[], {}], {1, 2, 3}, {"a"}, (1, "x"), ((), ()),
]


def matrix_values() -> list[Any]:
    vals: list[Any] = []
    for s in STRINGS + LONG_STRINGS:
        vals += [s, {"k": s}, {s: "v"}, [s], {"a": [s]}, [{"k": s}], {"a": {"b": s}}, [[s]],
                 {"key_" * 10: s}, {"k": [s, s]}, [s, {"k": s}]]
    for v in SCALARS:
        vals += [v, {"k": v}, [v], {"a": [v, {"b": v}]}]
    for v in SCALARS:
        try:
            hash(v)
        except TypeError:
            continue
        vals.append({v: "x"})
    vals += [
        {"b": 1, "a": 2, "c": 3}, {"b": 1, 1: 2, "a": 3}, {1: "a", 2.5: "b", True: "c", -3: "d"}, {None: 1, "a": 2},
        {None: 1}, {2: "x", 1: "y"}, {b"b": 1, b"a": 2}, {datetime.date(2024, 1, 2): 1, datetime.date(2023, 1, 1): 2},
        {datetime.date(2024, 1, 2): 1, "a": 2}, {(1, 2): 3}, {("a",): {"b": 1}}, {"z": {"y": {"x": [1, 2, {"w": None}]}}},
        {"list": [[1, 2], [3, [4, [5]]]], "map": {"m": {"n": {}}}}, [[[[[[]]]]]], {"a": {"b": {"c": {"d": {"e": 1}}}}},
        {"k" * 130: 1}, {"k" * 120: 1}, {"k" * 122: 1}, {"k" * 123: 1}, {"k" * 124: 1},
        {"multi\nline": 1}, {"": 1}, {" ": 1}, {"-": 1}, {"?": 1}, {":": 1}, {"a: b": 1},
        [None, None], {"a": None, "b": None}, [{"a": 1, "b": 2}, {"c": 3}], [[1, 2], [3, 4]], [[{"a": [1]}]],
        {"empty_list": [], "empty_dict": {}, "set": {1}}, {"s": {"b", "a", "c"}},
    ]
    return vals


ERROR_DOCS: list[str] = [
    "a: b: c", "- a\nb: c", "[a, b", "{a: b", "a: [b, c}", "a: 'unterminated", 'a: "unterminated', 'a: "bad \\q escape"',
    'a: "\\x4"', 'a: "\\u12"', 'a: "\\U0011000"', "a: *undefined", "a: &x 1\nb: &x 2", "--- a\n--- b", "a\n---\nb",
    "%YAML 1.1\n%YAML 1.1\n--- a", "%YAML 2.0\n--- a", "%YAML 1.1\n--- a", "%YAML 01.2\n--- a",
    "%TAG ! foo\n%TAG ! bar\n--- a", "%FOO bar baz\n--- a", "%YAML 1.1 x\n--- a", "%YAML a\n--- a", "%YAML 1\n--- a",
    "%YAML 1.\n--- a", "%\n--- a", "%TAG !x foo\n--- a", "%TAG !x! foo bar\n--- a", "%TAG !x!\n--- a",
    "%YAML 1.1 # comment\n--- a", "%YAML 1.1\n", "%YAML 1.1\na: b",
    "a: !!python/object/apply:os.system ['echo hi']", "a: !foo x", "a: !<tag:x> y", "a: !<tag:x y", "a: !e!foo x",
    "a: !e! x", "a: !!x!y z", "a: !x", "a: ! x", "a: ! 12", "a: !!", "!!str", "! a",
    "a: |0\n x", "a: |x\n", "a: | x\n", "a: |  # comment\n  x", "a: |1-\n  x", "a: |-1\n  x", "a: |+\n  x\n\n",
    "a: >\n b\n c\n", "a: >-\n b\n\n c\n", "a: >2\n   b\n", "a: |\n x\n  y\n z\n", "- |\n a\n- >\n b\n",
    "\t- a", "a:\n\t- b", "a: \tb", "key: value\n  bad: indent", "? a\n? b", "- a\n  - b", "a\nb: c", "a: b\n- c",
    "{a: b}}", "]", "}", ",", "a: `x", "@x", "a: @x", "a: %x", "a: 1\na: 2", "{a: 1, a: 2}", "a: 1\n1: 2\n1.0: 3",
    "a: 2024-02-30", "a: 2024-13-01", "a: 0000-01-01", "a: 2020-01-01 25:00:00", "a: 2020-01-01 10:61:00",
    "a: 2020-01-01 10:00:61", "a: 2020-01-01 10:00:00 +25:00", "a: 2020-01-01 10:00:00 -24:00",
    "a: 2020-01-01 10:00:00 +23:59", "a: 2020-01-01 10:00:00.1234567", "a: 2020-01-01 10:00:00.", "a: 2020-1-1 1:00:00",
    "a: !!timestamp foo", "a: !!timestamp 2020-1-1", "a: !!timestamp {=: 2020-01-01}", "a: !!int abc", "a: !!int",
    "a: !!int ''", "a: !!int 1:-5", "a: !!int --5", "a: !!int 0x0x1F", "a: !!int 0o17", "a: !!int 1_000_",
    "a: !!float x", "a: !!float", "a: !!float 1:30.5", "a: !!float -.nan", "a: !!float nan", "a: !!float 1e5",
    "a: !!float infinity", "a: !!int '-'", "a: !!int '+'", "a: !!float '-'", "a: !!int ١٢٣", "a: !!int '\u3000 12'",
    "a: !!int '\x1c12'", "a: !!float ١٢.٥", "a: !!int ０x１Ｆ", "a: !!int 0x١", "a: !!bool ＹＥＳ", "a: !!bool İ", "a: !!bool maybe", "a: !!bool Yes", "a: !!bool", "a: !!binary é", "a: !!binary éé x",
    "a: !!binary abcde", "a: !!binary abc", "a: !!binary aGVsbG8=", "a: !!binary 'aGVs bG8='", "a: !!binary a===",
    "a: !!binary ab=c", "a: !!binary |\n  aGVsbG8gd29y\n  bGQ=\n", "a: 0x_", "a: 0b_", "a: !!null x", "a: !!null {=: x}",
    "a: !!null [1]", "a: !!str [1]", "a: !!str {a: 1}", "a: !!str {=: v}", "a: !!seq x", "a: !!seq {}", "a: !!map x",
    "a: !!map ''", "a: !!map [1]", "a: !!map [[1, 2]]", "a: !!map []", "a: !!set x", "a: !!set {a, b}", "a: !!set {a: 1}",
    "a: !!omap {a: 1}", "a: !!omap [1]", "a: !!omap [{a: 1, b: 2}]", "a: !!omap [a: 1, b: 2]", "a: !!pairs [a: 1, a: 2]",
    "a: !!pairs x", "a: !!pairs [[1]]", "<<: {a: 1}\nb: 2", "a: &x {b: 1}\nc:\n  <<: *x\n  d: 2",
    "base: &b {x: 1}\nother: &o {y: 2, x: 9}\nm:\n  <<: [*b, *o]\n  z: 3", "<<: [1]", "<<: 1", "<<: [{a: 1}, 2]",
    "a: {<<: {b: 1}, b: 2}", "=: x", "a: =", "a: {=: x}", "? [a]\n: b", "? {a: 1}\n: b", "{[a]: b}", "? !!set {a}\n: b",
    "? !!binary aGk=\n: b", "? 2024-01-02\n: b", "{1: a, 1.0: b, true: c}", "{.nan: 1, .nan: 2}", "{0: a, -0.0: b}",
    "{2020-01-01 10:00:00+01:00: a, 2020-01-01 09:00:00Z: b}", "{2020-01-01: a, 2020-01-01 00:00:00: b}",
    'x: "\\ud800"', 'x: "\\U00110000"', 'x: "\\U0001F600"', 'x: "\\x41\\u00e9\\N\\_\\L\\P\\e\\0\\a\\b\\t\\v\\f\\r\\/"',
    'x: "\\\n  y"', "x: !<tag:yaml.org,2002:str> 5", "x: !!str 5", "x: ! 5", "x: ! '5'", 'x: ! "12\\n"',
    'x: ! "yes\\n"', 'x: ! "1:30\\n"', 'x: ! "~\\n"', 'x: ! "\\n"', 'x: ! "2020-01-01\\n"', 'x: ! "0x1F\\n"',
    "%TAG !e! tag:example.com,2000:\n---\na: !e!foo x", "x: !%41 y", "x: !%ff y", "x: !<%e2%82> y", "x: !<%e2%82%ac> y",
    "x: !%4 y", "x: !<> y", "x: !<!> y",
    "a" * 1100 + ": b", "a" * 1020 + ": b", "a\nb: c", "a: b\nc", "- a\n- b\n c", 'key: "a\n---\nb"', "key: 'a\n...\nb'",
    "\x00", "a: \x01", "a: \ufffe", "a: b\x7f", "\ufeffa: b", "a: \ufeffb", "a: b #c", "a: b#c", "a: 'x' y", 'a: "x" y',
    "a:\n  - b\n  c: d", "a: [b, c]: d", "[a: b]", "[? a: b]", "[a, ? b]", "{a, b: c}", "{? a}", "[a, b]: c", "{a: b}: c",
    "a: &x\nb: *x", "a: &x [1, 2]\nb: *x", "&a [*a]", "&a {x: *a}", "&a [&b [*a, *b]]", "a: &x b\nc: *x",
    "- &a !!str x\n- *a", "a: !!str &x y\nb: *x", "*a", "&a", "&a\n", "- &a\n- *a", "? &k a\n: *k", "&a a: b",
    "", "   ", "\n\n", "# only a comment\n", "---", "---\n", "...", "--- \n...\n", "---\n---\n", "...\n---\na: 1",
    "---\na: 1\n...\n", "--- |\n  x\n", "--- >\n  x\n", "--- 'x'", '--- "x"', "--- [a]", "--- {a: 1}", "---\n- a\n...",
    "a: 1\n---\n", "a: 1 # c\n# c\nb: 2", "-\n-\n", "- \n- x", "?\n:\n", "? \n: \n", ":", ": x", "a:", "a:\n", "- a:\n  b: c",
    "a: {b: [c, {d: e}], f: g}", "[1, [2, [3, [4]]]]", "{a: {b: {c: {}}}}", "a: [\n  1,\n  2,\n]\n", "a: {\n}\n",
    "a: [b,\nc]", "a: b,c", "a: [b,c]", "a: {b:c}", "a: {b: c,}", "a: [,]", "a: [b,,c]", "a: {,}", "{a: b, c}",
    "a:    b    ", "a: b   \n", "  a: b", "a:\n    - b\n    - c", "a:\n- b\n- c", "a:\n  b:\n    c: d\n  e: f",
    "x: |\n\n  a\n\n  b\n\n", "x: >\n  a\n  b\n\n  c\n   d\n  e\n", "x: |2\n    a\n", "x: |-\n\n", "x: >+\n\n\n",
    "x: 'a\n  b\n\n  c'", 'x: "a\n  b\n\n  c"', "x: 'it''s'", 'x: "say \\"hi\\""', "x: a\n  b\n\n  c", "x: a\n\n\n  b",
    "x: a\n---", "x: a\n...", "x: 'a'\n---\n", "a: b\n\t", "a: b\n  # comment\n", "a: >\n\tb", "a: 'b\tc'",
    "x: 12\r\ny: 13\r\n", "x: a\rb", "x: 'a\r\n b'", "x: |\r\n a\r\n b\r\n", "x: a\u2028y: b", "x: a\x85y: b",
    "x: 1\u2029", "- a\u2028- b",
]


def nesting_cases() -> list[str]:
    out = []
    for n in (12, 13, 30, 300, 330, 400, 520):
        out.append("[" * n + "]" * n)
    out.append("".join("  " * i + "- \n" for i in range(19)) + "  " * 19 + "- 1\n")
    out.append("".join("{a: " for _ in range(14)) + "1" + "}" * 14)
    return out


def frontmatter_cases(dump_values: list[Any]) -> dict[str, Any]:
    bodies = ["", "body\n", "body", "\n\nbody\n\n", "line\n---\nstill body\n", "# Title\n\ntext: with colon\n",
              "  indented\r\nwindows\r\n", "---\n"]
    join = []
    metas = [v for v in dump_values if isinstance(v, dict)][:600]
    for i, meta in enumerate(metas):
        body = bodies[i % len(bodies)]
        try:
            out = yamlsafe.join_frontmatter(meta, body)
            join.append({"meta": enc(meta), "body": body, "out": out})
        except (Unencodable, Exception):  # noqa: BLE001
            continue
    split_texts = [j["out"] for j in join] + [
        "no frontmatter", "---\nname: x\n", "---\n- a\n- b\n---\nbody", "---\n---\nbody", "---\r\nname: x\r\n---\r\nbody",
        "---\r\nname: x\n---\nbody", "---\nname: x\n--- \nbody", "---\nname: x\n---", "---\nname: x\n---\n",
        "---\nname: 'a\n---\nb'\n---\nbody", "---\na: &x 1\n---\n", "---\na: 1\na: 2\n---\n", "---\n: bad\n---\n",
        "--- \nname: x\n---\n", "---\nname: [x\n---\nbody", "---\nnull\n---\nbody", "---\n~\n---\nb", "---\n'x'\n---\n",
        "---\n# comment only\n---\nbody", "---\nname: x\n...\n---\nbody", "\ufeff---\nname: x\n---\n", "---\n\n---\n",
        "---\nname: x\n----\n---\nb", "---\n" + "a: b\n" * 3 + "---\n" + "x" * 10,
    ]
    split = []
    for t in split_texts:
        try:
            meta, body = yamlsafe.split_frontmatter(t)
            split.append({"text": t, "ok": [enc(meta), body]})
        except yamlsafe.YamlError as exc:
            split.append({"text": t, "err": {"message": str(exc)}})
        except Unencodable:
            continue
        except Exception as exc:  # noqa: BLE001
            split.append({"text": t, "err": {"py_exc": type(exc).__name__, "message": str(exc)}})
    return {"split": split, "join": join}


# ---- random batch (frozen oracle batch for the property tests) -----------------------------------------------------

ALPHABET = list("abcxyz019 _-.:,?!#&*|>'\"%@`[]{}\\/=<~+\n\t") + ["é", "😀", "\u00a0", "\u2028", "\x85", "\x00", "\r"]
WORDS = ["yes", "no", "on", "null", "~", "true", "1", "0x1F", "1:30", "1.0", ".inf", "2024-01-02", "<<", "=", "- ",
         ": ", " #", "---", "...", "'", '"', "\n", "  ", "é", "x" * 30]


def rand_str(r: random.Random) -> str:
    n = r.choice([0, 1, 2, 3, 5, 8, 13, 40, 110])
    parts = []
    while len("".join(parts)) < n:
        parts.append(r.choice(WORDS) if r.random() < 0.3 else r.choice(ALPHABET))
    return "".join(parts)


def rand_value(r: random.Random, depth: int = 0) -> Any:
    k = r.random()
    if depth > 3 or k < 0.55:
        c = r.random()
        if c < 0.55:
            return rand_str(r)
        if c < 0.65:
            return r.choice([None, True, False])
        if c < 0.8:
            return r.choice([0, 1, -1, 7, 255, 2**40, -(2**33), 10**20])
        return r.choice([0.0, 1.5, -2.25, 1e17, 1e-7, 3.0, float("inf")])
    if k < 0.8:
        return [rand_value(r, depth + 1) for _ in range(r.choice([0, 1, 2, 3, 5]))]
    d: dict[Any, Any] = {}
    for _ in range(r.choice([0, 1, 2, 3, 5])):
        key = rand_str(r) if r.random() < 0.9 else r.choice([1, 2, True, None, 1.5])
        d[key] = rand_value(r, depth + 1)
    return d


def rand_text(r: random.Random) -> str:
    lines = []
    for _ in range(r.choice([1, 2, 3, 5])):
        indent = " " * r.choice([0, 0, 2, 4, 1])
        lines.append(indent + rand_str(r))
    return "\n".join(lines) + r.choice(["", "\n"])


def structured_text(r: random.Random) -> str:
    """A random value dumped with random PyYAML styles (block/flow/canonical, literal/folded/quoted defaults, indents,
    widths, explicit markers, directives), then lightly mutated: realistic structure for the loader."""
    v = rand_value(r)
    kw: dict[str, Any] = {
        "default_flow_style": r.choice([False, True, None]),
        "indent": r.choice([None, 2, 3, 4, 8]),
        "width": r.choice([None, 20, 40, 100]),
        "allow_unicode": r.choice([True, False]),
        "explicit_start": r.random() < 0.2,
        "explicit_end": r.random() < 0.1,
        "canonical": r.random() < 0.1,
        "default_style": r.choice([None, None, None, "'", '"', "|", ">"]),
        "sort_keys": False,
    }
    if r.random() < 0.05:
        kw["version"] = (1, 1)
    if r.random() < 0.05:
        kw["tags"] = {"!e!": "tag:example.com,2000:"}
    try:
        text = yaml.safe_dump(v, **kw)
    except Exception:  # noqa: BLE001
        text = yaml.safe_dump(v)
    for _ in range(r.choice([0, 0, 1, 1, 2, 3])):
        if not text:
            break
        i = r.randrange(len(text))
        op = r.random()
        if op < 0.3:
            text = text[:i] + text[i + 1:]
        elif op < 0.6:
            text = text[:i] + r.choice(WORDS + ALPHABET) + text[i:]
        elif op < 0.8:
            lines = text.split("\n")
            j = r.randrange(len(lines))
            lines[j] = " " * r.choice([1, 2, 4]) + lines[j] if r.random() < 0.5 else lines[j].lstrip(" ")
            text = "\n".join(lines)
        else:
            lines = text.split("\n")
            j = r.randrange(len(lines))
            lines.insert(j, lines[j])
            text = "\n".join(lines)
    return text


# ---- main ----------------------------------------------------------------------------------------------------------

def write(name: str, data: Any) -> None:
    """One case per line (reviewable diffs, no indentation bulk)."""
    def lines(items: list[Any]) -> str:
        return "[\n" + ",\n".join(json.dumps(i, ensure_ascii=False, separators=(",", ":")) for i in items) + "\n]"
    if isinstance(data, dict):
        body = "{\n" + ",\n".join(f"{json.dumps(k)}:{lines(v)}" for k, v in data.items()) + "\n}"
    else:
        body = lines(data)
    (GOLDEN / name).write_text(body + "\n", encoding="utf-8")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--capture", nargs="*", default=[])
    args = ap.parse_args()
    if args.capture:
        merge_capture(args.capture)
    captured = json.loads(CAPTURED.read_text(encoding="utf-8"))

    texts: list[str] = []
    seen: set[str] = set()

    def add_text(t: str) -> None:
        if t not in seen:
            seen.add(t)
            texts.append(t)

    for r in captured["loads"]:
        add_text(r["text"])
    for t in repo_texts():
        add_text(t)
    for path, content in adapter_golden_files():
        if path.endswith((".yaml", ".yml")):
            add_text(content)
        elif content.startswith("---\n") and "\n---\n" in content[4:]:
            add_text(content[4:content.index("\n---\n", 4) + 1])
    for t in ERROR_DOCS + nesting_cases():
        add_text(t)
    for s in STRINGS + LONG_STRINGS:
        for t in (s, f"k: {s}\n", f"- {s}\n", f"{s}: v\n", f"[{s}]", f"{{k: {s}}}"):
            add_text(t)
    loads = [load_case(t) for t in texts]

    values: list[Any] = []
    vseen: set[str] = set()

    def add_value(v: Any, extra: bool = False) -> None:
        try:
            e = enc(v) if not isinstance(v, dict) or "__enc__" not in v else v["__enc__"]
        except Unencodable:
            return
        key = json.dumps(e, sort_keys=True, ensure_ascii=False)
        if key not in vseen:
            vseen.add(key)
            values.append((e, extra))

    for r in captured["dumps"]:
        add_value({"__enc__": r["data"]})
    for case in loads:
        if "ok" in case["strict"]:
            add_value({"__enc__": case["strict"]["ok"]})
    for v in matrix_values():
        add_value(v, extra=True)
    dumps = [c for c in (dump_case(e, extra) for e, extra in values) if c is not None]

    # deep values (built programmatically on both sides; the dumper must not recurse)
    for kind, depth in (("list", 150), ("list", 250), ("dict", 150), ("dict", 250), ("mixed", 201)):
        v: Any = "leaf"
        for i in range(depth):
            v = [v] if kind == "list" or (kind == "mixed" and i % 2) else {"k": v}
        dumps.append({"construct": [kind, depth],
                      "out": {name: yaml.safe_dump(v, **kw) for name, kw in OPTS.items()}})
    write("yaml_load.json", loads)
    write("yaml_dump.json", dumps)
    write("yaml_frontmatter.json", frontmatter_cases([dec(c["value"]) for c in dumps if "value" in c]))

    r = random.Random(20261027)
    rdumps = [c for c in (dump_case(enc(rand_value(r))) for _ in range(1500)) if c is not None]
    rloads = [load_case(rand_text(r)) for _ in range(3000)]
    rloads += [load_case(structured_text(r)) for _ in range(3000)]
    write("yaml_random.json", {"dumps": rdumps, "loads": rloads})
    print(f"loads {len(loads)}, dumps {len(dumps)}, random dumps {len(rdumps)}, random loads {len(rloads)}")


if __name__ == "__main__":
    main()
