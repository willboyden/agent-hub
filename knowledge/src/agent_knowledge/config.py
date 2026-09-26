"""Configuration: an optional YAML file (AGENT_KNOWLEDGE_CONFIG), everything defaulted and validated.

Secrets are referenced by env var NAME only (`router_api_key_env`); the value is read at call time and never stored."""
from __future__ import annotations

import os
import re
from pathlib import Path

import yaml
from pydantic import BaseModel, ConfigDict, Field, field_validator

ENV_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]{0,63}")


class Config(BaseModel):
    model_config = ConfigDict(extra="forbid")

    host: str = "127.0.0.1"
    port: int = 8795
    allowed_hosts: list[str] = Field(default_factory=list)
    data_dir: Path = Path("~/.local/share/agent-knowledge").expanduser()
    lance_dir: Path | None = None  # defaults to <data_dir>/lance; point at an SSD path
    qdrant_url: str | None = None  # e.g. http://127.0.0.1:6333 ; None disables the qdrant backend
    qdrant_api_key_env: str | None = "KNOWLEDGE_QDRANT_API_KEY"  # env var NAME; Qdrant must enforce the same key
    qdrant_vectors_on_disk: bool = True
    router_url: str = "http://127.0.0.1:4000"
    router_api_key_env: str = "KNOWLEDGE_EMBED_API_KEY"  # embeddings-only router virtual key, NEVER the master key
    default_embedding_model: str = "text-embedding-3-small"  # example only; set to a model your endpoint serves
    ingest_roots: list[Path] = Field(default_factory=list)  # allowlist for `path` ingest (admin only)
    index_roots: list[Path] = Field(default_factory=list)  # allowlist for the shared-index registry
    body_cap_bytes: int = 4 * 1024 * 1024
    max_docs_per_request: int = 200
    max_doc_chars: int = 500_000
    max_files_per_job: int = 2000
    max_job_bytes: int = 200 * 1024 * 1024
    max_chunks_per_namespace: int = 2_000_000
    max_k: int = 100
    max_filter_nodes: int = 16
    lance_max_scan_rows: int = 200_000  # filtered scans over larger tables are refused (422); doc_id filters are exempt
    store_timeout_s: float = 20.0  # per store call; on expiry the caller gets 429, not a stall
    lance_lock_wait_s: float = 5.0
    lance_workers: int = 4
    max_query_chars: int = 8000
    max_jobs_kept: int = 200
    embed_batch: int = 64
    default_rate_per_min: int = 120
    default_daily_queries: int = 5000
    default_daily_writes: int = 20000  # chunks upserted per day per write token
    sse_max_seconds: int = 3600
    sse_max_streams: int = 8

    @field_validator("qdrant_api_key_env", "router_api_key_env")
    @classmethod
    def _env_name(cls, v: str | None) -> str | None:
        if v is not None and not ENV_NAME.fullmatch(v):
            raise ValueError("must be an environment variable NAME, not a value")
        return v

    @field_validator("router_url", "qdrant_url")
    @classmethod
    def _url(cls, v: str | None) -> str | None:
        if v is not None and not re.fullmatch(r"https?://[A-Za-z0-9._-]+(:\d+)?(/[A-Za-z0-9._/-]*)?", v):
            raise ValueError("must be a plain http(s) URL without credentials or query")
        return v.rstrip("/") if v else v

    @property
    def lance_path(self) -> Path:
        return self.lance_dir or self.data_dir / "lance"


def load_config(path: str | None = None) -> Config:
    path = path or os.environ.get("AGENT_KNOWLEDGE_CONFIG")
    raw: dict[str, object] = {}
    if path:
        loaded = yaml.safe_load(Path(path).read_text())
        if loaded is not None:
            if not isinstance(loaded, dict):
                raise ValueError("config must be a mapping")
            raw = loaded
    env_url = os.environ.get("KNOWLEDGE_ROUTER_URL")
    if env_url and "router_url" not in raw:  # an explicit config value wins over the env default
        raw["router_url"] = env_url
    cfg = Config.model_validate(raw)
    cfg.data_dir = cfg.data_dir.expanduser()
    return cfg
