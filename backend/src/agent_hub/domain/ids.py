"""Identifier rules shared by every content kind. Always `fullmatch`: `match`/`search` would accept trailing junk."""
from __future__ import annotations

import re

ID_RE = re.compile(r"[a-z0-9][a-z0-9._-]{0,63}")
ENV_NAME_RE = re.compile(r"[A-Z_][A-Z0-9_]{0,127}")
HOST_RE = re.compile(r"(\*\.)?[A-Za-z0-9*]([A-Za-z0-9.*-]{0,251}[A-Za-z0-9*])?")
COMMIT_RE = re.compile(r"[0-9a-f]{7,40}")

KINDS: tuple[str, ...] = ("skill", "agent", "instruction", "mcp", "rule", "memory")
PLURAL = {"skill": "skills", "agent": "agents", "instruction": "instructions", "mcp": "mcp", "rule": "rules",
          "memory": "memory"}
FROM_PLURAL = {v: k for k, v in PLURAL.items()}
# which manage-flag ("concern") governs each item kind
CONCERN_OF_KIND = {"skill": "skills", "agent": "agents", "instruction": "instructions", "mcp": "mcp",
                   "rule": "permissions", "memory": "memory"}
CONCERNS: tuple[str, ...] = ("skills", "agents", "instructions", "mcp", "permissions", "memory")
# Defaults follow ARCHITECTURE decision 5: the operator's existing sync scripts stay authoritative for mcp/permissions.
DEFAULT_MANAGE = {"skills": True, "agents": True, "instructions": True, "mcp": False, "permissions": False,
                  "memory": False}


def valid_id(value: object) -> bool:
    return isinstance(value, str) and ID_RE.fullmatch(value) is not None


def check_ident(value: str) -> str:
    if not isinstance(value, str) or ID_RE.fullmatch(value) is None:
        raise ValueError("must match ^[a-z0-9][a-z0-9._-]{0,63}$")
    return value


def slugify(text: str) -> str:
    s = re.sub(r"[^a-z0-9._-]+", "-", text.lower()).strip("-._")
    return s[:64].strip("-._")
