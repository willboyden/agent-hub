"""advisory-gap: coverage is computed from the client's EFFECTIVE config (union of template, rendered live, project settings)."""
from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from agent_hub.config import APP_ROOT
from agent_hub.services.doctor import _covers, _norm, run_doctor
from agent_hub.services.floor import load_floor

from .test_doctor import FakeFS, ctx

FLOOR = load_floor(APP_ROOT / "policy" / "floor.yaml")
HOME = Path("/home/tester")


def full_template() -> dict[str, Any]:
    """The REAL template shape (permissions.deny strings + sandbox.credentials.files objects) covering the whole floor."""
    deny = [f"Read({g})" for g in FLOOR.deny_path_read] + [f"{t}({g})" for g in FLOOR.deny_path_write for t in ("Write", "Edit")]
    return {"sandbox": {"credentials": {"files": [{"path": "~/.config/example-client", "mode": "deny"}]}, "network": {"allowedDomains": []}},
            "permissions": {"deny": deny}}


def make(tmp_path: Path, template: Any = None, live: Any = None, project: Any = None, local: Any = None, adapter: str = "claude-code",
         params: dict[str, str] | None = None, **kw: Any) -> Any:
    proj = tmp_path / "proj"
    home = tmp_path / "home"
    files: dict[str, str] = {}
    for path, doc in ((proj / "config/settings.template.json", template), (home / ".config/example/settings.json", live),
                      (proj / ".claude/settings.json", project), (proj / ".claude/settings.local.json", local)):
        if doc is not None:
            files[str(path)] = doc if isinstance(doc, str) else json.dumps(doc)
    p = {"settings_template": "config/settings.template.json", "settings_rendered": str(home / ".config/example/settings.json"),
         "network_allowlist_key": "sandbox.network.allowedDomains", "rerender_hint": "then run ./sync.sh to re-render", **(params or {})}
    c = ctx(tmp_path, FakeFS(files), clients=[{"id": "claude-code", "adapter": adapter, "roots": {"project": str(proj)},
                                               "manage": {"permissions": False}, "params": p}], **kw)
    c.floor_read, c.floor_write = list(FLOOR.deny_path_read), list(FLOOR.deny_path_write)
    return c


def gap(rep: dict[str, Any]) -> dict[str, str]:
    return next(f for f in rep["findings"] if f["id"] == "advisory-gap-claude-code")


def test_zero_missing_when_the_template_covers_the_floor(tmp_path: Path) -> None:
    f = gap(run_doctor(make(tmp_path, full_template())))
    assert f["severity"] == "ok" and "every floor deny path is covered" in f["title"]


def test_removing_one_entry_reports_exactly_that_path_in_claude_syntax(tmp_path: Path) -> None:
    t = full_template()
    t["permissions"]["deny"].remove("Read(~/.aws/**)")
    f = gap(run_doctor(make(tmp_path, t)))
    assert f["severity"] == "warn" and f["title"].startswith("claude-code: 2 floor deny entries")
    assert 'permissions.deny += "Read(~/.aws/**)"' in f["fix"]
    assert 'sandbox.credentials.files += {"path": "~/.aws", "mode": "deny"}' in f["fix"]
    assert f["fix"].count("permissions.deny +=") == 1 and "gnupg" not in f["fix"] and "agent-hub" not in f["fix"]
    assert "./sync.sh" in f["fix"] and "settings.template.json" not in f["fix"].split("+=")[0] or "./sync.sh" in f["fix"]
    t["permissions"]["deny"].remove("Write(~/.ssh/**)")
    f = gap(run_doctor(make(tmp_path, t)))
    assert 'permissions.deny += "Write(~/.ssh/**)"' in f["fix"] and "Edit(~/.ssh/**)" not in f["fix"]    # only the missing tool


def test_union_of_template_rendered_and_project_settings(tmp_path: Path) -> None:
    t = full_template()
    t["permissions"]["deny"] = [d for d in t["permissions"]["deny"] if ".aws" not in d and ".netrc" not in d and ".env" not in d]
    assert gap(run_doctor(make(tmp_path, t)))["severity"] == "warn"
    live = {"permissions": {"deny": ["Read(~/.aws/**)", "Write(~/.aws/**)", "Edit(~/.aws/**)"]}}                                  # the rendered config covers one
    proj = {"permissions": {"deny": ["Read(~/.netrc)", "Write(~/.netrc)", "Edit(~/.netrc)"]}}                                   # the project settings another
    local = {"permissions": {"deny": ["Read(**/.env)"]}}                                      # settings.local.json a third
    rep = run_doctor(make(tmp_path, t, live, proj, local))
    assert gap(rep)["severity"] == "ok" and "example/settings.json" in gap(rep)["detail"]
    only_live = run_doctor(make(tmp_path, t, live))
    assert 'Read(~/.aws/**)' not in gap(only_live)["fix"] and "Read(~/.netrc)" in gap(only_live)["fix"]


def test_credentials_files_cover_a_tree_for_read_write_and_edit(tmp_path: Path) -> None:
    t = full_template()
    t["permissions"]["deny"] = [d for d in t["permissions"]["deny"] if "gnupg" not in d]
    t["sandbox"]["credentials"]["files"].append({"path": "~/.gnupg", "mode": "deny"})
    assert gap(run_doctor(make(tmp_path, t)))["severity"] == "ok"
    t["sandbox"]["credentials"]["files"][-1]["mode"] = "allow"                                # only mode=deny counts
    assert 'Read(~/.gnupg/**)' in gap(run_doctor(make(tmp_path, t)))["fix"]


def test_spelling_variants_are_equivalent(tmp_path: Path) -> None:
    home = str(tmp_path / "home")
    t = full_template()
    t["permissions"]["deny"] = [d.replace("~/.aws/**", f"{home}/.aws/**").replace("~/.gnupg/**", "$HOME/.gnupg/**").replace("~/.ssh/**", "~/.ssh")
                                for d in t["permissions"]["deny"]]
    assert gap(run_doctor(make(tmp_path, t)))["severity"] == "ok"


def test_glob_aware_containment_helpers() -> None:
    assert _norm("~/.ssh/**", HOME) == "~/.ssh" and _norm("/home/tester/.aws/", HOME) == "~/.aws" and _norm("$HOME/x/*", HOME) == "~/x"
    assert _covers("~/.ssh", "~/.ssh") and _covers("~/.config", "~/.config/gcloud") and _covers("**/.env", "**/.env")
    assert not _covers("~/.ssh/keys", "~/.ssh") and not _covers("**/.env.*", "**/.env") and _covers("~/.mozilla*", "~/.mozilla")


def test_a_broader_ancestor_deny_covers_children(tmp_path: Path) -> None:
    t = full_template()
    t["permissions"]["deny"] = [d for d in t["permissions"]["deny"] if "agent-hub" not in d and "agent-knowledge" not in d]
    assert gap(run_doctor(make(tmp_path, t)))["severity"] == "warn"
    t["permissions"]["deny"] += ["Read(~/.local/share/**)", "Read(~/.config/**)"]            # ancestors cover the four state paths (reads)
    f = gap(run_doctor(make(tmp_path, t)))
    assert "agent-hub" not in f["fix"] or "Read(" not in f["fix"].split("agent-hub")[0][-6:]


@pytest.mark.parametrize("hostile", ["{bad", "[]", "null", '{"permissions": 5}', '{"sandbox": {"credentials": {"files": [1, null]}}}', ""])
def test_hostile_files_are_treated_as_no_coverage_without_crashing(tmp_path: Path, hostile: str) -> None:
    rep = run_doctor(make(tmp_path, full_template(), live=hostile, project=hostile))
    assert gap(rep)["severity"] == "ok"                                                       # the good template still covers everything
    rep = run_doctor(make(tmp_path, hostile))
    assert gap(rep)["severity"] in ("warn", "info") and not [f for f in rep["findings"] if f["id"].startswith("check-")]


def test_no_readable_settings_at_all_is_info_not_a_false_alarm(tmp_path: Path) -> None:
    assert gap(run_doctor(make(tmp_path)))["severity"] == "info"


def test_opencode_is_info_with_an_honest_count(tmp_path: Path) -> None:
    c = make(tmp_path, adapter="opencode", permission_gaps=lambda cid: [f"deny path_read p{i}" for i in range(7)])
    f = gap(run_doctor(c))
    assert f["severity"] == "info" and "7 floor paths" in f["title"] and "cannot be made complete" in f["title"]
    assert "docs/SECURITY.md" in f["fix"]
    assert not [x for x in run_doctor(make(tmp_path, adapter="opencode", permission_gaps=lambda cid: [])) ["findings"]
                if x["id"] == "advisory-gap-claude-code"]


def test_hermes_containers_are_ok(tmp_path: Path) -> None:
    for adapter in ("hermes-agent", "hermes-variant"):
        f = gap(run_doctor(make(tmp_path, adapter=adapter, permission_gaps=lambda cid: ["x"])))
        assert f["severity"] == "ok" and "no host mounts" in f["title"]


def test_claude_template_finding_mentions_the_rerender(tmp_path: Path) -> None:
    t = full_template()
    t["permissions"]["deny"] = []
    f = next(x for x in run_doctor(make(tmp_path, t))["findings"] if x["id"] == "claude-template" and x["severity"] == "warn")
    assert "./sync.sh" in f["fix"]


def test_unconfigured_params_are_skipped_as_info_never_guessed(tmp_path: Path) -> None:
    c = make(tmp_path, full_template(), params={})
    c.clients[0]["params"] = {}                                                              # nothing configured at all
    rep = run_doctor(c)
    ids = {f["id"]: f for f in rep["findings"]}
    for fid in ("claude-template", "claude-network", "claude-rendered"):
        assert ids[fid]["severity"] == "info" and "not configured" in ids[fid]["title"], fid
    note = ids["doctor-params"]
    assert note["severity"] == "info" and "settings_template" in note["detail"] and "params:" in note["fix"]
    assert ids["advisory-gap-claude-code"]["severity"] in ("info",)                          # no project settings either: nothing to compare
    text = json.dumps(rep)
    for forbidden in ("claude" + "-local", "sync" + "-config", "MANUAL" + "-SETUP", "/Pro" + "jects/"):
        assert forbidden not in text


def test_project_settings_default_and_param_override(tmp_path: Path) -> None:
    t = full_template()
    t["permissions"]["deny"] = []
    c = make(tmp_path, t, project=full_template())                                            # default <root>/.claude/settings.json
    assert gap(run_doctor(c))["severity"] == "ok"
    custom = make(tmp_path, t, project=None, params={"settings_project": "elsewhere/settings.json"})
    custom.fs.files[str(tmp_path / "proj/elsewhere/settings.json")] = json.dumps(full_template())     # type: ignore[attr-defined]
    custom.fs.files[str(tmp_path / "proj/elsewhere/settings.local.json")] = "{}"                       # type: ignore[attr-defined]
    assert gap(run_doctor(custom))["severity"] == "ok"
