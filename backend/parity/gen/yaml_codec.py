"""Typed JSON encoding of the Python values PyYAML loads and dumps, shared by the YAML golden generator, the capture
plugin and the oracle (backend/crates/hub-core/tests/yaml_golden.rs decodes it into PyValue):

    None -> null            bool -> true/false        int -> {"int": "<decimal>"}     float -> {"float": repr(f)}
    str  -> "<string>"      bytes -> {"bytes": "<base64>"}
    date -> {"date": [y, m, d]}            datetime -> {"datetime": [y, m, d, H, M, S, us, utcoffset_seconds|null]}
    list -> {"list": [...]} tuple -> {"tuple": [...]} dict -> {"dict": [[k, v], ...]}   set -> {"set": [...]}

Containers keep Python's iteration order. Values that cannot be encoded (cycles, lone surrogates, other types) raise
Unencodable."""
from __future__ import annotations

import base64
import datetime
from typing import Any


class Unencodable(Exception):
    pass


def enc(v: Any, _seen: tuple[int, ...] = ()) -> Any:
    if v is None or v is True or v is False:
        return v
    if isinstance(v, int):
        try:
            return {"int": str(v)}
        except ValueError as exc:  # more than 4300 digits
            raise Unencodable(str(exc)) from exc
    if isinstance(v, float):
        return {"float": repr(v)}
    if isinstance(v, str):
        try:
            v.encode("utf-8")
        except UnicodeEncodeError as exc:
            raise Unencodable("lone surrogate") from exc
        return v
    if isinstance(v, bytes):
        return {"bytes": base64.b64encode(v).decode("ascii")}
    if isinstance(v, datetime.datetime):
        off = v.utcoffset()
        return {"datetime": [v.year, v.month, v.day, v.hour, v.minute, v.second, v.microsecond,
                             None if off is None else int(off.total_seconds())]}
    if isinstance(v, datetime.date):
        return {"date": [v.year, v.month, v.day]}
    if isinstance(v, (list, tuple, dict, set, frozenset)):
        if id(v) in _seen:
            raise Unencodable("recursive structure")
        seen = _seen + (id(v),)
        if isinstance(v, list):
            return {"list": [enc(x, seen) for x in v]}
        if isinstance(v, tuple):
            return {"tuple": [enc(x, seen) for x in v]}
        if isinstance(v, dict):
            return {"dict": [[enc(k, seen), enc(x, seen)] for k, x in v.items()]}
        return {"set": [enc(x, seen) for x in v]}
    raise Unencodable(type(v).__name__)


def dec(j: Any) -> Any:
    if j is None or isinstance(j, bool):
        return j
    if isinstance(j, str):
        return j
    if isinstance(j, dict):
        (k, v), = j.items()
        if k == "int":
            return int(v)
        if k == "float":
            return float(v)
        if k == "bytes":
            return base64.b64decode(v)
        if k == "date":
            return datetime.date(*v)
        if k == "datetime":
            tz = None if v[7] is None else datetime.timezone(datetime.timedelta(seconds=v[7]))
            return datetime.datetime(*v[:7], tzinfo=tz)
        if k == "list":
            return [dec(x) for x in v]
        if k == "tuple":
            return tuple(dec(x) for x in v)
        if k == "dict":
            return {dec(a): dec(b) for a, b in v}
        if k == "set":
            return {dec(x) for x in v}
    raise ValueError(f"bad encoding: {j!r}")


def error_record(exc: BaseException) -> dict[str, Any]:
    """What the Rust side's YamlError must carry for this exception."""
    import yaml

    from agent_hub.domain import yamlsafe

    text = str(exc)
    if isinstance(exc, yamlsafe.YamlError):
        return {"problem": text, "full": text, "custom": True, "py_exc": None, "problem_mark": None}
    if isinstance(exc, yaml.YAMLError):
        lines = text.splitlines()
        pm = getattr(exc, "problem_mark", None)
        mark = None if pm is None else {"line": pm.line, "column": pm.column, "index": pm.index}
        return {"problem": lines[0] if lines else "", "full": text, "custom": False, "py_exc": None,
                "problem_mark": mark}
    name = type(exc).__name__
    return {"problem": text, "full": f"{name}: {text}", "custom": False, "py_exc": name, "problem_mark": None}
