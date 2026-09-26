"""Merge engine for one target file: `own`, `json_keys`, `yaml_keys`, `block`. Pure (bytes in, bytes out).

Every mode reduces the file to a *slice* (the part the hub owns): the whole file, the managed keys, or the marker
region. Conflict detection compares slice hashes, so unmanaged content elsewhere in the file may change freely.
`content=None` on a Desired means 'the hub no longer wants this slice' (removal)."""
from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any

import yaml

from agent_hub.domain import yamlsafe
from agent_hub.services.common import sha256

MD_BEGIN_PREFIX = "<!-- agent-hub:begin"
MD_END = "<!-- agent-hub:end -->"
HASH_BEGIN_PREFIX = "# agent-hub:begin"
HASH_END = "# agent-hub:end"
_MARK = {"md": (MD_BEGIN_PREFIX, MD_END), "hash": (HASH_BEGIN_PREFIX, HASH_END)}


def find_region(text: str, style: str) -> tuple[int, int] | None | str:
    """(start, end) char offsets of the marker region (end includes its newline), None when absent, 'broken' when a
    begin marker has no end marker."""
    begin, end = _MARK[style]
    pos = 0
    start: int | None = None
    for line in text.splitlines(keepends=True):
        if start is None and line.startswith(begin):
            start = pos
        elif start is not None and line.rstrip() == end:
            return start, pos + len(line)
        pos += len(line)
    return "broken" if start is not None else None


@dataclass
class Desired:
    root: str
    path: str
    kind: str
    merge: str
    source_ids: list[str]
    mode: int
    managed_keys: list[str]
    content: bytes | None                    # None: remove the hub's slice
    managed: bool = True

    @property
    def key(self) -> str:
        return f"{self.root}:{self.path}"


@dataclass
class Merged:
    new: bytes | None = None                 # bytes to write; None: nothing to write / delete the file
    slice_live: str | None = None            # digest of the hub-owned slice as found (None: absent)
    slice_new: str | None = None             # digest of the slice after the merge (None: absent)
    unparseable: str = ""                    # non-empty: the live file cannot be merged safely
    empty_after: bool = False                # the result would be an empty document/file
    block_style: str = ""


def _canon(obj: Any) -> str:
    return sha256(json.dumps(obj, sort_keys=True, separators=(",", ":"), default=str).encode())


def get_dotted(doc: Any, path: str) -> tuple[bool, Any]:
    cur = doc
    for p in path.split("."):
        if not isinstance(cur, dict) or p not in cur:
            return False, None
        cur = cur[p]
    return True, cur


def set_dotted(doc: dict[str, Any], path: str, value: Any) -> None:
    cur = doc
    parts = path.split(".")
    for p in parts[:-1]:
        nxt = cur.get(p)
        if not isinstance(nxt, dict):
            nxt = {}
            cur[p] = nxt
        cur = nxt
    cur[parts[-1]] = value


def del_dotted(doc: dict[str, Any], path: str) -> None:
    """Delete a key and prune parents that this deletion left empty."""
    parts = path.split(".")
    chain: list[dict[str, Any]] = [doc]
    for p in parts[:-1]:
        nxt = chain[-1].get(p)
        if not isinstance(nxt, dict):
            return
        chain.append(nxt)
    chain[-1].pop(parts[-1], None)
    for i in range(len(parts) - 1, 0, -1):
        if not chain[i]:
            chain[i - 1].pop(parts[i - 1], None)


def _extract(doc: dict[str, Any], keys: list[str]) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for k in sorted(keys):
        ok, v = get_dotted(doc, k)
        if ok:
            out[k] = v
    return out


def _load_doc(raw: bytes, fmt: str) -> dict[str, Any]:
    text = raw.decode("utf-8")
    if not text.strip():
        return {}
    data = json.loads(text) if fmt == "json" else yamlsafe.load(text, max_bytes=8 * 1024 * 1024)
    if data is None:
        return {}
    if not isinstance(data, dict):
        raise ValueError("top level is not a mapping")
    return data


def _dump_doc(doc: dict[str, Any], fmt: str) -> bytes:
    if fmt == "json":
        return (json.dumps(doc, indent=2, ensure_ascii=False) + "\n").encode("utf-8")
    return yaml.safe_dump(doc, sort_keys=False, allow_unicode=True, default_flow_style=False, width=100000).encode("utf-8")


def _merge_keys(ds: list[Desired], live: bytes | None, fmt: str, adopt_broken: bool) -> Merged:
    keys = sorted({k for d in ds for k in d.managed_keys})
    m = Merged()
    try:
        doc = {} if live is None else _load_doc(live, fmt)
    except (ValueError, UnicodeDecodeError, yamlsafe.YamlError) as exc:
        if not adopt_broken:
            m.unparseable = f"live file is not a mergeable {fmt} document ({type(exc).__name__})"
            return m
        doc = {}
    m.slice_live = _canon(_extract(doc, keys)) if live is not None and _extract(doc, keys) else None
    for d in ds:
        try:
            src = {} if d.content is None else _load_doc(d.content, fmt)
        except (ValueError, UnicodeDecodeError, yamlsafe.YamlError) as exc:
            m.unparseable = f"rendered artifact is not a valid {fmt} document ({type(exc).__name__})"
            return m
        for k in d.managed_keys:
            ok, v = get_dotted(src, k)
            if ok:
                set_dotted(doc, k, v)
            else:
                del_dotted(doc, k)
    new_slice = _extract(doc, keys)
    m.slice_new = _canon(new_slice) if new_slice else None
    m.empty_after = not doc
    m.new = None if (live is None and not doc) else _dump_doc(doc, fmt)
    return m


def _style(content: bytes) -> str:
    return "md" if content.lstrip().startswith(MD_BEGIN_PREFIX.encode()) else "hash"


def _merge_block(ds: list[Desired], live: bytes | None, adopt_broken: bool) -> Merged:
    m = Merged()
    d = ds[0]
    style = _style(d.content) if d.content is not None else ("md" if d.path.endswith((".md", ".markdown")) else "hash")
    m.block_style = style
    text = "" if live is None else live.decode("utf-8", "replace")
    found = find_region(text, style)
    if found == "broken":
        if not adopt_broken:
            m.unparseable = "hub begin marker without a matching end marker"
            return m
        text = text[: text.index(_MARK[style][0])]          # adopt: drop the broken tail
        found = None
    span = found if isinstance(found, tuple) else None
    region = text[span[0]:span[1]] if span else None
    if region is not None and not region.endswith("\n"):
        region += "\n"
    m.slice_live = sha256(region.encode()) if region is not None else None
    if d.content is None:
        m.slice_new = None
        new_text = text if span is None else text[: span[0]].rstrip("\n") + ("\n\n" if text[: span[0]].strip() and text[span[1]:].strip() else "\n" if text[: span[0]].strip() else "") + text[span[1]:].lstrip("\n")
        if not new_text.strip():
            new_text = ""
    else:
        block = d.content.decode("utf-8")
        if not block.endswith("\n"):
            block += "\n"
        m.slice_new = sha256(block.encode())
        if span is not None:
            new_text = text[: span[0]] + block + text[span[1]:]
        else:
            new_text = text.rstrip("\n") + ("\n\n" if text.strip() else "") + block
    m.empty_after = not new_text.strip()
    m.new = None if (live is None and d.content is None) else new_text.encode("utf-8")
    return m


def merge_file(ds: list[Desired], live: bytes | None, *, adopt_broken: bool = False) -> Merged:
    mode = ds[0].merge
    if mode == "own":
        d = ds[0]
        m = Merged(slice_live=sha256(live) if live is not None else None)
        if d.content is not None:
            m.new, m.slice_new = d.content, sha256(d.content)
        return m
    if mode == "json_keys":
        return _merge_keys(ds, live, "json", adopt_broken)
    if mode == "yaml_keys":
        return _merge_keys(ds, live, "yaml", adopt_broken)
    if mode == "block":
        return _merge_block(ds, live, adopt_broken)
    return Merged(unparseable=f"unknown merge mode {mode!r}")
