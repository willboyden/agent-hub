# Agent Hub developer tasks. Nothing here (except the opt-in *-desktop targets, which write launcher files) applies
# anything to a client, touches Docker or systemd, or uses the network beyond what cargo needs (crates.io, and the
# RustSec advisory database for `audit`); `crate-age` queries the crates.io API and `parity` may need uv's cache or the
# network for the Python reference. `make run` starts the server against your real state dir and content repo: read
# docs/OPERATIONS.md first. The backend is one Rust workspace in backend/ (hub, hubctl, knowledge service); its
# toolchain is pinned by backend/rust-toolchain.toml.
SHELL := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c
BACKEND  := backend
FRONTEND := frontend
CARGO    := cargo
RELEASE  := $(BACKEND)/target/release

.PHONY: help setup hooks build release run run-knowledge mock test knowledge-test knowledge-live fmt lint clippy typecheck \
        deny audit crate-age test-map parity frontend-test e2e all install-desktop install-desktop-icon uninstall-desktop

help:  ## list targets
	@grep -E '^[a-z-]+:.*##' $(MAKEFILE_LIST) | sed 's/:.*## /\t/'

setup:  ## build the backend (debug, locked dependencies) and enable the git hooks
	cd $(BACKEND) && $(CARGO) build --locked --workspace
	if git rev-parse --git-dir >/dev/null 2>&1; then $(MAKE) --no-print-directory hooks; fi

hooks:  ## use the versioned git hooks in .githooks (pre-push refuses code without a recorded e2e pass)
	git config core.hooksPath .githooks

build:  ## debug binaries: backend/target/debug/{agent-hub,hubctl,agent-knowledge}
	cd $(BACKEND) && $(CARGO) build --locked --workspace --bins

# Release binaries carry no path of the build machine: source paths in panic messages and debug data are rewritten
# (the checkout to /agent-hub, the cargo home to /cargo, the rest of $HOME to ~), and the build fails if one remains.
# Cargo's trim-paths would do this but is not stable on the pinned toolchain.
CARGO_HOME ?= $(HOME)/.cargo
REMAP := --remap-path-prefix=$(HOME)=~ --remap-path-prefix=$(CARGO_HOME)=/cargo --remap-path-prefix=$(CURDIR)=/agent-hub
RELEASE_BINS := agent-hub hubctl agent-knowledge

release:  ## optimised binaries for real use: backend/target/release/{agent-hub,hubctl,agent-knowledge}
	cd $(BACKEND) && RUSTFLAGS="$(strip $(RUSTFLAGS) $(REMAP))" $(CARGO) build --locked --release --workspace --bins
	for b in $(RELEASE_BINS); do \
	  for p in "$(CURDIR)/" "$(HOME)/"; do \
	    if [ "$$p" != "//" ] && grep -aqF "$$p" "$(RELEASE)/$$b"; then echo "release: $$b still contains the path $$p" >&2; exit 1; fi; \
	  done; \
	done

run: release  ## serve API + UI on 127.0.0.1:8792 (foreground; uses HUB_* env and ~/.config/agent-hub/env)
	$(RELEASE)/agent-hub

run-knowledge: release  ## serve the knowledge service on 127.0.0.1:8795 (foreground; defaults, or AGENT_KNOWLEDGE_CONFIG)
	$(RELEASE)/agent-knowledge

mock:  ## contract-mock API + static UI on 127.0.0.1:8794 (invented data, no backend needed)
	cd $(FRONTEND) && node dev/mock-server.mjs

test:  ## every backend test: hub, hubctl and knowledge (offline: fakes, temp dirs, the embedded Lance store)
	cd $(BACKEND) && $(CARGO) test --locked --workspace

knowledge-test:  ## the knowledge service's tests only (the Qdrant half of test_live is skipped without AK_LIVE=1)
	cd $(BACKEND) && $(CARGO) test --locked -p agent-knowledge

knowledge-live:  ## needs AK_LIVE_QDRANT_URL of a THROWAWAY Qdrant; runs the live tests against it
	cd $(BACKEND) && AK_LIVE=1 $(CARGO) test --locked -p agent-knowledge --test test_live

fmt:  ## rustfmt the backend in place
	cd $(BACKEND) && $(CARGO) fmt --all

lint:  ## rustfmt check + clippy with warnings as errors
	cd $(BACKEND) && $(CARGO) fmt --all --check
	$(MAKE) --no-print-directory clippy

clippy:  ## clippy on every crate and target (warnings are errors)
	cd $(BACKEND) && $(CARGO) clippy --locked --workspace --all-targets --all-features -- -D warnings

typecheck: clippy  ## the Rust compiler and clippy are the type check (was mypy --strict)

deny:  ## cargo-deny: advisories, licenses, bans, sources (backend/deny.toml)
	cd $(BACKEND) && $(CARGO) deny --locked check

audit:  ## cargo-audit against the RustSec database (ignores and their reasons: backend/.cargo/audit.toml)
	cd $(BACKEND) && $(CARGO) audit

crate-age:  ## the 14-day dependency cooldown (queries the crates.io API; exceptions: backend/tools/crate-age-exceptions.toml)
	python3 -I $(BACKEND)/tools/crate-age.py

test-map:  ## every pytest id of the Python backend is mapped to a Rust test or N/A (backend/TEST-MAP.md)
	node $(BACKEND)/parity/check-test-map.mjs

parity: build  ## differential run against the last Python backend (needs uv + Python 3.13): hub, then knowledge
	node $(BACKEND)/parity/run.mjs
	node $(BACKEND)/parity/run.mjs --app knowledge

frontend-test:  ## node --test on the pure-logic modules (no npm install needed)
	cd $(FRONTEND) && node --test tests/*.test.mjs

e2e:  ## browser end-to-end: real backend + headless Chrome, every page and tab; records a pass for the pre-push hook
	scripts/e2e-gate.sh run

all: lint test test-map deny audit frontend-test e2e  ## everything the quality bar requires (plus crate-age and parity)

# Desktop launcher (opt-in, Linux). Paths go into a .desktop Exec line and a sed replacement, so the checkout path is
# limited to a safe character set. Fixed ~/.local/share, like the app's own config (XDG vars are unreliable under snaps);
# for the same reason gio runs without GIO_MODULE_DIR, which a snap terminal points at modules that cannot set metadata.
APPS_DIR := $(HOME)/.local/share/applications
LAUNCHER := agent-hub.desktop

install-desktop:  ## add "Agent Hub" to your app menu (starts the hub if needed, then opens the UI); needs `make release`
	test -x $(RELEASE)/agent-hub || { echo "run 'make release' first" >&2; exit 1; }
	dir=$$(pwd -P); \
	case "$$dir" in *[!A-Za-z0-9._/+-]*) echo "install-desktop: move the checkout to a path of [A-Za-z0-9._/+-] only" >&2; exit 1;; esac; \
	mkdir -p "$(APPS_DIR)"; \
	sed -e "s|@BIN@|$$dir/$(RELEASE)/agent-hub|g" -e "s|@ICON@|$$dir/deploy/agent-hub.svg|g" \
	    deploy/$(LAUNCHER) > "$(APPS_DIR)/$(LAUNCHER)"; \
	chmod 644 "$(APPS_DIR)/$(LAUNCHER)"; \
	echo "installed $(APPS_DIR)/$(LAUNCHER)"

install-desktop-icon: install-desktop  ## also put the launcher on your desktop (GNOME: marked trusted so it runs on double-click)
	desk=$$(xdg-user-dir DESKTOP 2>/dev/null || true); [ -n "$$desk" ] && [ "$$desk" != "$(HOME)" ] || desk="$(HOME)/Desktop"; \
	[ -d "$$desk" ] || { echo "install-desktop-icon: no desktop folder at $$desk" >&2; exit 1; }; \
	install -m 755 "$(APPS_DIR)/$(LAUNCHER)" "$$desk/$(LAUNCHER)"; \
	if command -v gio >/dev/null; then env -u GIO_MODULE_DIR gio set "$$desk/$(LAUNCHER)" metadata::trusted true || echo "note: could not mark it trusted; right-click it > Allow Launching" >&2; fi; \
	echo "installed $$desk/$(LAUNCHER)"

uninstall-desktop:  ## remove the menu entry and the desktop icon
	rm -f "$(APPS_DIR)/$(LAUNCHER)"
	desk=$$(xdg-user-dir DESKTOP 2>/dev/null || true); [ -n "$$desk" ] && [ "$$desk" != "$(HOME)" ] || desk="$(HOME)/Desktop"; \
	rm -f "$$desk/$(LAUNCHER)"
