"""Metadata filter grammar. Validated into an AST; backends translate the AST, never the raw input.

Grammar (JSON):
  filter  := {"$and": [filter,..]} | {"$or": [filter,..]} | {"$not": filter} | {field: cond, ...}   (multi-key = AND)
  cond    := scalar (equality) | {"$eq"|"$ne"|"$gt"|"$gte"|"$lt"|"$lte": scalar} | {"$in": [scalar,..]}
  scalar  := string (<=512 chars) | int | float | bool
Field names fullmatch [A-Za-z_][A-Za-z0-9_]{0,63}. Depth <= 4, <= 32 nodes, <= 64 values per $in."""
from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Any

FIELD = re.compile(r"[A-Za-z_][A-Za-z0-9_]{0,63}")
Scalar = str | int | float | bool
CMP_OPS = {"$eq", "$ne", "$gt", "$gte", "$lt", "$lte"}
MAX_DEPTH, MAX_NODES, MAX_IN, MAX_STR = 4, 32, 64, 512


class FilterError(ValueError):
    pass


@dataclass(frozen=True)
class Cmp:
    field: str
    op: str  # eq ne gt gte lt lte
    value: Scalar


@dataclass(frozen=True)
class In:
    field: str
    values: tuple[Scalar, ...]


@dataclass(frozen=True)
class And:
    items: tuple[Node, ...]


@dataclass(frozen=True)
class Or:
    items: tuple[Node, ...]


@dataclass(frozen=True)
class Not:
    item: Node


Node = Cmp | In | And | Or | Not


def _scalar(v: Any) -> Scalar:
    if isinstance(v, bool | int | float):
        return v
    if isinstance(v, str) and len(v) <= MAX_STR:
        return v
    raise FilterError("filter values must be string (<=512 chars), number or boolean")


def parse_filter(raw: Any, max_nodes: int = MAX_NODES) -> Node:
    count = 0

    def walk(node: Any, depth: int) -> Node:
        nonlocal count
        count += 1
        if depth > MAX_DEPTH or count > max_nodes:
            raise FilterError("filter too deep or too large")
        if not isinstance(node, dict) or not node:
            raise FilterError("a filter must be a non-empty object")
        parts: list[Node] = []
        for key, val in node.items():
            if key in ("$and", "$or"):
                if not isinstance(val, list) or not val:
                    raise FilterError(f"{key} needs a non-empty list")
                kids = tuple(walk(v, depth + 1) for v in val)
                parts.append(And(kids) if key == "$and" else Or(kids))
            elif key == "$not":
                parts.append(Not(walk(val, depth + 1)))
            elif isinstance(key, str) and FIELD.fullmatch(key):
                count += 1
                if isinstance(val, dict):
                    if len(val) != 1:
                        raise FilterError("one operator per field condition")
                    ((op, arg),) = val.items()
                    if op == "$in":
                        if not isinstance(arg, list) or not arg or len(arg) > MAX_IN:
                            raise FilterError("$in needs a list of 1..64 values")
                        parts.append(In(key, tuple(_scalar(a) for a in arg)))
                    elif op in CMP_OPS:
                        parts.append(Cmp(key, op[1:], _scalar(arg)))
                    else:
                        raise FilterError("unknown operator")
                else:
                    parts.append(Cmp(key, "eq", _scalar(val)))
            else:
                raise FilterError("invalid field name or operator")
        return parts[0] if len(parts) == 1 else And(tuple(parts))

    return walk(raw, 1)


def evaluate(node: Node, meta: dict[str, Any]) -> bool:
    """Reference semantics, used by the in-memory store and the LanceDB id-resolution step."""
    if isinstance(node, And):
        return all(evaluate(i, meta) for i in node.items)
    if isinstance(node, Or):
        return any(evaluate(i, meta) for i in node.items)
    if isinstance(node, Not):
        return not evaluate(node.item, meta)
    if isinstance(node, In):
        return node.field in meta and any(_eq(meta[node.field], v) for v in node.values)
    if node.field not in meta:
        return node.op == "ne"
    have = meta[node.field]
    if node.op == "eq":
        return _eq(have, node.value)
    if node.op == "ne":
        return not _eq(have, node.value)
    if isinstance(have, bool) or isinstance(node.value, bool):
        return False
    try:
        if node.op == "gt":
            return bool(have > node.value)
        if node.op == "gte":
            return bool(have >= node.value)
        if node.op == "lt":
            return bool(have < node.value)
        return bool(have <= node.value)
    except TypeError:
        return False


def _eq(a: Any, b: Any) -> bool:
    if isinstance(a, bool) != isinstance(b, bool):
        return False
    return bool(a == b)


def to_qdrant(node: Node) -> dict[str, Any]:
    """Structured JSON only: field keys are regex-validated, values are JSON scalars in typed slots."""

    def key(f: str) -> str:
        return f"meta.{f}"

    def cmp_(c: Cmp) -> dict[str, Any]:
        if c.op in ("eq", "ne"):
            if isinstance(c.value, float):
                raise FilterError("qdrant cannot match floats exactly; use a range operator")
            cond = {"key": key(c.field), "match": {"value": c.value}}
            return cond if c.op == "eq" else {"must_not": [cond]}
        if isinstance(c.value, bool | str):
            raise FilterError("range operators need a number")
        return {"key": key(c.field), "range": {c.op: c.value}}

    def go(n: Node) -> dict[str, Any]:
        if isinstance(n, Cmp):
            return {"must": [cmp_(n)]}
        if isinstance(n, In):
            if any(isinstance(v, float) for v in n.values):
                raise FilterError("qdrant cannot match floats exactly; use a range operator")
            return {"should": [{"key": key(n.field), "match": {"value": v}} for v in n.values]}
        if isinstance(n, And):
            return {"must": [go(i) for i in n.items]}
        if isinstance(n, Or):
            return {"should": [go(i) for i in n.items]}
        return {"must_not": [go(n.item)]}

    return go(node)
