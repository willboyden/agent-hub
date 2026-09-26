#!/usr/bin/env bash
# hermes-deliver.sh — OPT-IN delivery of hub-staged skills into a Hermes Agent container volume.
#
# NEVER run by the hub or by any other script. You run it by hand, after reviewing the stage directory the hub rendered.
# Everything instance-specific is an environment variable; the script refuses to run when a required one is unset.
#
# Required:
#   HERMES_STAGE_DIR    host directory whose children are skill directories (each with a SKILL.md),
#                       e.g. ~/hub-out/hermes/skills
#   HERMES_COMPOSE_FILE host path of the compose file that defines the Hermes service, e.g. ~/hermes/docker-compose.yml
#                       (must be a regular file named docker-compose.y[a]ml or compose.y[a]ml, not a symlink)
#   HERMES_SERVICE      the compose service name, e.g. hermes
#   HERMES_SKILLS_DEST  absolute path INSIDE the container for the hub-owned skills category, e.g. /opt/data/skills/hub
#                       Use a category of its own: only this directory is replaced.
# Optional:
#   HERMES_DOCKER_CONTEXT   passed as `docker --context <value>` (e.g. a rootless context)
#   HERMES_BIN              hermes command inside the container (default: hermes)
#   HERMES_PATH_PREFIX      directory prepended to PATH inside the container (e.g. where hermes lives)
#
# Safety: works on a private copy of the stage; refuses an empty stage, one without SKILL.md, or any symlink; umask 077; prints
# only skill names (never file contents or environment). Extracts to a temp directory beside the destination, swaps with mv,
# then asks hermes to LIST the skills and matches WHOLE names; a failed check rolls back to the previous tree. The destination is
# never removed before a successful extract.
# NOTE for maintainers: the in-container script below is single-quoted for `sh -c`; it must never contain an apostrophe.
set -euo pipefail

need() {
  local v="${!1:-}"
  if [ -z "$v" ]; then
    echo "hermes-deliver: refusing: $1 is not set (see the header of this script)" >&2
    exit 2
  fi
}

need HERMES_STAGE_DIR
need HERMES_COMPOSE_FILE
need HERMES_SERVICE
need HERMES_SKILLS_DEST

STAGE="$HERMES_STAGE_DIR"
COMPOSE="$HERMES_COMPOSE_FILE"
SERVICE="$HERMES_SERVICE"
DEST="$HERMES_SKILLS_DEST"
HBIN="${HERMES_BIN:-hermes}"
PATH_PREFIX="${HERMES_PATH_PREFIX:-}"

# Names that reach a shell or a compose argument must be plain.
[[ "$SERVICE" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || { echo "hermes-deliver: refusing: bad HERMES_SERVICE" >&2; exit 2; }
[[ "$HBIN" =~ ^[A-Za-z0-9._/-]+$ ]] || { echo "hermes-deliver: refusing: bad HERMES_BIN" >&2; exit 2; }
[[ -z "$PATH_PREFIX" || "$PATH_PREFIX" =~ ^/[A-Za-z0-9._/-]*$ ]] || { echo "hermes-deliver: refusing: bad HERMES_PATH_PREFIX" >&2; exit 2; }
# An absolute, plain, at-least-two-level path: never "/" or a top-level directory, never "..".
if ! [[ "$DEST" =~ ^/[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)+$ ]] || [[ "/$DEST/" == *"/../"* ]]; then
  echo "hermes-deliver: refusing: HERMES_SKILLS_DEST must be a plain absolute path with at least two components" >&2
  exit 2
fi

[ -d "$STAGE" ] || { echo "hermes-deliver: stage dir missing: $STAGE (render + apply the hub first)" >&2; exit 1; }
case "$(basename "$COMPOSE")" in
  docker-compose.yml | docker-compose.yaml | compose.yml | compose.yaml) ;;
  *) echo "hermes-deliver: refusing: HERMES_COMPOSE_FILE must be named docker-compose.y[a]ml or compose.y[a]ml" >&2; exit 2 ;;
esac
if [ ! -f "$COMPOSE" ] || [ -L "$COMPOSE" ]; then
  echo "hermes-deliver: compose file missing or a symlink: $COMPOSE" >&2
  exit 1
fi

DOCKER=(docker)
if [ -n "${HERMES_DOCKER_CONTEXT:-}" ]; then
  DOCKER=(docker --context "$HERMES_DOCKER_CONTEXT")
fi

# Work on a private COPY of the stage: every check below runs on bytes nobody else can change, so there is no
# check-then-use window (a symlink swapped into the stage after a check cannot reach the tar).
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
chmod 700 "$WORK"
cp -a --no-dereference "$STAGE/." "$WORK/" || { echo "hermes-deliver: could not copy the stage" >&2; exit 1; }

if [ -n "$(find "$WORK" -type l -print -quit)" ]; then
  echo "hermes-deliver: refusing: symlink inside the stage" >&2
  exit 1
fi
# An empty stage would replace the destination with nothing: refuse.
if [ -z "$(find "$WORK" -name SKILL.md -print -quit)" ]; then
  echo "hermes-deliver: refusing: no SKILL.md under $STAGE" >&2
  exit 1
fi

echo "hermes-deliver: $STAGE -> $SERVICE:$DEST"
find "$WORK" -name SKILL.md -printf '  staged: %h\n' | sed "s#$WORK/##" | sort

# shellcheck disable=SC2016  # the single-quoted body is meant to expand inside the container, not here
# --exclude: caches never belong in the sandbox. The destination reaches the container as a positional argument ($1), not by
# string splicing.
if ! tar -C "$WORK" --exclude=__pycache__ --exclude='*.pyc' -cf - . |
  "${DOCKER[@]}" compose -f "$COMPOSE" run --rm -T --no-deps --entrypoint /bin/sh "$SERVICE" -c '
    set -eu; umask 077
    D=$1; HB=$2; PP=$3
    if [ -n "$PP" ]; then export PATH=$PP:$PATH; fi
    S=$(dirname "$D"); B=$(basename "$D")
    NEW=$S/.$B.new.$$
    OLD=$S/.$B.old.$$
    mkdir -p "$S"
    rm -rf "$NEW"
    mkdir "$NEW"
    if ! tar -C "$NEW" -xf -; then
      rm -rf "$NEW"
      echo "hermes-deliver: extract failed; existing skills left untouched" >&2
      exit 1
    fi
    if ! find "$NEW" -name SKILL.md | grep -q .; then
      rm -rf "$NEW"
      echo "hermes-deliver: no SKILL.md after extract; existing skills left untouched" >&2
      exit 1
    fi
    chown -R hermes:hermes "$NEW" 2>/dev/null || true
    chmod -R u+rwX,go-rwx "$NEW"
    if [ -d "$D" ]; then mv "$D" "$OLD"; fi
    if ! mv "$NEW" "$D"; then
      if [ -d "$OLD" ]; then mv "$OLD" "$D"; fi
      rm -rf "$NEW"
      echo "hermes-deliver: swap failed; previous skills restored" >&2
      exit 1
    fi
    # A copy is not a load. Ask hermes and match WHOLE names (video must not satisfy video-improve-loop).
    listing=$($HB skills list 2>/dev/null || true)
    names=$(printf "%s\n" "$listing" | tr -c "A-Za-z0-9._-" "\n" | sort -u)
    for d in "$D"/*/; do
      n=$(basename "$d")
      if ! printf "%s\n" "$names" | grep -Fxq -- "$n"; then
        rm -rf "$D"
        if [ -d "$OLD" ]; then mv "$OLD" "$D"; fi
        echo "hermes-deliver: hermes does not list skill $n; rolled back" >&2
        exit 1
      fi
    done
    rm -rf "$OLD"
    find "$D" -name SKILL.md -printf "  delivered: %P\n"
  ' sh "$DEST" "$HBIN" "$PATH_PREFIX"; then
  echo "hermes-deliver: FAILED" >&2
  exit 1
fi
echo "hermes-deliver: done"
