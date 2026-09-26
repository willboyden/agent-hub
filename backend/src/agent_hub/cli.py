"""`hubctl`: the operator CLI. Same services as the API, no HTTP, no auth (it is a local process run by the operator).
Exit codes: 0 ok, 1 error, 2 the plan has errors or floor violations."""
from __future__ import annotations

import argparse
import contextlib
import getpass
import json
import os
import sys
from typing import Any

from dotenv import dotenv_values

from agent_hub import config as config_mod
from agent_hub.config import check_bind, load_settings
from agent_hub.domain.errors import ProblemError, Unauthorized, Unavailable
from agent_hub.services.auth import BOOTSTRAP_KEY_FILE
from agent_hub.services.hub import Hub

ACTOR = "hubctl"


def _emit(args: argparse.Namespace, payload: Any, text: str) -> None:
    if args.json:
        print(json.dumps(payload, indent=2, default=str))
    else:
        print(text)


def _plan_text(plan: dict[str, Any]) -> str:
    lines = [f"plan {plan['id']}  content {plan['content_hash'][:12]}"]
    if plan.get("pending_changes"):
        lines.append(f"  note: {plan['pending_changes']} uncommitted content change(s) are NOT in this plan")
    for cp in plan["clients"]:
        summ = ", ".join(f"{v} {k}" for k, v in sorted(cp["summary"].items())) or "nothing to do"
        lines.append(f"- {cp['client']}: {summ}" + ("  [BLOCKED]" if cp["blocked"] else ""))
        for f in cp["files"]:
            if f["action"] != "unchanged":
                lines.append(f"    {f['action']:9} {f['root']}:{f['path']}" + (f"  ({f['reason']})" if f["reason"] else ""))
        for r in cp["blocked_reasons"]:
            lines.append(f"    ! {r}")
    return "\n".join(lines)


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="hubctl", description="Agent Hub operator CLI")
    p.add_argument("--json", action="store_true", help="machine-readable output")
    p.add_argument("--content-dir", help="override HUB_CONTENT_DIR")
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("init", help="create the content repo (git init) and seed it")
    sub.add_parser("validate", help="validate every content file (working tree)")
    for name in ("plan", "apply", "verify", "drift", "audit"):
        sp = sub.add_parser(name)
        sp.add_argument("--client", action="append", default=None, help="limit to a client (repeatable)")
        if name == "apply":
            sp.add_argument("--adopt", action="append", default=[], help="adopt a conflicting path: root:path")
            sp.add_argument("--yes", action="store_true", help="confirm (apply never runs without it)")
    imp = sub.add_parser("import", help="import what a client already has into the content working tree")
    imp.add_argument("--client", required=True)
    imp.add_argument("--all", action="store_true", help="import everything discovered")
    imp.add_argument("--item", action="append", default=[], help="kind:name (repeatable)")
    imp.add_argument("--on-conflict", default=None, choices=["skip", "rename", "replace", "link"],
                     help="default: link identical items, skip differing ones")
    sub.add_parser("status", help="show pending (uncommitted) content changes")
    cm = sub.add_parser("commit", help="approve pending content changes (git commit with the hub's fixed identity)")
    cm.add_argument("-m", "--message", required=True)
    rb = sub.add_parser("rollback", help="undo an apply (default: the latest) using its recorded backups")
    rb.add_argument("--client", required=True)
    rb.add_argument("--apply-id")
    rb.add_argument("--dry-run", action="store_true", help="show the diff, change nothing")
    rb.add_argument("--yes", action="store_true")
    rb.add_argument("--force-paths", action="append", default=[], metavar="ROOT:PATH", help="restore even if edited since")
    ap_ = sub.add_parser("applies", help="list recorded applies for a client")
    ap_.add_argument("--client", required=True)
    mg = sub.add_parser("migrate-content", help="move the content repo to a private location outside the source tree")
    mg.add_argument("--to", required=True, metavar="PATH")
    mg.add_argument("--yes", action="store_true", help="do it (without it: show what would move)")
    at = sub.add_parser("attest-mcp", help="record that you HAVE scanned an MCP server (binds `clean` to its current fields)")
    at.add_argument("name")
    at.add_argument("--yes", action="store_true", help="confirm that your MCP scanner was run and came back clean")
    sub.add_parser("retire-bootstrap-key", help="verify the admin key, then wipe and remove the bootstrap key file (terminal only)")
    dr = sub.add_parser("doctor", help="read-only report of how exposed the hub's secrets and policy are on this host")
    dr.add_argument("--json", action="store_true", dest="doctor_json")
    sub.add_parser("adapters", help="list installed adapters")
    srv = sub.add_parser("serve", help="run the API + UI")
    srv.add_argument("--port", type=int)
    return p


def _clients(hub: Hub, wanted: list[str] | None) -> list[str]:
    _, store = hub.approved()
    return wanted or sorted(store.catalog().clients)


def run(argv: list[str] | None = None, hub: Hub | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        if hub is None:
            overrides: dict[str, Any] = {}
            if args.content_dir:
                from pathlib import Path
                overrides["content_dir"] = Path(args.content_dir)
            hub = Hub(load_settings(**overrides))
        if args.cmd not in ("init", "serve", "retire-bootstrap-key"):
            _authenticate(hub)
        return _dispatch(args, hub)
    except ProblemError as exc:
        print(f"error: {exc.code}: {exc.detail}", file=sys.stderr)
        return 1


def _authenticate(hub: Hub) -> None:
    """hubctl needs an admin key too (HUB_API_KEY, else the 0600 bootstrap file): a shell-capable agent must not be able
    to drive the hub just because it can run a local command."""
    key = os.environ.get("HUB_API_KEY", "").strip()
    if not key and config_mod.ENV_FILE.is_file():                # the same env file as the server settings; never printed
        key = str(dotenv_values(config_mod.ENV_FILE).get("HUB_API_KEY") or "").strip()
    if not key:
        with contextlib.suppress(OSError):
            key = (hub.cfg.data_dir / BOOTSTRAP_KEY_FILE).read_text().strip()
    if not key and sys.stdin.isatty():                          # no key at rest: ask, hidden; never echoed, logged or stored
        key = getpass.getpass("Hub admin key (input hidden): ").strip()
    p = hub.auth.authenticate(key) if key else None
    if p is None or p.role != "admin":
        raise Unauthorized("an admin key is required: set HUB_API_KEY (environment or the env file), or run from a terminal to be "
                           "prompted for it")


def _retire_bootstrap_key(hub: Hub) -> int:
    """Remove the admin key from disk once the operator has it elsewhere. TTY only; the key is verified BEFORE anything is
    touched; the file is overwritten with zeros, then unlinked (best effort on journaling/CoW filesystems)."""
    f = hub.cfg.data_dir / BOOTSTRAP_KEY_FILE
    if not f.exists() and not f.is_symlink():
        print("the bootstrap key file is already gone; nothing to retire")
        return 0
    if not sys.stdin.isatty():
        print("error: retire-bootstrap-key needs an interactive terminal (it asks for the key)", file=sys.stderr)
        return 1
    key = getpass.getpass("Hub admin key (input hidden): ").strip()
    p = hub.auth.authenticate(key) if key else None          # compares stored hashes in constant time
    if p is None or p.role != "admin":
        print("error: that key did not verify as an admin key; the file was NOT touched", file=sys.stderr)
        return 1
    if f.is_symlink() or not f.is_file():
        print("error: the bootstrap key path is not a regular file; refusing to touch it", file=sys.stderr)
        return 1
    with contextlib.suppress(OSError):
        size = f.stat().st_size
        with open(f, "r+b") as fh:
            fh.write(b"\x00" * size)
            fh.flush()
            os.fsync(fh.fileno())
    f.unlink()
    print("bootstrap key file removed. Keep the admin key in a password manager: hubctl and the UI will now prompt for it.")
    return 0


def _dispatch(args: argparse.Namespace, hub: Hub) -> int:
    cmd = args.cmd
    if cmd == "init":
        r = hub.init_content()
        hub.auth.ensure_bootstrap_key(hub.cfg.data_dir)
        _emit(args, r, f"content repo ready at {r['content_dir']} ({len(r['seeded'])} file(s) seeded)")
        return 0
    if cmd == "status":
        hub.require_repo()
        ch = hub.changes()
        _emit(args, ch, "\n".join(f"{c['status']:9} {c['path']}" for c in ch["items"]) or "no pending changes")
        return 0
    if cmd == "commit":
        r = hub.commit(args.message, ACTOR)
        _emit(args, r, f"committed {r['commit'][:12]}")
        return 0
    if cmd == "applies":
        r = hub.list_applies(args.client)
        _emit(args, r, "\n".join(f"{a['apply_id']}  {a['counts']}" + ("  [rolled back]" if a["rolled_back_at"] else "")
                                for a in r["items"]) or "no applies recorded")
        return 0
    if cmd == "rollback":
        out = hub.rollback(args.client, args.apply_id, args.yes, args.dry_run, args.force_paths, ACTOR)
        lines = [f"{out['status']}: apply {out.get('apply_id')}"]
        lines += [f"  {c['action']:8} {c['root']}:{c['path']}" for c in out["changed"]]
        lines += [f"  CONFLICT {c['root']}:{c['path']}: {c['reason']} (skipped; --force-paths to override)" for c in out["conflicts"]]
        if out.get("message"):
            lines.append(out["message"])
        if args.dry_run:
            lines += [c["diff"] for c in out["changed"]]
        _emit(args, out, "\n".join(lines))
        return 1 if (out["conflicts"] and not args.dry_run) else 0
    if cmd == "migrate-content":
        r = hub.migrate_content(args.to, args.yes)
        if r["dry_run"]:
            _emit(args, r, f"would move {r['from']} -> {r['to']}; re-run with --yes")
            return 1
        _emit(args, r, f"content moved to {r['to']} (HEAD {str(r['head'])[:12]}). Set HUB_CONTENT_DIR={r['to']} "
                       "(not needed if that is <data_dir>/content).")
        return 0
    if cmd == "retire-bootstrap-key":
        return _retire_bootstrap_key(hub)
    if cmd == "doctor":
        ks = None
        with contextlib.suppress(Exception):
            import asyncio
            ks = asyncio.run(asyncio.wait_for(hub.knowledge.summary(), 4))
        rep = hub.doctor(ks)
        if args.json or getattr(args, "doctor_json", False):
            print(json.dumps(rep, indent=2))
        else:
            print(f"status: {rep['status']}  ({rep['counts']['crit']} crit, {rep['counts']['warn']} warn, {rep['counts']['info']} info)")
            for f in rep["findings"]:
                if f["severity"] == "ok":
                    continue
                print(f"[{f['severity'].upper():4}] {f['title']}")
                if f["detail"]:
                    print(f"       {f['detail']}")
                if f["fix"]:
                    print("       fix: " + f["fix"].replace("\n", "\n            "))
        return 2 if rep["counts"]["crit"] else 0
    if cmd == "attest-mcp":
        ref = "your MCP scanner"
        if not args.yes:
            print(f"Run the scanner yourself first ({ref}); this command does NOT run it. Then re-run with --yes to attest "
                  f"{args.name!r} as clean for its current command/args/url/pinned_ref/transport.", file=sys.stderr)
            return 1
        with hub.mutating():
            item = hub.work.attest_mcp(args.name)
        _emit(args, {"name": item.name, "scan_status": "clean"}, f"attested {item.name} (uncommitted; commit it to approve). Scanner: {ref}")
        return 0
    if cmd == "adapters":
        views = hub.adapters_view()
        _emit(args, {"adapters": views, "load_errors": hub.registry.load_errors},
              "\n".join(f"{v['id']:14} {v['display_name']}" for v in views) or "no adapters installed")
        return 0
    if cmd == "validate":
        hub.require_repo()
        r = hub.validate_content()
        _emit(args, r, "content is valid" if r["ok"] else "\n".join(f"{p['path']}: {p['message']}" for p in r["problems"]))
        return 0 if r["ok"] else 1
    if cmd == "plan":
        plan = hub.plan(args.client).model_dump(mode="json")
        _emit(args, plan, _plan_text(plan))
        return 2 if any(c["blocked"] for c in plan["clients"]) else 0
    if cmd == "apply":
        plan = hub.plan(args.client).model_dump(mode="json")
        if any(c["blocked"] for c in plan["clients"]):
            _emit(args, plan, _plan_text(plan))
            return 2
        if not args.yes:
            _emit(args, plan, _plan_text(plan) + "\n\nnothing applied: re-run with --yes to apply this plan")
            return 1
        res = hub.apply(plan["id"], True, args.adopt, ACTOR)
        bad = any(r["status"] == "failed" or r["verify_ok"] is False for r in res["results"])
        _emit(args, res, "\n".join(f"{r['client']}: {r['status']}, wrote {len(r['written'])}, removed {len(r['removed'])}, "
                                   f"{len(r['skipped_conflicts'])} conflict(s) skipped"
                                   + (f", verify {'ok' if r['verify_ok'] else 'FAILED'}" if r["verify_ok"] is not None else "")
                                   for r in res["results"]))
        return 1 if bad else 0
    if cmd in ("verify", "drift", "audit"):
        results = []
        code = 0
        for c in _clients(hub, args.client):
            r = getattr(hub, "audit_client" if cmd == "audit" else cmd)(c)
            results.append(r)
            if (cmd == "verify" and not r["ok"]) or (cmd == "audit" and not r["ok"]):
                code = 2 if cmd == "audit" else 1
        _emit(args, results, "\n".join(json.dumps(r, default=str)[:600] for r in results))
        return code
    if cmd == "import":
        selection = None if args.all else [tuple(i.split(":", 1)) for i in args.item]
        if not args.all and not selection:
            print("error: pass --all or --item kind:name", file=sys.stderr)
            return 1
        r = hub.import_items(args.client, selection, args.on_conflict)
        _emit(args, r, "\n".join(f"{x['status']:10} {x['kind']}:{x['final_name']} {x['detail']}" for x in r["results"]))
        return 1 if any(x["status"] == "error" for x in r["results"]) else 0
    if cmd == "serve":
        import uvicorn

        from agent_hub.main import create_app
        try:
            check_bind(hub.cfg.host)
        except ValueError as exc:
            raise Unavailable(str(exc), code="nonloopback_bind_refused") from exc
        port = args.port or hub.cfg.port
        hub.cfg.port = port                                    # the Host allowlist follows the effective bind
        kf = hub.auth.ensure_bootstrap_key(hub.cfg.data_dir)
        keyfile = kf or hub.cfg.data_dir / BOOTSTRAP_KEY_FILE
        if keyfile.exists():
            print(f"admin key file: {keyfile} (0600). Use it: export HUB_API_KEY=$(cat {keyfile})  # the UI asks for it on first load\n"
                  "Better: store the key in a password manager, then run `hubctl retire-bootstrap-key` so no key sits on disk.")
        else:
            print("no bootstrap key file on disk (retired): hubctl and the UI prompt for the admin key")
        if hub.cfg.trust_loopback:
            print("WARNING: HUB_TRUST_LOOPBACK is on: local processes are admin without a key", file=sys.stderr)
        uvicorn.run(create_app(hub.cfg, hub), host=hub.cfg.host, port=port, log_config=None)
        return 0
    return 1


def main() -> None:
    sys.exit(run())


if __name__ == "__main__":
    main()
