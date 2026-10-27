#!/usr/bin/env bash
# Ship gate for the browser end-to-end suite (frontend/e2e/run.mjs).
#
#   scripts/e2e-gate.sh run           run the suite; on a pass with a clean backend/, frontend/ and policy/, record it
#   scripts/e2e-gate.sh check <rev>   exit 0 only if the suite passed on exactly <rev>'s backend/, frontend/ and policy/
#
# The record is the git tree ids of backend/, frontend/ and policy/ (the floor the server loads), kept inside .git
# (never committed). A docs-only commit after a pass needs no rerun; any change to the app does. The pre-push hook
# (.githooks/pre-push, enabled by `make hooks`) calls `check` for every commit or tag being pushed. CI runs the suite
# again on every push. No `set -e`: every failure below is handled and reported on purpose.
set -uo pipefail

cd "$(dirname "$0")/.." || exit 2
STAMP="$(git rev-parse --git-path agent-hub-e2e.pass)" || { echo "e2e-gate: not a git checkout" >&2; exit 2; }
DIRS=(backend frontend policy)

# "<backend tree> <frontend tree> <policy tree>" of a commit (an annotated tag is peeled to its commit).
trees() {
  local d t out=()
  for d in "${DIRS[@]}"; do
    t=$(git rev-parse --verify --quiet "$1^{commit}:$d") || return 1
    out+=("$t")
  done
  echo "${out[*]}"
}

case "${1:-}" in
  run)
    # A Chrome that is not on PATH can be named once per checkout: git config agent-hub.chrome /path/to/chrome
    if [ -z "${CHROME_BIN:-}" ] && chrome=$(git config --get agent-hub.chrome); then export CHROME_BIN="$chrome"; fi
    node frontend/e2e/run.mjs || exit 1
    # The suite tested the working tree, so a pass only counts for a commit when the app directories match it.
    if [ -n "$(git status --porcelain -- "${DIRS[@]}")" ]; then
      echo "e2e-gate: passed, but backend/, frontend/ or policy/ has uncommitted changes, so no pass was recorded." >&2
      echo "e2e-gate: commit, then run 'make e2e' again before pushing." >&2
      exit 0
    fi
    want=$(trees HEAD) || { echo "e2e-gate: cannot read HEAD's trees" >&2; exit 1; }
    echo "$want" > "$STAMP" || exit 1
    echo "e2e-gate: pass recorded for $(git rev-parse --short HEAD)"
    ;;
  check)
    rev="${2:-}"
    [ -n "$rev" ] || { echo "usage: scripts/e2e-gate.sh check <rev>" >&2; exit 2; }
    want=$(trees "$rev") || { echo "e2e-gate: $rev has no backend/, frontend/ and policy/ trees" >&2; exit 1; }
    [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$want" ]
    ;;
  *)
    echo "usage: scripts/e2e-gate.sh run | check <rev>" >&2
    exit 2
    ;;
esac
