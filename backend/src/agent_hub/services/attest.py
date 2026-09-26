"""MCP scan attestation. `scan_status: clean` is only honoured while `scan_digest` equals an HMAC-SHA256 (keyed with a
per-installation secret that lives outside the content repo) over the fields that decide what runs and where it connects.
Anyone who can edit content/*.yaml can change the fields but cannot forge the digest without the key."""
from __future__ import annotations

import hashlib
import hmac
import json
import os
import secrets
import stat
from pathlib import Path
from typing import Any

from agent_hub.domain.errors import ProblemError


def mcp_blob(command: Any, args: Any, url: Any, pinned_ref: Any, transport: Any) -> bytes:
    return json.dumps([command, list(args or []), url, pinned_ref, transport], sort_keys=True, separators=(",", ":")).encode()


class Attestor:
    def __init__(self, key_file: Path) -> None:
        self.key_file = key_file
        self._key: bytes | None = None

    def _load(self) -> bytes:
        if self._key is not None:
            return self._key
        f = self.key_file
        try:
            st = f.lstat()
        except FileNotFoundError:
            f.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            fd = os.open(f, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            with os.fdopen(fd, "w") as fh:
                fh.write(secrets.token_hex(32))
            st = f.lstat()
        if not stat.S_ISREG(st.st_mode) or st.st_mode & 0o077 or st.st_size > 4096:
            raise ProblemError(f"{f} must be a regular file with mode 0600 (it keys MCP scan attestations)",
                               code="attest_key_unusable", status=503)
        self._key = bytes.fromhex(f.read_text().strip())
        return self._key

    def digest(self, command: Any, args: Any, url: Any, pinned_ref: Any, transport: Any) -> str:
        return hmac.new(self._load(), mcp_blob(command, args, url, pinned_ref, transport), hashlib.sha256).hexdigest()

    def valid(self, digest: str | None, command: Any, args: Any, url: Any, pinned_ref: Any, transport: Any) -> bool:
        return bool(digest) and hmac.compare_digest(str(digest), self.digest(command, args, url, pinned_ref, transport))
