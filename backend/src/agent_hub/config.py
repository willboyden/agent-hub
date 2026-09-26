"""Process configuration: HUB_* environment variables plus an optional per-user env file.
Real environment variables win over the file. Fixed ~/.config and ~/.local/share paths (XDG vars are unreliable
under snap-packaged terminals). Tests replace ENV_FILE via an autouse fixture so the developer's file is never read."""
from __future__ import annotations

import os
from pathlib import Path
from typing import Any

from pydantic import AliasChoices, Field, model_validator
from pydantic_settings import BaseSettings, SettingsConfigDict

_HOME = Path.home()
APP_ROOT = Path(__file__).resolve().parents[3]          # .../hub
ENV_FILE = Path(os.environ.get("AGENT_HUB_ENV_FILE") or _HOME / ".config" / "agent-hub" / "env")


class Settings(BaseSettings):
    model_config = SettingsConfigDict(env_prefix="HUB_", extra="ignore", case_sensitive=False)

    host: str = "127.0.0.1"
    port: int = 8792
    content_dir: Path = Path()                                  # default: <data_dir>/content (private, outside any client's project tree)
    data_dir: Path = _HOME / ".local" / "share" / "agent-hub"
    policy_file: Path = APP_ROOT / "policy" / "floor.yaml"      # app repo, never content, never editable via the API
    frontend_dir: Path | None = None
    git_bin: str = "git"
    git_timeout_s: float = 30.0
    git_author_name: str = "Agent Hub"                          # fixed identity: the operator's global git config is never used
    git_author_email: str = "agent-hub@localhost"
    allowed_hosts: list[str] = []                               # extra Host header values (DNS-rebinding guard)
    trust_loopback: bool = False   # OFF: an unsandboxed agent with a shell could reach 127.0.0.1 and edit its own policy; a bearer key is required
    body_cap_bytes: int = 1024 * 1024
    import_body_cap_bytes: int = 4 * 1024 * 1024
    max_sse_connections: int = 8
    sse_idle_timeout_s: float = 300.0
    max_content_file_bytes: int = 256 * 1024
    max_skill_bytes: int = 4 * 1024 * 1024
    max_live_file_bytes: int = 8 * 1024 * 1024
    knowledge_url: str = "http://127.0.0.1:8795"
    knowledge_token: str | None = Field(default=None, repr=False)   # legacy name for knowledge_admin_key
    knowledge_admin_key: str | None = Field(default=None, repr=False)   # HUB_KNOWLEDGE_ADMIN_KEY; never logged or returned
    # the knowledge service writes this 0600 file on its first start; read lazily at proxy time when no key is set
    knowledge_admin_key_file: Path = Field(default=_HOME / ".local" / "share" / "agent-knowledge" / "admin.key",
                                           validation_alias=AliasChoices("KNOWLEDGE_ADMIN_KEY_FILE", "HUB_KNOWLEDGE_ADMIN_KEY_FILE"))
    knowledge_timeout_s: float = 30.0
    knowledge_body_cap_bytes: int = 1024 * 1024
    knowledge_response_cap_bytes: int = 8 * 1024 * 1024
    discover_extra_roots: list[str] = []                        # extra roots discover/verify may read (besides client roots)
    inbox_rate_per_min: int = 20
    inbox_max_per_client: int = 200
    plan_retention: int = 20
    enable_metrics: bool = True

    @model_validator(mode="after")
    def _default_content_dir(self) -> Settings:
        if "content_dir" not in self.model_fields_set:
            self.content_dir = self.data_dir / "content"
        return self

    @property
    def db_path(self) -> Path:
        return self.data_dir / "hub.db"


def check_bind(host: str) -> None:
    """Refuse a non-loopback bind unless the operator opts in explicitly (mirrors the knowledge service)."""
    if host in ("127.0.0.1", "::1", "localhost") or host.startswith("127."):
        return
    if os.environ.get("HUB_ALLOW_NONLOOPBACK") != "1":
        raise ValueError(f"refusing to bind {host!r}: the hub is loopback-only. Set HUB_ALLOW_NONLOOPBACK=1 to override "
                         "(and put an authenticating reverse proxy in front)")


def load_settings(**overrides: Any) -> Settings:
    env_file = ENV_FILE if ENV_FILE.is_file() else None
    return Settings(_env_file=env_file, **overrides)
