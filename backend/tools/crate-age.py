#!/usr/bin/env python3
"""crate-age: the project's dependency cooldown as a gate.

Every crates.io package in backend/Cargo.lock must have been public for at least --days days (default 14) before now.
A younger version passes only when backend/tools/crate-age-exceptions.toml lists it with the RustSec advisories it
fixes: security fixes bypass the cooldown (as Dependabot's security updates do), version updates never do.

    python3 -I backend/tools/crate-age.py              check (exit 0 = all old enough, 1 = too new, 2 = could not check)
    python3 -I backend/tools/crate-age.py --suggest    also print, per too-new package, the newest release of the same
                                                    semver class that is old enough (for `cargo update --precise`)

Network: only https://crates.io/api/v1 (one request per second, the crates.io crawler policy). A version's
publication time never changes, so answers are cached in backend/target/crate-age-cache.json (gitignored) and a second
run needs no network. Stdlib only; run it with `python3 -I`.
"""
from __future__ import annotations

import argparse
import datetime as dt
import json
import sys
import time
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

RUST = Path(__file__).resolve().parent.parent
API = "https://crates.io/api/v1/crates"
UA = "agent-hub-crate-age-check"     # crates.io asks API clients to identify themselves; nothing personal here


def fetch(url: str) -> dict:
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    last: Exception | None = None
    for attempt in range(3):
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                return json.load(r)
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as e:
            last = e
            time.sleep(2 + attempt * 3)
    raise RuntimeError(f"{url}: {last}")


def parse_time(s: str) -> dt.datetime:
    return dt.datetime.fromisoformat(s.replace("Z", "+00:00"))


def semver_class(v: str) -> tuple[int, ...]:
    """The versions cargo treats as compatible with `v` (^v): same major, or same 0.minor, or same 0.0.patch."""
    nums = tuple(int(x) for x in v.split("+")[0].split("-")[0].split("."))
    major, minor, patch = (nums + (0, 0, 0))[:3]
    return (major,) if major else (0, minor) if minor else (0, 0, patch)


def version_key(v: str) -> tuple[tuple[int, ...], bool]:
    core = v.split("+")[0]
    main, _, pre = core.partition("-")
    return tuple(int(x) for x in main.split(".")), pre == ""


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--lock", type=Path, default=RUST / "Cargo.lock")
    ap.add_argument("--exceptions", type=Path, default=RUST / "tools" / "crate-age-exceptions.toml")
    ap.add_argument("--cache", type=Path, default=RUST / "target" / "crate-age-cache.json")
    ap.add_argument("--days", type=int, default=14)
    ap.add_argument("--suggest", action="store_true")
    args = ap.parse_args()

    now = dt.datetime.now(dt.UTC)
    cutoff = now - dt.timedelta(days=args.days)
    lock = tomllib.loads(args.lock.read_text())
    pkgs = sorted({(p["name"], p["version"]) for p in lock.get("package", [])
                   if str(p.get("source", "")).startswith("registry+https://github.com/rust-lang/crates.io-index")})
    exc_doc = tomllib.loads(args.exceptions.read_text()) if args.exceptions.exists() else {}
    exceptions = {(e["name"], e["version"]): e for e in exc_doc.get("exception", [])}
    for key, e in exceptions.items():
        if not e.get("advisories") or not e.get("reason"):
            print(f"crate-age: exception {key[0]} {key[1]} needs `advisories` and `reason`", file=sys.stderr)
            return 2

    cache: dict[str, str] = {}
    if args.cache.exists():
        try:
            cache = json.loads(args.cache.read_text())
        except json.JSONDecodeError:
            cache = {}
    fetched = 0
    try:
        for name, ver in pkgs:
            k = f"{name} {ver}"
            if k in cache:
                continue
            if fetched:
                time.sleep(1.0)
            cache[k] = fetch(f"{API}/{name}/{ver}")["version"]["created_at"]
            fetched += 1
    except RuntimeError as e:
        print(f"crate-age: cannot reach crates.io ({e}); nothing was judged", file=sys.stderr)
        return 2
    finally:
        if fetched:
            args.cache.parent.mkdir(parents=True, exist_ok=True)
            args.cache.write_text(json.dumps(cache, indent=0, sort_keys=True))

    too_new, excused = [], []
    for name, ver in pkgs:
        created = parse_time(cache[f"{name} {ver}"])
        if created <= cutoff:
            continue
        (excused if (name, ver) in exceptions else too_new).append((name, ver, created))
    stale = [k for k in exceptions
             if k not in set(pkgs) or parse_time(cache.get(f"{k[0]} {k[1]}", now.isoformat())) <= cutoff]

    print(f"crate-age: {len(pkgs)} crates.io packages, cutoff {cutoff:%Y-%m-%dT%H:%MZ} ({args.days} days); "
          f"{len(too_new)} too new, {len(excused)} security exceptions, {len(stale)} stale exceptions "
          f"({fetched} looked up)")
    for name, ver, created in excused:
        e = exceptions[(name, ver)]
        print(f"  exception {name} {ver} (published {created:%Y-%m-%d}): fixes {', '.join(e['advisories'])}")
    for name, ver in stale:
        print(f"  stale exception {name} {ver}: no longer locked or now old enough; remove it from {args.exceptions.name}")
    for name, ver, created in too_new:
        line = f"  TOO NEW {name} {ver} (published {created:%Y-%m-%dT%H:%MZ})"
        if args.suggest:
            time.sleep(1.0)
            vs = fetch(f"{API}/{name}/versions?per_page=100")["versions"]
            ok = sorted((version_key(v["num"]), v["num"]) for v in vs
                        if not v["yanked"] and "-" not in v["num"] and parse_time(v["created_at"]) <= cutoff
                        and semver_class(v["num"]) == semver_class(ver))
            line += f" -> cargo update -p {name}@{ver} --precise {ok[-1][1]}" if ok else " -> no old-enough release in its semver class"
        print(line)
    return 1 if too_new else 0


if __name__ == "__main__":
    sys.exit(main())
