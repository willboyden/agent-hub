"""API keys and roles (admin / viewer / client). Keys are 256-bit random, so a fast unsalted hash is fine (there is
no dictionary to attack); only the hash is stored. The first start writes a bootstrap admin key (0600)."""
from __future__ import annotations

import hashlib
import hmac
import json
import secrets
from dataclasses import dataclass, field
from pathlib import Path

from agent_hub.domain.errors import BadRequest, NotFound, Unprocessable
from agent_hub.domain.ids import valid_id
from agent_hub.services.common import new_id, now, write_secret_file
from agent_hub.services.store import Store

BOOTSTRAP_KEY_FILE = "bootstrap-admin.key"
ROLES = ("admin", "viewer", "client")
SCOPES = ("inbox:write",)     # knowledge access uses tokens issued by the knowledge service, not the hub


@dataclass(frozen=True)
class Principal:
    name: str
    role: str
    key_id: str | None = None
    client: str | None = None
    scopes: tuple[str, ...] = field(default_factory=tuple)


def _hash(secret: str) -> str:
    return hashlib.sha256(secret.encode()).hexdigest()


class AuthService:
    def __init__(self, store: Store) -> None:
        self._db = store

    def create_key(self, name: str, role: str, client: str | None = None, scopes: list[str] | None = None) -> dict[str, object]:
        if role not in ROLES:
            raise BadRequest("role must be admin, viewer or client")
        scopes = scopes or []
        bad = [s[:40] for s in scopes if s not in SCOPES]
        if bad:
            raise Unprocessable(f"unknown scope(s) {bad}: the only hub token scope is inbox:write "
                                "(knowledge access uses tokens issued by the knowledge service)", code="unknown_scope")
        if role == "client" and (not client or not valid_id(client)):
            raise BadRequest("a client token needs a valid `client` id")
        if role != "client" and (client or scopes):
            raise BadRequest("client/scopes only apply to role=client")
        if not name.strip() or len(name) > 100:
            raise BadRequest("name must be 1..100 characters")
        secret = "hc_" + secrets.token_urlsafe(32)
        kid = new_id("key_")
        self._db.execute("INSERT INTO api_keys(id,name,role,prefix,hash,client,scopes,created_at) VALUES(?,?,?,?,?,?,?,?)",
                         (kid, name.strip(), role, secret[:8], _hash(secret), client, json.dumps(scopes), now()))
        return {**self._view(self._row(kid)), "secret": secret}      # the only time the secret is ever returned

    def _row(self, kid: str) -> dict[str, object]:
        r = self._db.one("SELECT * FROM api_keys WHERE id=?", (kid,))
        assert r is not None
        return dict(r)

    @staticmethod
    def _view(r: dict[str, object]) -> dict[str, object]:
        return {"id": r["id"], "name": r["name"], "role": r["role"], "prefix": r["prefix"], "client": r["client"],
                "scopes": json.loads(str(r["scopes"])), "created_at": r["created_at"], "last_used_at": r["last_used_at"]}

    def list_keys(self) -> list[dict[str, object]]:
        return [self._view(dict(r)) for r in self._db.all("SELECT * FROM api_keys WHERE revoked_at IS NULL ORDER BY created_at")]

    def revoke(self, key_id: str) -> None:
        cur = self._db.execute("UPDATE api_keys SET revoked_at=? WHERE id=? AND revoked_at IS NULL", (now(), key_id))
        if cur.rowcount == 0:
            raise NotFound(f"no such key: {key_id}")

    def authenticate(self, secret: str) -> Principal | None:
        h = _hash(secret)
        row = self._db.one("SELECT * FROM api_keys WHERE hash=? AND revoked_at IS NULL", (h,))
        if row is None or not hmac.compare_digest(row["hash"], h):
            return None
        self._db.execute("UPDATE api_keys SET last_used_at=? WHERE hash=?", (now(), h))
        return Principal(row["name"], row["role"], row["id"], row["client"], tuple(json.loads(row["scopes"])))

    def ensure_bootstrap_key(self, data_dir: Path) -> Path | None:
        """First start: mint an admin key and write it 0600. Returns the file path if one was created."""
        if self._db.one("SELECT 1 FROM api_keys LIMIT 1"):
            return None
        info = self.create_key("bootstrap-admin", "admin")
        path = data_dir / BOOTSTRAP_KEY_FILE
        write_secret_file(path, str(info["secret"]) + "\n")
        return path
