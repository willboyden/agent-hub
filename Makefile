# Agent Hub developer tasks. Nothing here applies anything to a client, touches Docker or systemd, or uses the network
# beyond what `uv sync` needs. `make run` starts the server against your real state dir and content repo: read
# docs/OPERATIONS.md first. Backend and knowledge use uv + Python 3.13 (3.14 lacks wheels for some dependencies).
SHELL := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c
BACKEND   := backend
KNOWLEDGE := knowledge
FRONTEND  := frontend

.PHONY: help setup setup-knowledge run run-knowledge mock test knowledge-test knowledge-live lint typecheck frontend-test all

help:  ## list targets
	@grep -E '^[a-z-]+:.*##' $(MAKEFILE_LIST) | sed 's/:.*## /\t/'

setup:  ## create the backend venv and install runtime + dev deps from uv.lock
	cd $(BACKEND) && uv venv --python 3.13 && uv sync --frozen

setup-knowledge:  ## create the knowledge-service venv from its uv.lock
	cd $(KNOWLEDGE) && uv venv --python 3.13 && uv sync --frozen

run:  ## serve API + UI on 127.0.0.1:8792 (foreground; uses HUB_* env and ~/.config/agent-hub/env)
	cd $(BACKEND) && uv run --frozen agent-hub

run-knowledge:  ## serve the knowledge service on 127.0.0.1:8795 (foreground; defaults, or AGENT_KNOWLEDGE_CONFIG)
	cd $(KNOWLEDGE) && uv run --frozen agent-knowledge

mock:  ## contract-mock API + static UI on 127.0.0.1:8794 (invented data, no backend needed)
	cd $(FRONTEND) && node dev/mock-server.mjs

test:  ## backend pytest (offline: fakes, plus live self-tests in temp dirs)
	cd $(BACKEND) && uv run --frozen pytest -q

knowledge-test:  ## knowledge pytest (offline; the `live` marker is skipped without AK_LIVE=1)
	cd $(KNOWLEDGE) && uv run --frozen pytest -q

knowledge-live:  ## needs AK_LIVE_QDRANT_URL of a THROWAWAY Qdrant; runs the live-marked tests
	cd $(KNOWLEDGE) && AK_LIVE=1 uv run --frozen pytest -q -m live

lint:  ## ruff on both Python packages
	cd $(BACKEND) && uv run --frozen ruff check src tests
	cd $(KNOWLEDGE) && uv run --frozen ruff check src tests

typecheck:  ## mypy --strict on the backend and the knowledge service
	cd $(BACKEND) && uv run --frozen mypy src
	cd $(KNOWLEDGE) && uv run --frozen mypy src

frontend-test:  ## node --test on the pure-logic modules (no npm install needed)
	cd $(FRONTEND) && node --test tests/*.test.mjs

all: lint typecheck test knowledge-test frontend-test  ## everything the quality bar requires
