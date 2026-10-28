#!/usr/bin/env bash
# dsh-deliver.sh — OPT-IN delivery of a hub-staged tree into the docker volume that DeepSeek Harness (dsh) reads as its
# agents home (DSH_AGENTS_HOME).
#
# NEVER run by the hub or by any other script. You run it by hand, after reviewing the stage directory the hub rendered.
# Everything instance-specific is an environment variable; the script refuses to run when a required one is unset.
#
# Usage:
#   deploy/dsh-deliver.sh             deliver the stage
#   deploy/dsh-deliver.sh rollback    put back the tree the last delivery replaced (one generation is kept)
#
# Required:
#   DSH_STAGE_DIR       the client's stage root as the hub rendered it (holds skills/, AGENTS.md, hub.cordis.patch.yml),
#                       e.g. ~/hub-out/deepseek-harness. Not needed for rollback.
#   DSH_VOLUME          the docker VOLUME the dsh container mounts read-only as DSH_AGENTS_HOME, e.g. my-dsh_hub-agents.
#                       A volume name, never a host path (a path would turn `docker run -v` into a bind mount); it must
#                       already exist.
# Optional:
#   DSH_DOCKER_CONTEXT  passed as `docker --context <value>` (e.g. a rootless context)
#   DSH_CONTAINER       the running dsh container, e.g. my-dsh-1. When set, the script then asks it with
#                       `node /opt/lab/list-skills.mjs` (a helper in the image that runs dsh's own skill discovery and
#                       prints JSON) and ROLLS BACK unless every staged skill is listed with source "user-agents".
#                       Needs python3 on the host to read that JSON.
#
# What is delivered: only skills/, AGENTS.md and hub.cordis.patch.yml from the stage (anything else there is ignored and
# named). The three are one set: an entry that is no longer staged is removed from the volume. Whether dsh loads the patch
# (--patch) is up to the image's entrypoint.
#
# Safety: works on a private copy of the stage (umask 077) and checks the stage before and after copying. Refuses symlinks,
# hard links (link count > 1) and anything that is not a regular file or a directory; a skill folder that is not a dsh
# skill name, has no SKILL.md, has a SKILL.md over 64 KiB or holding a <skill_...> tag (dsh inserts SKILL.md verbatim
# between its own <skill_...> tags), or weighs over 4 MiB; and a hub.cordis.patch.yml containing "!!", "!<" or a "%"
# directive line (a YAML tag in a dsh patch can execute JavaScript). Writes into the volume only through a throwaway
# container (pinned alpine image, no network, read-only root, only the capabilities the swap needs): it extracts beside the
# live tree, makes it root-owned and world-readable (dsh runs as another uid and must read, never write), swaps each entry
# with mv and keeps the replaced tree in .hub-prev for rollback. Prints only names and counts, never file contents or the
# environment. No `set -e`: every failure is handled and reported where it happens.
# NOTE for maintainers: the in-volume script (BODY) is single-quoted for `sh -c`; it must never contain an apostrophe.
set -uo pipefail
umask 077

IMAGE="alpine:3.22@sha256:2e0b81d60597f449baa56dc25d355367869d2dca4cd57e866f1e2b52af3eb55f"
# Keep these equal to MAX_SKILL_MD / MAX_SKILL_DIR in backend/src/agent_hub/adapters/deepseek_harness.py (a test compares).
MAX_SKILL_MD=65536
MAX_SKILL_DIR=4194304
MAX_PATCH=262144
MAX_LIST_OUT=4194304
NAME_RE='^[a-z0-9]+(-[a-z0-9]+)*$'
PLAIN_RE='^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$'
VOLUME_RE='^[A-Za-z0-9][A-Za-z0-9_.-]{0,254}$'
FRAMING_RE='<[[:space:]]*/?[[:space:]]*skill_'
TAG_RISK_RE='!!|!<|^[[:space:]]*%'
ENTRIES=(skills AGENTS.md hub.cordis.patch.yml)

say() { printf 'dsh-deliver: %s\n' "$*"; }
die() {
  local code=$1
  shift
  printf 'dsh-deliver: %s\n' "$*" >&2
  exit "$code"
}
need() {
  if [ -z "${!1:-}" ]; then
    die 2 "refusing: $1 is not set (see the header of this script)"
  fi
}

MODE="${1:-deliver}"
case "$MODE" in
  deliver | rollback) ;;
  *) die 2 "usage: dsh-deliver.sh [rollback]" ;;
esac
[ "$#" -le 1 ] || die 2 "usage: dsh-deliver.sh [rollback]"

need DSH_VOLUME
if [ "$MODE" = deliver ]; then
  need DSH_STAGE_DIR
fi
VOLUME="$DSH_VOLUME"
CONTAINER="${DSH_CONTAINER:-}"
CTX="${DSH_DOCKER_CONTEXT:-}"
[[ "$VOLUME" =~ $VOLUME_RE ]] || die 2 "refusing: DSH_VOLUME must be a docker volume name ([A-Za-z0-9][A-Za-z0-9_.-]*), not a path"
[[ -z "$CONTAINER" || "$CONTAINER" =~ $PLAIN_RE ]] || die 2 "refusing: bad DSH_CONTAINER"
[[ -z "$CTX" || "$CTX" =~ $PLAIN_RE ]] || die 2 "refusing: bad DSH_DOCKER_CONTEXT"
if [ -n "$CONTAINER" ] && [ "$MODE" = deliver ] && ! command -v python3 >/dev/null 2>&1; then
  die 2 "refusing: DSH_CONTAINER is set but python3 (needed to read dsh's answer) is not on PATH"
fi

DOCKER=(docker)
if [ -n "$CTX" ]; then
  DOCKER=(docker --context "$CTX")
fi

# The in-volume script. $1 = deliver|rollback, $2 = the volume root. A CONSTANT: nothing is ever spliced into it.
# swap SRC: move the live entries into .hub-stale, then SRC's entries live; on a failure put everything back.
# Returns 0 done, 1 failed with nothing changed, 2 failed and the undo failed too (the volume needs a look).
# shellcheck disable=SC2016  # expanded by the sh inside the container, not here
BODY='
set -u
umask 022
MODE=$1
R=$2
NEW=$R/.hub-new
PREV=$R/.hub-prev
STALE=$R/.hub-stale
E="skills AGENTS.md hub.cordis.patch.yml"
fail() { echo "dsh-deliver (volume): $1" >&2; exit 1; }
present() { [ -e "$1" ] || [ -L "$1" ]; }
undo() {
  u=0
  if [ "$2" -eq 2 ]; then
    for e in $E; do if present "$R/$e"; then mv "$R/$e" "$1/$e" || u=1; fi; done
  fi
  for e in $E; do if present "$STALE/$e"; then mv "$STALE/$e" "$R/$e" || u=1; fi; done
  return $u
}
swap() {
  rm -rf "$STALE" || return 1
  mkdir "$STALE" || return 1
  for e in $E; do
    if present "$R/$e"; then
      if ! mv "$R/$e" "$STALE/$e"; then undo "$1" 1 && return 1; return 2; fi
    fi
  done
  for e in $E; do
    if present "$1/$e"; then
      if ! mv "$1/$e" "$R/$e"; then undo "$1" 2 && return 1; return 2; fi
    fi
  done
  return 0
}
[ -d "$R" ] || fail "the volume is not mounted at $R"
if [ "$MODE" = deliver ]; then
  rm -rf "$NEW" "$STALE" || fail "could not clear what an earlier run left; nothing changed"
  mkdir "$NEW" || fail "could not create $NEW; nothing changed"
  if ! tar -C "$NEW" -xf -; then rm -rf "$NEW"; fail "extract failed; the live tree is untouched"; fi
  odd=$(find "$NEW" ! -type f ! -type d) || { rm -rf "$NEW"; fail "could not check the extracted tree; the live tree is untouched"; }
  if [ -n "$odd" ]; then rm -rf "$NEW"; fail "the archive held something other than files and directories; the live tree is untouched"; fi
  have=0
  for e in $E; do if present "$NEW/$e"; then have=1; fi; done
  if [ "$have" -eq 0 ]; then rm -rf "$NEW"; fail "nothing to deliver after extract; the live tree is untouched"; fi
  if [ "$(id -u)" -eq 0 ]; then
    chown -R 0:0 "$NEW" || { rm -rf "$NEW"; fail "chown failed; the live tree is untouched"; }
  fi
  chmod -R a+rX,go-w "$NEW" || { rm -rf "$NEW"; fail "chmod failed; the live tree is untouched"; }
  swap "$NEW"
  rc=$?
  if [ "$rc" -eq 1 ]; then rm -rf "$NEW" "$STALE"; fail "swap failed; the live tree is unchanged"; fi
  if [ "$rc" -ne 0 ]; then fail "swap failed and could not be undone: inspect .hub-stale and .hub-new in the volume"; fi
  rm -rf "$PREV" || fail "delivered, but the older .hub-prev could not be removed: the replaced tree is in .hub-stale"
  mv "$STALE" "$PREV" || fail "delivered, but the replaced tree could not be kept as .hub-prev (it is in .hub-stale)"
  rm -rf "$NEW" || echo "dsh-deliver (volume): delivered; could not remove the empty .hub-new" >&2
  echo "dsh-deliver (volume): swapped in; the replaced tree is kept in .hub-prev"
else
  [ -d "$PREV" ] || fail "no previous tree to restore (nothing delivered yet, or already rolled back)"
  swap "$PREV"
  rc=$?
  if [ "$rc" -eq 1 ]; then rm -rf "$STALE"; fail "rollback failed; the live tree is unchanged"; fi
  if [ "$rc" -ne 0 ]; then fail "rollback failed and could not be undone: inspect .hub-stale and .hub-prev in the volume"; fi
  rm -rf "$STALE" "$PREV" || echo "dsh-deliver (volume): restored; could not remove the replaced tree in .hub-stale" >&2
  echo "dsh-deliver (volume): restored the previous tree"
fi
'

# Run BODY in a throwaway container that sees only the volume. Its stdin is this function's stdin.
in_volume() {
  "${DOCKER[@]}" run --rm -i --network none --read-only --user 0:0 --cap-drop ALL \
    --cap-add CHOWN --cap-add DAC_OVERRIDE --cap-add FOWNER --security-opt no-new-privileges \
    -v "$VOLUME:/s" --entrypoint /bin/sh "$IMAGE" -c "$BODY" sh "$1" /s
}

volume_exists() {
  command -v docker >/dev/null 2>&1 || die 1 "docker is not on PATH"
  if ! "${DOCKER[@]}" volume inspect --format '{{.Name}}' "$VOLUME" >/dev/null 2>&1; then
    die 1 "volume $VOLUME not found (check DSH_VOLUME and DSH_DOCKER_CONTEXT); nothing changed"
  fi
}

if [ "$MODE" = rollback ]; then
  volume_exists
  say "rollback in volume $VOLUME"
  if ! in_volume rollback </dev/null; then
    die 1 "rollback FAILED (see the message above)"
  fi
  say "done"
  exit 0
fi

# ---- deliver: build and check a private copy of the stage ----------------------------------------------------------------
STAGE="$DSH_STAGE_DIR"
[ -d "$STAGE" ] || die 1 "stage dir missing: $STAGE (apply the hub client first)"

WORK="$(mktemp -d)" || die 1 "could not create a temp dir"
trap 'rm -rf "$WORK"' EXIT
OUT="$WORK/out"
mkdir "$OUT" || die 1 "could not create a temp dir"

# Why $1 cannot be delivered (a symlink, a special file or a hard link anywhere in it); nothing when it is clean.
problem_in() {
  local p=$1 hit
  if [ -L "$p" ]; then
    printf 'is a symlink'
    return 0
  fi
  if ! hit=$(find "$p" ! -type f ! -type d -print -quit); then
    printf 'could not be read completely'
    return 0
  fi
  if [ -n "$hit" ]; then
    printf 'holds a symlink or special file (%q)' "${hit#"$p"}"
    return 0
  fi
  if ! hit=$(find "$p" -type f -links +1 -print -quit); then
    printf 'could not be read completely'
    return 0
  fi
  if [ -n "$hit" ]; then
    printf 'holds a hard link (%q)' "${hit#"$p"}"
  fi
  return 0
}

if ! IGNORED=$(find "$STAGE" -mindepth 1 -maxdepth 1 ! -name skills ! -name AGENTS.md ! -name hub.cordis.patch.yml -printf '%P\n'); then
  die 1 "could not list the stage"
fi
for e in "${ENTRIES[@]}"; do
  src="$STAGE/$e"
  if [ ! -e "$src" ] && [ ! -L "$src" ]; then
    continue
  fi
  why=$(problem_in "$src")
  [ -z "$why" ] || die 1 "refusing: $e $why"
  cp -a --no-dereference -- "$src" "$OUT/$e" || die 1 "could not copy $e from the stage"
done
# The same checks on what will be sent, and on the stage again (a link swapped in while copying).
for e in "${ENTRIES[@]}"; do
  for base in "$OUT" "$STAGE"; do
    if [ -e "$base/$e" ] || [ -L "$base/$e" ]; then
      why=$(problem_in "$base/$e")
      [ -z "$why" ] || die 1 "refusing: $e $why (changed while copying)"
    fi
  done
done

if [ -e "$OUT/skills" ] && [ ! -d "$OUT/skills" ]; then
  die 1 "refusing: skills in the stage is not a directory"
fi
for f in AGENTS.md hub.cordis.patch.yml; do
  if [ -e "$OUT/$f" ] && [ ! -f "$OUT/$f" ]; then
    die 1 "refusing: $f in the stage is not a regular file"
  fi
done

# Compiled caches never belong in the agents home (the copy is ours to prune).
if ! find "$OUT" \( -type d -name __pycache__ -prune -exec rm -rf -- {} + \) -o \( -type f \( -name '*.pyc' -o -name '*.pyo' \) -exec rm -f -- {} + \); then
  die 1 "could not leave out __pycache__ / *.pyc"
fi

NAMES=()
if [ -d "$OUT/skills" ]; then
  find "$OUT/skills" -mindepth 1 -maxdepth 1 -printf '%P\0' >"$WORK/names" || die 1 "could not list the staged skills"
  while IFS= read -r -d '' n; do
    d="$OUT/skills/$n"
    if ! [[ "$n" =~ $NAME_RE ]] || [ "${#n}" -gt 64 ]; then
      die 1 "refusing: skills/$(printf '%q' "$n") is not a dsh skill name (lowercase letters and digits in hyphen-separated words)"
    fi
    [ -d "$d" ] || die 1 "refusing: skills/$n is not a directory"
    [ -f "$d/SKILL.md" ] || die 1 "refusing: skills/$n has no SKILL.md"
    size=$(stat -c %s -- "$d/SKILL.md") || die 1 "could not read the size of skills/$n/SKILL.md"
    [ "$size" -le "$MAX_SKILL_MD" ] || die 1 "refusing: skills/$n/SKILL.md is $size bytes (dsh limit here: $MAX_SKILL_MD)"
    total=$(find "$d" -type f -printf '%s\n' | awk '{ t += $1 } END { print t + 0 }') || die 1 "could not size skills/$n"
    [ "$total" -le "$MAX_SKILL_DIR" ] || die 1 "refusing: skills/$n is $total bytes (limit $MAX_SKILL_DIR)"
    grep -qiE -- "$FRAMING_RE" "$d/SKILL.md"
    case $? in
      0) die 1 "refusing: skills/$n/SKILL.md contains a <skill_...> tag that could fake dsh's prompt framing" ;;
      1) ;;
      *) die 1 "could not read skills/$n/SKILL.md" ;;
    esac
    NAMES+=("$n")
  done < <(sort -z "$WORK/names")
fi

PATCH="$OUT/hub.cordis.patch.yml"
if [ -f "$PATCH" ]; then
  size=$(stat -c %s -- "$PATCH") || die 1 "could not read the size of hub.cordis.patch.yml"
  [ "$size" -le "$MAX_PATCH" ] || die 1 "refusing: hub.cordis.patch.yml is $size bytes (limit $MAX_PATCH)"
  grep -qE -- "$TAG_RISK_RE" "$PATCH"
  case $? in
    0) die 1 "refusing: hub.cordis.patch.yml contains a YAML tag or directive (!!, !< or a % line); a tag in a dsh patch can execute JavaScript" ;;
    1) ;;
    *) die 1 "could not read hub.cordis.patch.yml" ;;
  esac
fi

if [ "${#NAMES[@]}" -eq 0 ] && [ ! -f "$OUT/AGENTS.md" ] && [ ! -f "$PATCH" ]; then
  die 1 "refusing: nothing to deliver (no skill, AGENTS.md or hub.cordis.patch.yml in $STAGE); an empty delivery would clear the volume"
fi

say "$STAGE -> volume $VOLUME"
for n in "${NAMES[@]}"; do
  printf '  skill: %s\n' "$n"
done
if [ -f "$OUT/AGENTS.md" ]; then
  printf '  AGENTS.md\n'
fi
if [ -f "$PATCH" ]; then
  printf '  hub.cordis.patch.yml\n'
fi
while IFS= read -r n; do
  if [ -n "$n" ]; then
    printf '  ignored (not delivered): %q\n' "$n"
  fi
done <<<"$IGNORED"

# ---- deliver: write the volume -----------------------------------------------------------------------------------------
volume_exists
if ! tar -C "$OUT" --sort=name --owner=0 --group=0 --numeric-owner -cf - . | in_volume deliver; then
  die 1 "FAILED: nothing was delivered or the previous tree is back (see the message above)"
fi

if [ -z "$CONTAINER" ]; then
  say "delivered ${#NAMES[@]} skill(s); not checked (set DSH_CONTAINER to ask dsh which skills it sees)"
  exit 0
fi
if [ "${#NAMES[@]}" -eq 0 ]; then
  say "delivered; no skills staged, so there is nothing to ask dsh"
  exit 0
fi

# ---- deliver: ask dsh, roll back unless every staged skill is listed from its agents home --------------------------------
# A CONSTANT program; the staged names arrive as arguments. Exit 0 all listed, 1 some missing (names printed), 2 bad JSON.
CHECK_PY='
import json, sys
cap = int(sys.argv[1])
raw = sys.stdin.buffer.read(cap + 1)
if len(raw) > cap:
    sys.exit(2)
try:
    text = raw.decode("utf-8")
except UnicodeDecodeError:
    sys.exit(2)
doc = None
lines = [ln for ln in text.splitlines() if ln.strip()]
for candidate in (text, lines[-1] if lines else ""):
    try:
        doc = json.loads(candidate)
        break
    except (ValueError, RecursionError):
        doc = None
if not isinstance(doc, dict) or not isinstance(doc.get("skills"), list):
    sys.exit(2)
seen = {s.get("name") for s in doc["skills"] if isinstance(s, dict) and s.get("source") == "user-agents"}
missing = [n for n in sys.argv[2:] if n not in seen]
print(" ".join(missing[:10]))
sys.exit(1 if missing else 0)
'
LIST="$WORK/list.json"
timeout 120 "${DOCKER[@]}" exec "$CONTAINER" node /opt/lab/list-skills.mjs </dev/null 2>/dev/null | head -c "$((MAX_LIST_OUT + 1))" >"$LIST"
st=("${PIPESTATUS[@]}")
why=""
if [ "${st[0]}" -ne 0 ]; then
  why="list-skills in $CONTAINER exited ${st[0]}"
else
  missing=$(python3 -I -c "$CHECK_PY" "$MAX_LIST_OUT" "${NAMES[@]}" <"$LIST")
  case $? in
    0) ;;
    1) why="dsh does not list these staged skills from its agents home (source user-agents): $missing" ;;
    *) why="list-skills printed no usable JSON" ;;
  esac
fi
if [ -z "$why" ]; then
  say "dsh lists all ${#NAMES[@]} staged skill(s) from its agents home; done"
  exit 0
fi
say "check failed: $why; rolling back"
if in_volume rollback </dev/null; then
  die 1 "rolled back: the volume holds the previous tree again"
fi
die 1 "ROLLBACK FAILED: the volume may hold the new tree; run '$0 rollback' or inspect it"
