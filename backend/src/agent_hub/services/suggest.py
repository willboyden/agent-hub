"""Deterministic collection suggestions over the content catalog: common name prefixes, provenance, shared tags and a small
keyword table. No I/O, no randomness: the same content always yields the same ids, titles and order."""
from __future__ import annotations

import hashlib
import re
from typing import Any

from agent_hub.domain.ids import slugify
from agent_hub.services.content import Catalog, Item

MIN_MEMBERS, MAX_MEMBERS = 3, 40
PALETTE = ["#4f8cff", "#2fbf71", "#f5a524", "#e5484d", "#8e4ec6", "#12a594", "#d6409f", "#6e7681"]
SPECIAL = {"api": "API", "mcp": "MCP", "llm": "LLM", "gpu": "GPU", "cuda": "CUDA",
           "vllm": "vLLM", "sglang": "SGLang", "adr": "ADR", "ui": "UI", "ci": "CI", "pr": "PR", "hf": "HF"}
KEYWORDS: dict[str, tuple[str, set[str], str]] = {
    "security": ("Security", {"security", "secret", "secrets", "audit", "sandbox", "egress", "harden", "hardening", "threat", "vuln"}, "shield"),
    "docs": ("Documentation", {"doc", "docs", "documentation", "readme", "adr", "guide", "changelog"}, "book"),
    "benchmark": ("Benchmarking", {"bench", "benchmark", "perf", "performance", "eval", "latency", "throughput"}, "gauge"),
    "deploy": ("Deploy and release", {"deploy", "release", "publish", "rollout", "ship", "install"}, "rocket"),
    "troubleshoot": ("Troubleshooting", {"debug", "troubleshoot", "diagnose", "incident", "triage", "fix", "monitor"}, "wrench"),
}
ORDER = {"common_prefix": 0, "source": 1, "tag": 2, "keyword": 3}


def _title(text: str) -> str:
    return " ".join(SPECIAL.get(t, t.capitalize()) for t in re.split(r"[-_.\s]+", text) if t)


def _colour(cid: str) -> str:
    return PALETTE[int(hashlib.sha256(cid.encode()).hexdigest(), 16) % len(PALETTE)]


def _words(item: Item) -> set[str]:
    return set(re.findall(r"[a-z0-9]+", f"{item.name} {item.description[:300]}".lower()))


def suggest(cat: Catalog) -> list[dict[str, Any]]:
    items = [i for i in cat.items.values() if i.valid]
    raw: list[tuple[str, str, str, str, str, list[Item]]] = []      # (reason, id, title, description, icon, members)

    # 1. common hyphen prefixes (one or two tokens)
    groups: dict[str, list[Item]] = {}
    for it in items:
        if it.kind == "rule" and it.name.startswith("imp-"):       # generated ids of imported rules carry no meaning
            continue
        toks = it.name.split("-")
        for n in (1, 2):
            if len(toks) >= n:
                pre = "-".join(toks[:n])
                if len(pre) >= 3 and not pre.isdigit():
                    groups.setdefault(pre, []).append(it)
    for pre, members in groups.items():
        members = [m for m in members if m.name == pre or m.name.startswith(pre + "-")]
        if len({(m.kind, m.name) for m in members}) >= MIN_MEMBERS:
            raw.append(("common_prefix", slugify(pre), _title(pre),
                        f"Items whose names start with '{pre}'", "folder", members))
    # 2. provenance (skills carry hub.yaml `source`)
    by_src: dict[str, list[Item]] = {}
    for it in items:
        if it.source:
            by_src.setdefault(it.source, []).append(it)
    for src, members in by_src.items():
        label = src.split(":", 1)[-1] or src
        raw.append(("source", slugify(f"from-{label}"), f"From {_title(label)}",
                    f"Imported from the same source ({src})", "download", members))
    # 3. shared tags
    by_tag: dict[str, list[Item]] = {}
    for it in items:
        for t in it.tags:
            if t != "imported":
                by_tag.setdefault(t, []).append(it)
    for tag, members in by_tag.items():
        raw.append(("tag", slugify(f"tag-{tag}"), f"Tagged {_title(tag)}", f"Items tagged '{tag}'", "tag", members))
    # 4. keyword table
    for key, (title, words, icon) in sorted(KEYWORDS.items()):
        members = [it for it in items if _words(it) & words]
        raw.append(("keyword", key, title, f"Items that mention {', '.join(sorted(words)[:4])}...", icon, members))

    out: list[dict[str, Any]] = []
    seen_sets: set[frozenset[tuple[str, str]]] = set()
    seen_ids: set[str] = set(cat.collections)
    for reason, cid, title, desc, icon, members in sorted(raw, key=lambda r: (ORDER[r[0]], -len(r[1]), r[1])):     # more specific prefix wins a tie
        uniq = sorted({(m.kind, m.name) for m in members})
        if not cid or cid in seen_ids or not len(uniq) >= MIN_MEMBERS or cid in cat.collections:
            continue
        uniq = uniq[:MAX_MEMBERS]
        key_set = frozenset(uniq)
        if key_set in seen_sets:                                    # same members under a weaker reason: keep the first
            continue
        seen_sets.add(key_set)
        seen_ids.add(cid)
        out.append({"id": cid, "title": title, "description": desc, "icon": icon, "color": _colour(cid), "reason": reason,
                    "members": [{"kind": k, "name": n} for k, n in uniq]})
    return sorted(out, key=lambda x: (ORDER[x["reason"]], x["id"]))
