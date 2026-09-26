"""Console entry point: `agent-knowledge [--config FILE]`. Binds loopback only."""
from __future__ import annotations

import argparse
import logging

import uvicorn

from agent_knowledge.app import create_app
from agent_knowledge.config import load_config


def run() -> None:
    ap = argparse.ArgumentParser(prog="agent-knowledge")
    ap.add_argument("--config", help="YAML config (default: $AGENT_KNOWLEDGE_CONFIG or built-in defaults)")
    ap.add_argument("--allow-bind-any", action="store_true",
                    help="bind the configured non-loopback host (ONLY inside a container on an internal network)")
    args = ap.parse_args()
    cfg = load_config(args.config)
    if cfg.host not in ("127.0.0.1", "::1", "localhost") and not args.allow_bind_any:
        raise SystemExit("refusing a non-loopback host without --allow-bind-any (container use only; front it with a broker)")
    logging.basicConfig(level=logging.INFO)
    uvicorn.run(create_app(cfg), host=cfg.host, port=cfg.port, log_level="info", server_header=False)


if __name__ == "__main__":
    run()
