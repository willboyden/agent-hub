"""Golden cases for hub-core's pydantic-compatible validation (crates/hub-core/tests/golden/models.json).

    "$PYREL/backend/.venv/bin/python" -I backend/parity/gen/gen_models_golden.py backend/crates/hub-core/tests/golden/models.json
    (from the repo root; PYREL=$(node backend/parity/lib/python-release.mjs) is the last Python backend, see
     backend/parity/lib/python-release.mjs)

For each (model, input) the file records what pydantic 2.13.5 does: the dumped model (mode="json", and with
exclude_none) or the errors() list (loc/msg/type) and str(ValidationError). Inputs are JSON values, or YAML text
(`Yaml(...)`, loaded with yaml.safe_load like content frontmatter) for what JSON cannot express: non-str keys."""
from __future__ import annotations

import json
import sys
from typing import Any

import yaml
from pydantic import ValidationError

from agent_hub.domain import models as m
from agent_hub.api.routers import changes, clients, content, organize, skilleval, system

MODELS = {
    "HubMeta": m.HubMeta, "SkillMeta": m.SkillMeta, "Sidecar": m.Sidecar, "InstructionMeta": m.InstructionMeta,
    "AgentMeta": m.AgentMeta, "McpDoc": m.McpDoc, "RuleDoc": m.RuleDoc, "MemoryMeta": m.MemoryMeta, "InboxMeta": m.InboxMeta,
    "Member": m.Member, "CollectionDoc": m.CollectionDoc, "ClientDoc": m.ClientDoc, "Selection": m.Selection,
    "ProfileDoc": m.ProfileDoc,
    "KeyIn": system.KeyIn, "ImportIn": clients.ImportIn, "ObservedImportIn": clients.ObservedImportIn,
    "RollbackIn": clients.RollbackIn, "DuplicateIn": content.DuplicateIn, "InboxIn": content.InboxIn,
    "MembersIn": organize.MembersIn, "ReorderIn": organize.ReorderIn, "BulkIn": organize.BulkIn, "ToggleIn": organize.ToggleIn,
    "BulkToggleIn": organize.BulkToggleIn, "AcceptIn": organize.AcceptIn, "CommitIn": changes.CommitIn,
    "DiscardIn": changes.DiscardIn, "RevertIn": changes.RevertIn, "PlanIn": changes.PlanIn, "ApplyIn": changes.ApplyIn,
    "EvaluateIn": skilleval.EvaluateIn, "RunIn": skilleval.RunIn,
}

LONG = "x" * 1100


class Yaml(str):
    """An input given as YAML text (keys may be bools, ints, floats or null, as in YAML 1.1 frontmatter)."""


YAML_KEYS = "name: ok\ndescription: d\non: 1\nyes: 2\n5: x\n-3: y\n2.5: z\n~: n\na.b: 1\n"
YAML_NONE_EXTRAS = "name: ok\ndescription: d\nextra_none: ~\nkept: {a: ~, b: [~]}\n"

CASES: list[tuple[str, Any]] = [
    ("HubMeta", {}), ("HubMeta", {"version": 2}), ("HubMeta", {"version": 1, "name": LONG[:301]}), ("HubMeta", {"extra": 1}),
    ("SkillMeta", {"name": "ok", "description": "d", "allowed-tools": "Read", "x": {"y": [1, None]}}),
    ("SkillMeta", {"name": "Bad Name", "description": ""}), ("SkillMeta", {}), ("SkillMeta", {"name": 5, "description": LONG}),
    ("SkillMeta", "not a dict"), ("SkillMeta", None), ("SkillMeta", [1]),
    ("Sidecar", {}), ("Sidecar", {"groups": ["a", "B"], "tags": "x", "notes": LONG + LONG, "provenance": {"k": 1}}),
    ("InstructionMeta", {"id": "a", "title": "t", "order": 5}), ("InstructionMeta", {"id": "a", "title": "t", "order": "7"}),
    ("InstructionMeta", {"id": "a", "title": "t", "order": 5.0}), ("InstructionMeta", {"id": "a", "title": "t", "order": 5.5}),
    ("InstructionMeta", {"id": "a", "title": "t", "order": True}), ("InstructionMeta", {"id": "a", "title": "t", "order": 100001}),
    ("InstructionMeta", {"id": "a", "title": "t", "order": "x"}), ("InstructionMeta", {"id": "a", "title": None}),
    ("AgentMeta", {"name": "a", "description": "d", "capabilities": ["read", "fly", "swim"], "model_tier": "huge", "mode": "x",
                   "read_only": "yes"}),
    ("AgentMeta", {"name": "a", "description": "d", "read_only": "no", "model_tier": None}),
    ("AgentMeta", {"name": "a", "description": "d", "read_only": 2}),
    ("McpDoc", {"name": "s", "transport": "stdio"}), ("McpDoc", {"name": "s", "transport": "http"}),
    ("McpDoc", {"name": "s", "transport": "http", "url": "ftp://x"}),
    ("McpDoc", {"name": "s", "transport": "stdio", "command": "c", "env_names": ["lower"], "egress_hosts": ["a b"]}),
    ("McpDoc", {"name": "s", "transport": "sse", "url": "https://h/x", "scan_status": "clean", "args": ["a", 1]}),
    ("RuleDoc", {"id": "r", "title": "t", "kind": "tool", "match": "a\nb", "decision": "allow"}),
    ("RuleDoc", {"id": "r", "title": "t", "kind": "nope", "match": "", "decision": "maybe"}),
    ("MemoryMeta", {"id": "m", "type": "user", "title": "t", "tags": ["ok"]}),
    ("InboxMeta", {"id": "i", "type": "user", "title": "t", "client": "c", "created": 5}),
    ("CollectionDoc", {"id": "c", "title": "t", "color": "red!", "icon": "x" * 17,
                       "members": [{"kind": "skill", "name": "a"}, {"kind": "skill", "name": "a"}, {"kind": "bad", "name": "b"}]}),
    ("ClientDoc", {"id": "c", "adapter": "generic", "display_name": "x"}),
    ("ClientDoc", {"id": "c", "adapter": "claude-code", "display_name": "x", "manage": {"skills": True, "nope": False},
                   "roots": {"project": "/tmp/p", "Bad": "x"}, "params": {"a": None, "b": [None]}, "icon": None}),
    ("ClientDoc", {"id": "c", "adapter": "claude-code", "display_name": "x", "strict": "false", "manage": {"skills": "yes"}}),
    ("ProfileDoc", {"collections": ["a"], "enable": {"skills": ["x"]}, "disable": {"nope": []}}),
    ("ProfileDoc", {"enable": "x"}),
    ("KeyIn", {"name": "k"}), ("KeyIn", {"name": "k", "role": "admin", "scopes": "x"}), ("KeyIn", {}), ("KeyIn", {"name": "k", "x": 1}),
    ("ImportIn", {"client": "c", "items": [{"kind": "skill", "name": "a"}], "on_conflict": None}),
    ("ImportIn", {"client": "c", "items": [{"kind": "skill"}], "enable": "maybe"}),
    ("ObservedImportIn", {"items": [{"key": "k"}], "on_conflict": "merge"}),
    ("RollbackIn", {"force_paths": "x"}), ("DuplicateIn", {}), ("InboxIn", {"client": "c", "title": "t", "body": "b"}),
    ("BulkIn", {"items": [{"kind": "skill", "name": "a"}], "add_tags": ["x"]}),
    ("ToggleIn", {"client": "c", "kind": "agent", "name": "a", "enabled": "true"}),
    ("ToggleIn", {"client": "c", "kind": "agents", "name": "a", "enabled": None}),
    ("BulkToggleIn", {"clients": "c", "items": [], "enabled": 1}),
    ("AcceptIn", {"ids": ["a"], "overrides": {"a": {"title": "x"}, "b": 1}}),
    ("CommitIn", {"message": 5}), ("DiscardIn", {"paths": None, "confirm": "1"}), ("PlanIn", {"clients": None}),
    ("ApplyIn", {"plan_id": "p", "confirm": "on", "adopt_paths": [1]}),
    ("EvaluateIn", {"tiers": "tier9"}), ("RunIn", {"subjects": ["a"], "select": "some", "tiers": "tier1+2", "force": "off"}),
    # non-str keys (YAML 1.1 `on:`/`yes:` are booleans) and None-valued extras, for every content model's extra mode
    *[(name, Yaml(YAML_KEYS)) for name in ("HubMeta", "SkillMeta", "Sidecar", "InstructionMeta", "AgentMeta", "McpDoc",
                                           "RuleDoc", "MemoryMeta", "InboxMeta", "CollectionDoc", "ClientDoc", "ProfileDoc")],
    ("SkillMeta", Yaml(YAML_NONE_EXTRAS)), ("Sidecar", Yaml("notes: ~\nprovenance: {k: ~}\n")),
    ("ClientDoc", Yaml("id: c\nadapter: generic\ndisplay_name: x\nparams: {1: a, on: b, ~: c}\n")),
    ("ClientDoc", Yaml("id: c\nadapter: generic\ndisplay_name: x\nmanage: {skills: yes, 3: no}\n")),
    ("SkillMeta", Yaml("name: ok\ndescription: d\nx.y: 1\n")),
    # lax scalars (A-10): bool from floats/ints/bytes, big ints, bytes as str, dict keys of any type
    *[("AgentMeta", Yaml(f"name: a\ndescription: d\nread_only: {v}\n"))
      for v in ("2.0", "2.5", "-1.0", ".nan", ".inf", "1.0e+300", "3", "1000000000000000000000000000000", "1.0",
                "!!binary eWVz", "!!binary /w==", "!!binary bWF5YmU=")],
    *[("InstructionMeta", Yaml(f"id: a\ntitle: t\norder: {v}\n"))
      for v in ("1000000000000000000000000000000", "-1000000000000000000000000000000", "1.0e+30", "'99999999999999999999'",
                "'00042'")],
    ("InstructionMeta", Yaml("id: a\ntitle: !!binary aGVsbG8=\n")), ("InstructionMeta", Yaml("id: a\ntitle: !!binary /w==\n")),
    ("ClientDoc", Yaml("id: c\nadapter: generic\ndisplay_name: x\nparams: {2024-01-02: a, 2.5: b, ~: c, yes: d, 100000000000000000000: e}\n")),
]


def main() -> None:
    out = []
    for name, data in CASES:
        model = MODELS[name]
        rec: dict[str, Any] = {"model": name, "input": data}
        if isinstance(data, Yaml):
            rec = {"model": name, "yaml": str(data)}
            data = yaml.safe_load(data)
        try:
            obj = model.model_validate(data)
        except ValidationError as exc:
            rec["ok"] = False
            rec["errors"] = [{"loc": list(e["loc"]), "msg": e["msg"], "type": e["type"]} for e in exc.errors()]
            rec["str"] = str(exc)
            rec["title"] = exc.title
        else:
            rec["ok"] = True
            rec["dump"] = obj.model_dump(mode="json")
            rec["dump_exclude_none"] = obj.model_dump(mode="json", exclude_none=True)
        out.append(rec)
    with open(sys.argv[1], "w") as fh:
        json.dump(out, fh, indent=1, ensure_ascii=False)
        fh.write("\n")
    print(f"{len(out)} cases -> {sys.argv[1]}")


if __name__ == "__main__":
    main()
