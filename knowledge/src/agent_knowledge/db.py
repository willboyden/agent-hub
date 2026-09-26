"""Small SQLite registry (namespaces, hashed tokens, daily usage, shared-index registry). The DB and the bootstrap admin
key live under data_dir with 0700/0600 permissions."""
from __future__ import annotations

import hashlib
import json
import os
import secrets
import sqlite3
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

SCHEMA = """
CREATE TABLE IF NOT EXISTS namespaces(name TEXT PRIMARY KEY, backend TEXT NOT NULL, embedding_model TEXT NOT NULL,
  dim INTEGER NOT NULL, description TEXT NOT NULL DEFAULT '', created REAL NOT NULL);
CREATE TABLE IF NOT EXISTS tokens(id TEXT PRIMARY KEY, client TEXT NOT NULL, hash TEXT NOT NULL UNIQUE,
  scopes TEXT NOT NULL, rate_per_min INTEGER NOT NULL, daily_queries INTEGER NOT NULL, daily_writes INTEGER NOT NULL,
  created REAL NOT NULL);
CREATE TABLE IF NOT EXISTS usage(token_id TEXT NOT NULL, day TEXT NOT NULL, queries INTEGER NOT NULL DEFAULT 0,
  writes INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(token_id, day));
CREATE TABLE IF NOT EXISTS indexes(name TEXT PRIMARY KEY, kind TEXT NOT NULL, path TEXT NOT NULL, project TEXT NOT NULL);
"""


def hash_secret(secret: str) -> str:
    # Secrets are 256-bit random, so a fast unsalted hash is sufficient (no brute-force surface).
    return hashlib.sha256(secret.encode()).hexdigest()


@dataclass
class Principal:
    id: str
    client: str
    admin: bool
    scopes: dict[str, str]  # ns -> "read" | "write"
    rate_per_min: int = 0
    daily_queries: int = 0
    daily_writes: int = 0

    def can(self, ns: str, mode: str) -> bool:
        if self.admin:
            return True
        have = self.scopes.get(ns)
        return have == "write" or (have == "read" and mode == "read")


class Db:
    def __init__(self, data_dir: Path) -> None:
        data_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        os.chmod(data_dir, 0o700)
        self.dir = data_dir
        self._lock = threading.RLock()
        self._c = sqlite3.connect(str(data_dir / "knowledge.db"), check_same_thread=False, isolation_level=None)
        self._c.row_factory = sqlite3.Row
        self._c.executescript(SCHEMA)
        os.chmod(data_dir / "knowledge.db", 0o600)

    def q(self, sql: str, args: tuple[Any, ...] = ()) -> list[sqlite3.Row]:
        with self._lock:
            return self._c.execute(sql, args).fetchall()

    def x(self, sql: str, args: tuple[Any, ...] = ()) -> int:
        with self._lock:
            return self._c.execute(sql, args).rowcount

    # --- bootstrap admin key ---------------------------------------------------------------------------------
    def admin_key(self) -> str:
        """Create (once) and return the bootstrap admin key; the file is 0600 and only its hash is kept in memory."""
        p = self.dir / "admin.key"
        if not p.exists():
            fd = os.open(p, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "w") as f:
                f.write("akn_admin_" + secrets.token_urlsafe(32) + "\n")
        os.chmod(p, 0o600)
        return p.read_text().strip()

    # --- tokens -----------------------------------------------------------------------------------------------
    def add_token(self, client: str, scopes: list[dict[str, str]], rate: int, dq: int, dw: int) -> tuple[str, str]:
        tid = secrets.token_hex(6)
        secret = f"akn_{tid}_{secrets.token_urlsafe(32)}"
        self.x("INSERT INTO tokens VALUES(?,?,?,?,?,?,?,?)",
               (tid, client, hash_secret(secret), json.dumps(scopes), rate, dq, dw, time.time()))
        return tid, secret

    def token_by_secret(self, secret: str) -> Principal | None:
        rows = self.q("SELECT * FROM tokens WHERE hash=?", (hash_secret(secret),))
        return self._principal(rows[0]) if rows else None

    @staticmethod
    def _principal(r: sqlite3.Row) -> Principal:
        return Principal(r["id"], r["client"], False, {s["ns"]: s["mode"] for s in json.loads(r["scopes"])},
                         r["rate_per_min"], r["daily_queries"], r["daily_writes"])

    def list_tokens(self) -> list[dict[str, Any]]:
        return [{"id": r["id"], "client": r["client"], "scopes": json.loads(r["scopes"]),
                 "rate_per_min": r["rate_per_min"], "daily_queries": r["daily_queries"],
                 "daily_writes": r["daily_writes"], "created": r["created"]}
                for r in self.q("SELECT * FROM tokens ORDER BY created")]

    def drop_token(self, tid: str) -> bool:
        self.x("DELETE FROM usage WHERE token_id=?", (tid,))
        return self.x("DELETE FROM tokens WHERE id=?", (tid,)) > 0

    def drop_ns_scopes(self, ns: str) -> None:
        """A dropped namespace must not leave live grants: a later namespace of the same name would inherit them."""
        for r in self.q("SELECT id, scopes FROM tokens"):
            kept = [s for s in json.loads(r["scopes"]) if s["ns"] != ns]
            self.x("UPDATE tokens SET scopes=? WHERE id=?", (json.dumps(kept), r["id"]))

    # --- usage ------------------------------------------------------------------------------------------------
    def bump(self, tid: str, field: str, n: int, limit: int) -> bool:
        """Atomically add n to today's counter; returns False (and does not add) if it would exceed the limit."""
        assert field in ("queries", "writes")
        day = time.strftime("%Y-%m-%d", time.gmtime())
        with self._lock:
            self._c.execute("INSERT OR IGNORE INTO usage(token_id, day) VALUES(?,?)", (tid, day))
            cur = self._c.execute(f"SELECT {field} FROM usage WHERE token_id=? AND day=?", (tid, day)).fetchone()[0]  # noqa: S608
            if cur + n > limit:
                return False
            self._c.execute(f"UPDATE usage SET {field}={field}+? WHERE token_id=? AND day=?", (n, tid, day))  # noqa: S608
            return True

    def usage(self, tid: str) -> dict[str, int]:
        day = time.strftime("%Y-%m-%d", time.gmtime())
        r = self.q("SELECT queries, writes FROM usage WHERE token_id=? AND day=?", (tid, day))
        return {"queries": r[0][0], "writes": r[0][1]} if r else {"queries": 0, "writes": 0}
