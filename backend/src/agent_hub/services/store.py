"""SQLite (WAL) store, 0600. One connection behind a lock: single node, short queries. Schema migrations are inline
and append-only. The audit table rejects UPDATE/DELETE via triggers."""
from __future__ import annotations

import sqlite3
import threading
from collections.abc import Sequence
from pathlib import Path
from typing import Any

MIGRATIONS: list[tuple[str, list[str]]] = [
    ("0001_init", [
        "CREATE TABLE audit (id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL NOT NULL, actor TEXT NOT NULL, role TEXT NOT NULL,"
        " method TEXT NOT NULL, path TEXT NOT NULL, status INTEGER NOT NULL, params TEXT NOT NULL)",
        "CREATE TRIGGER audit_no_update BEFORE UPDATE ON audit BEGIN SELECT RAISE(ABORT, 'audit is append-only'); END",
        "CREATE TRIGGER audit_no_delete BEFORE DELETE ON audit BEGIN SELECT RAISE(ABORT, 'audit is append-only'); END",
        "CREATE TABLE api_keys (id TEXT PRIMARY KEY, name TEXT NOT NULL, role TEXT NOT NULL, prefix TEXT NOT NULL,"
        " hash TEXT NOT NULL UNIQUE, client TEXT, scopes TEXT NOT NULL DEFAULT '[]', created_at REAL NOT NULL,"
        " last_used_at REAL, revoked_at REAL)",
        "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    ]),
]


class Store:
    def __init__(self, path: Path | str) -> None:
        self._lock = threading.RLock()
        if str(path) != ":memory:":
            Path(path).parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        self._conn = sqlite3.connect(str(path), check_same_thread=False, isolation_level=None)
        self._conn.row_factory = sqlite3.Row
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._conn.execute("PRAGMA synchronous=NORMAL")
        if str(path) != ":memory:":
            Path(path).chmod(0o600)
        self.migrate()

    def migrate(self) -> list[str]:
        applied: list[str] = []
        with self._lock:
            self._conn.execute("CREATE TABLE IF NOT EXISTS schema_migrations (name TEXT PRIMARY KEY, applied_at REAL)")
            done = {r["name"] for r in self._conn.execute("SELECT name FROM schema_migrations")}
            for name, stmts in MIGRATIONS:
                if name in done:
                    continue
                try:
                    self._conn.execute("BEGIN")
                    for s in stmts:
                        self._conn.execute(s)
                    self._conn.execute("INSERT INTO schema_migrations VALUES (?, strftime('%s','now'))", (name,))
                    self._conn.execute("COMMIT")
                except Exception:
                    self._conn.execute("ROLLBACK")
                    raise
                applied.append(name)
        return applied

    def execute(self, sql: str, params: Sequence[Any] = ()) -> sqlite3.Cursor:
        with self._lock:
            return self._conn.execute(sql, params)

    def one(self, sql: str, params: Sequence[Any] = ()) -> sqlite3.Row | None:
        with self._lock:
            row: sqlite3.Row | None = self._conn.execute(sql, params).fetchone()
            return row

    def all(self, sql: str, params: Sequence[Any] = ()) -> list[sqlite3.Row]:
        with self._lock:
            return list(self._conn.execute(sql, params).fetchall())

    def close(self) -> None:
        with self._lock:
            self._conn.close()


def paginate(limit: int, cursor: str | None) -> tuple[int, int]:
    """Opaque cursor == integer offset. Returns (limit, offset); callers fetch limit+1 to detect a next page."""
    try:
        offset = max(0, int(cursor)) if cursor else 0
    except ValueError:
        offset = 0
    return max(1, min(limit, 500)), offset


def page_list(items: list[Any], limit: int, cursor: str | None) -> dict[str, Any]:
    limit, off = paginate(limit, cursor)
    chunk = items[off: off + limit]
    return {"items": chunk, "next_cursor": str(off + limit) if off + limit < len(items) else None}
