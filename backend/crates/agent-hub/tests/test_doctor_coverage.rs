//! Port of backend/tests/core/test_doctor_coverage.py: advisory-gap coverage is computed from the client's EFFECTIVE
//! config (union of template, rendered live, project settings). Plus a golden replay of the doctor's Python-compatible
//! helpers (`_norm`, `_covers`, the ReadWritePaths regex, `str.capitalize`, `json.loads`, `claude_settings_gaps`).

mod support_wsf;

use std::path::{Path, PathBuf};

use agent_hub::services::doctor::{
    capitalize, claude_settings_gaps, covers, json_loads, norm, read_write_paths_values,
};
use agent_hub::services::floor::{FloorPolicy, load_floor};
use hub_core::value::PyValue;
use serde_json::{Value, json};
use support_wsf::doctor_fakes::{Ctx, FakeFs, ctx};
use support_wsf::{E, R, p};

fn floor() -> Result<FloorPolicy, E> {
    load_floor(&agent_hub::config::app_root().join("policy/floor.yaml")).map_err(p)
}

/// The REAL template shape (permissions.deny strings + sandbox.credentials.files objects) covering the whole floor.
fn full_template() -> Result<Value, E> {
    let f = floor()?;
    let mut deny: Vec<String> = f.deny_path_read.iter().map(|g| format!("Read({g})")).collect();
    deny.extend(
        f.deny_path_write
            .iter()
            .flat_map(|g| ["Write", "Edit"].map(|t| format!("{t}({g})"))),
    );
    Ok(
        json!({"sandbox": {"credentials": {"files": [{"path": "~/.config/example-client", "mode": "deny"}]},
                          "network": {"allowedDomains": []}},
              "permissions": {"deny": deny}}),
    )
}

#[derive(Default)]
struct Docs {
    template: Option<String>,
    live: Option<String>,
    project: Option<String>,
    local: Option<String>,
}

fn doc(v: &Value) -> Option<String> {
    Some(v.to_string())
}

/// `make(tmp, template, live, project, local, adapter, params)`.
fn make(tmp: &Path, docs: &Docs, adapter: &str, params: Option<Value>) -> Result<Ctx, E> {
    let proj = tmp.join("proj");
    let home = tmp.join("home");
    let mut fs = FakeFs::default();
    for (path, d) in [
        (proj.join("config/settings.template.json"), &docs.template),
        (home.join(".config/example/settings.json"), &docs.live),
        (proj.join(".claude/settings.json"), &docs.project),
        (proj.join(".claude/settings.local.json"), &docs.local),
    ] {
        if let Some(d) = d {
            fs.files.insert(path.to_string_lossy().into(), d.clone());
        }
    }
    let mut prm = json!({"settings_template": "config/settings.template.json",
                         "settings_rendered": home.join(".config/example/settings.json").to_string_lossy(),
                         "network_allowlist_key": "sandbox.network.allowedDomains",
                         "rerender_hint": "then run ./sync.sh to re-render"});
    if let (Some(m), Some(Value::Object(x))) = (prm.as_object_mut(), params) {
        for (k, v) in x {
            m.insert(k, v);
        }
    }
    let mut c = ctx(tmp, fs)?;
    c.clients = vec![
        json!({"id": "claude-code", "adapter": adapter, "roots": {"project": proj.to_string_lossy()},
                            "manage": {"permissions": false}, "params": prm}),
    ];
    let f = floor()?;
    c.floor_read = f.deny_path_read;
    c.floor_write = f.deny_path_write;
    Ok(c)
}

fn gap(rep: &Value) -> Result<Value, E> {
    rep["findings"]
        .as_array()
        .and_then(|a| a.iter().find(|f| f["id"] == "advisory-gap-claude-code").cloned())
        .ok_or_else(|| "no advisory-gap-claude-code finding".into())
}

fn text(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

fn template_without(pred: impl Fn(&str) -> bool) -> Result<Value, E> {
    let mut t = full_template()?;
    let deny: Vec<Value> = t["permissions"]["deny"]
        .as_array()
        .ok_or("deny")?
        .iter()
        .filter(|d| !pred(d.as_str().unwrap_or_default()))
        .cloned()
        .collect();
    t["permissions"]["deny"] = Value::Array(deny);
    Ok(t)
}

#[test]
fn test_zero_missing_when_the_template_covers_the_floor() -> R {
    let d = tempfile::tempdir()?;
    let docs = Docs {
        template: doc(&full_template()?),
        ..Docs::default()
    };
    let f = gap(&make(d.path(), &docs, "claude-code", None)?.run())?;
    assert!(f["severity"] == "ok" && text(&f, "title").contains("every floor deny path is covered"));
    Ok(())
}

#[test]
fn test_removing_one_entry_reports_exactly_that_path_in_claude_syntax() -> R {
    let d = tempfile::tempdir()?;
    let t = template_without(|s| s == "Read(~/.aws/**)")?;
    let f = gap(&make(
        d.path(),
        &Docs {
            template: doc(&t),
            ..Docs::default()
        },
        "claude-code",
        None,
    )?
    .run())?;
    let fix = text(&f, "fix");
    assert!(f["severity"] == "warn" && text(&f, "title").starts_with("claude-code: 2 floor deny entries"));
    assert!(fix.contains("permissions.deny += \"Read(~/.aws/**)\""));
    assert!(fix.contains("sandbox.credentials.files += {\"path\": \"~/.aws\", \"mode\": \"deny\"}"));
    assert!(fix.matches("permissions.deny +=").count() == 1 && !fix.contains("gnupg") && !fix.contains("agent-hub"));
    assert!(fix.contains("./sync.sh"));
    let t = template_without(|s| s == "Read(~/.aws/**)" || s == "Write(~/.ssh/**)")?;
    let f = gap(&make(
        d.path(),
        &Docs {
            template: doc(&t),
            ..Docs::default()
        },
        "claude-code",
        None,
    )?
    .run())?;
    let fix = text(&f, "fix");
    assert!(fix.contains("permissions.deny += \"Write(~/.ssh/**)\"") && !fix.contains("Edit(~/.ssh/**)")); // only the missing tool
    Ok(())
}

#[test]
fn test_union_of_template_rendered_and_project_settings() -> R {
    let d = tempfile::tempdir()?;
    let t = template_without(|s| s.contains(".aws") || s.contains(".netrc") || s.contains(".env"))?;
    let only_t = Docs {
        template: doc(&t),
        ..Docs::default()
    };
    assert_eq!(
        gap(&make(d.path(), &only_t, "claude-code", None)?.run())?["severity"],
        "warn"
    );
    let live = json!({"permissions": {"deny": ["Read(~/.aws/**)", "Write(~/.aws/**)", "Edit(~/.aws/**)"]}}); // covers one
    let proj = json!({"permissions": {"deny": ["Read(~/.netrc)", "Write(~/.netrc)", "Edit(~/.netrc)"]}}); // another
    let local = json!({"permissions": {"deny": ["Read(**/.env)"]}}); // a third
    let all = Docs {
        template: doc(&t),
        live: doc(&live),
        project: doc(&proj),
        local: doc(&local),
    };
    let rep = make(d.path(), &all, "claude-code", None)?.run();
    assert!(gap(&rep)?["severity"] == "ok" && text(&gap(&rep)?, "detail").contains("example/settings.json"));
    let only_live = Docs {
        template: doc(&t),
        live: doc(&live),
        ..Docs::default()
    };
    let fix = text(&gap(&make(d.path(), &only_live, "claude-code", None)?.run())?, "fix");
    assert!(!fix.contains("Read(~/.aws/**)") && fix.contains("Read(~/.netrc)"));
    Ok(())
}

#[test]
fn test_credentials_files_cover_a_tree_for_read_write_and_edit() -> R {
    let d = tempfile::tempdir()?;
    let mut t = template_without(|s| s.contains("gnupg"))?;
    if let Some(files) = t["sandbox"]["credentials"]["files"].as_array_mut() {
        files.push(json!({"path": "~/.gnupg", "mode": "deny"}));
    }
    let run = |t: &Value| -> Result<Value, E> {
        gap(&make(
            d.path(),
            &Docs {
                template: doc(t),
                ..Docs::default()
            },
            "claude-code",
            None,
        )?
        .run())
    };
    assert_eq!(run(&t)?["severity"], "ok");
    t["sandbox"]["credentials"]["files"][1]["mode"] = json!("allow"); // only mode=deny counts
    assert!(text(&run(&t)?, "fix").contains("Read(~/.gnupg/**)"));
    Ok(())
}

#[test]
fn test_spelling_variants_are_equivalent() -> R {
    let d = tempfile::tempdir()?;
    let home = d.path().join("home").to_string_lossy().to_string();
    let mut t = full_template()?;
    let deny: Vec<Value> = t["permissions"]["deny"]
        .as_array()
        .ok_or("deny")?
        .iter()
        .map(|x| {
            json!(
                x.as_str()
                    .unwrap_or_default()
                    .replace("~/.aws/**", &format!("{home}/.aws/**"))
                    .replace("~/.gnupg/**", "$HOME/.gnupg/**")
                    .replace("~/.ssh/**", "~/.ssh")
            )
        })
        .collect();
    t["permissions"]["deny"] = Value::Array(deny);
    let f = gap(&make(
        d.path(),
        &Docs {
            template: doc(&t),
            ..Docs::default()
        },
        "claude-code",
        None,
    )?
    .run())?;
    assert_eq!(f["severity"], "ok");
    Ok(())
}

#[test]
fn test_glob_aware_containment_helpers() {
    let home = Path::new("/home/tester");
    assert!(
        norm("~/.ssh/**", home) == "~/.ssh"
            && norm("/home/tester/.aws/", home) == "~/.aws"
            && norm("$HOME/x/*", home) == "~/x"
    );
    assert!(covers("~/.ssh", "~/.ssh") && covers("~/.config", "~/.config/gcloud") && covers("**/.env", "**/.env"));
    assert!(!covers("~/.ssh/keys", "~/.ssh") && !covers("**/.env.*", "**/.env") && covers("~/.mozilla*", "~/.mozilla"));
}

#[test]
fn test_a_broader_ancestor_deny_covers_children() -> R {
    let d = tempfile::tempdir()?;
    let mut t = template_without(|s| s.contains("agent-hub") || s.contains("agent-knowledge"))?;
    let run = |t: &Value| -> Result<Value, E> {
        gap(&make(
            d.path(),
            &Docs {
                template: doc(t),
                ..Docs::default()
            },
            "claude-code",
            None,
        )?
        .run())
    };
    assert_eq!(run(&t)?["severity"], "warn");
    if let Some(deny) = t["permissions"]["deny"].as_array_mut() {
        deny.push(json!("Read(~/.local/share/**)"));
        deny.push(json!("Read(~/.config/**)")); // ancestors cover the four state paths (reads)
    }
    let fix = text(&run(&t)?, "fix");
    let before = fix.split("agent-hub").next().unwrap_or_default();
    let tail: String = before
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    assert!(!fix.contains("agent-hub") || !tail.contains("Read("));
    Ok(())
}

#[test]
fn test_hostile_files_are_treated_as_no_coverage_without_crashing() -> R {
    for hostile in [
        "{bad",
        "[]",
        "null",
        r#"{"permissions": 5}"#,
        r#"{"sandbox": {"credentials": {"files": [1, null]}}}"#,
        "",
    ] {
        let d = tempfile::tempdir()?;
        let h = Some(hostile.to_string());
        let docs = Docs {
            template: doc(&full_template()?),
            live: h.clone(),
            project: h.clone(),
            ..Docs::default()
        };
        assert_eq!(
            gap(&make(d.path(), &docs, "claude-code", None)?.run())?["severity"],
            "ok"
        ); // the good template still covers everything
        let rep = make(
            d.path(),
            &Docs {
                template: h,
                ..Docs::default()
            },
            "claude-code",
            None,
        )?
        .run();
        assert!(["warn", "info"].contains(&text(&gap(&rep)?, "severity").as_str()));
        assert!(
            !rep["findings"]
                .as_array()
                .ok_or("findings")?
                .iter()
                .any(|f| text(f, "id").starts_with("check-"))
        );
    }
    Ok(())
}

#[test]
fn test_no_readable_settings_at_all_is_info_not_a_false_alarm() -> R {
    let d = tempfile::tempdir()?;
    assert_eq!(
        gap(&make(d.path(), &Docs::default(), "claude-code", None)?.run())?["severity"],
        "info"
    );
    Ok(())
}

#[test]
fn test_opencode_is_info_with_an_honest_count() -> R {
    let d = tempfile::tempdir()?;
    let mut c = make(d.path(), &Docs::default(), "opencode", None)?;
    c.gaps = Box::new(|_| Some((0..7).map(|i| format!("deny path_read p{i}")).collect()));
    let f = gap(&c.run())?;
    assert!(
        f["severity"] == "info"
            && text(&f, "title").contains("7 floor paths")
            && text(&f, "title").contains("cannot be made complete")
    );
    assert!(text(&f, "fix").contains("docs/SECURITY.md"));
    let mut c = make(d.path(), &Docs::default(), "opencode", None)?;
    c.gaps = Box::new(|_| Some(Vec::new()));
    assert!(gap(&c.run()).is_err());
    Ok(())
}

#[test]
fn test_hermes_containers_are_ok() -> R {
    let d = tempfile::tempdir()?;
    for adapter in ["hermes-agent", "hermes-variant"] {
        let mut c = make(d.path(), &Docs::default(), adapter, None)?;
        c.gaps = Box::new(|_| Some(vec!["x".into()]));
        let f = gap(&c.run())?;
        assert!(
            f["severity"] == "ok" && text(&f, "title").contains("no host mounts"),
            "{adapter}"
        );
    }
    Ok(())
}

#[test]
fn test_claude_template_finding_mentions_the_rerender() -> R {
    let d = tempfile::tempdir()?;
    let t = template_without(|_| true)?;
    let rep = make(
        d.path(),
        &Docs {
            template: doc(&t),
            ..Docs::default()
        },
        "claude-code",
        None,
    )?
    .run();
    let f = rep["findings"]
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|x| x["id"] == "claude-template" && x["severity"] == "warn")
        })
        .ok_or("claude-template warn")?;
    assert!(text(f, "fix").contains("./sync.sh"));
    Ok(())
}

#[test]
fn test_unconfigured_params_are_skipped_as_info_never_guessed() -> R {
    let d = tempfile::tempdir()?;
    let mut c = make(
        d.path(),
        &Docs {
            template: doc(&full_template()?),
            ..Docs::default()
        },
        "claude-code",
        None,
    )?;
    c.clients[0]["params"] = json!({}); // nothing configured at all
    let rep = c.run();
    let find = |id: &str| -> Result<Value, E> {
        rep["findings"]
            .as_array()
            .and_then(|a| a.iter().find(|f| f["id"] == id).cloned())
            .ok_or_else(|| format!("no {id}").into())
    };
    for fid in ["claude-template", "claude-network", "claude-rendered"] {
        let f = find(fid)?;
        assert!(
            f["severity"] == "info" && text(&f, "title").contains("not configured"),
            "{fid}"
        );
    }
    let note = find("doctor-params")?;
    assert!(
        note["severity"] == "info"
            && text(&note, "detail").contains("settings_template")
            && text(&note, "fix").contains("params:")
    );
    assert_eq!(find("advisory-gap-claude-code")?["severity"], "info"); // no project settings either
    let all = rep.to_string();
    for forbidden in ["claude-local", "sync-config", "MANUAL-SETUP", "/Projects/"] {
        assert!(!all.contains(forbidden), "{forbidden}");
    }
    Ok(())
}

#[test]
fn test_project_settings_default_and_param_override() -> R {
    let d = tempfile::tempdir()?;
    let t = template_without(|_| true)?;
    let c = make(
        d.path(),
        &Docs {
            template: doc(&t),
            project: doc(&full_template()?),
            ..Docs::default()
        },
        "claude-code",
        None,
    )?;
    assert_eq!(gap(&c.run())?["severity"], "ok"); // default <root>/.claude/settings.json
    let mut custom = make(
        d.path(),
        &Docs {
            template: doc(&t),
            ..Docs::default()
        },
        "claude-code",
        Some(json!({"settings_project": "elsewhere/settings.json"})),
    )?;
    custom.fs.files.insert(
        d.path().join("proj/elsewhere/settings.json").to_string_lossy().into(),
        full_template()?.to_string(),
    );
    custom.fs.files.insert(
        d.path()
            .join("proj/elsewhere/settings.local.json")
            .to_string_lossy()
            .into(),
        "{}".into(),
    );
    assert_eq!(gap(&custom.run())?["severity"], "ok");
    Ok(())
}

// ---- golden table: the doctor's Python-compatible helpers -------------------------------------------------------------

fn golden_doctor() -> Result<Value, E> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/floor_decisions.json");
    let g: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    Ok(g["doctor"].clone())
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

#[test]
fn golden_doctor_helpers_match_python() -> R {
    let g = golden_doctor()?;
    let home = Path::new("/home/tester");
    let mut bad: Vec<String> = Vec::new();
    let mut n = 0;
    let rows = |k: &str| g[k].as_array().cloned().unwrap_or_default();
    for c in rows("norm") {
        n += 1;
        if norm(s(&c["p"]), home) != s(&c["norm"]) {
            bad.push(format!("norm {c}"));
        }
    }
    for c in rows("covers") {
        n += 1;
        if covers(s(&c["pattern"]), s(&c["target"])) != c["covers"] {
            bad.push(format!("covers {c}"));
        }
    }
    for c in rows("units") {
        n += 1;
        if json!(read_write_paths_values(s(&c["text"]))) != c["values"] {
            bad.push(format!("units {c} -> {:?}", read_write_paths_values(s(&c["text"]))));
        }
    }
    for c in rows("capitalize") {
        n += 1;
        if capitalize(s(&c["s"])) != s(&c["cap"]) {
            bad.push(format!("capitalize {c} -> {}", capitalize(s(&c["s"]))));
        }
    }
    for c in rows("json") {
        n += 1;
        let got = json_loads(s(&c["text"])).map(|v| v.repr());
        if json!(got) != c["repr"] {
            bad.push(format!("json {c} -> {got:?}"));
        }
    }
    for c in rows("gaps") {
        n += 1;
        if json!(claude_settings_gaps(&PyValue::from_json(&c["doc"]))) != c["gaps"] {
            bad.push(format!("gaps {c}"));
        }
    }
    assert!(
        bad.is_empty() && n > 0,
        "{} of {n} differ: {:#?}",
        bad.len(),
        bad.iter().take(5).collect::<Vec<_>>()
    );
    Ok(())
}

/// json_loads accepts what Python's json.loads accepts at the edges the doctor meets: lone surrogates, NaN,
/// deep nesting up to CPython 3.13's RecursionError depth, int-from-str digit limit.
#[test]
fn doctor_json_loads_edges() {
    assert!(json_loads(r#""\ud800""#).is_some());
    assert!(json_loads(r#"{"a": NaN, "b": -Infinity}"#).is_some_and(|v| matches!(v, PyValue::Dict(_))));
    let ok = format!("{}{}", "[".repeat(9998), "]".repeat(9998));
    assert!(json_loads(&ok).is_some());
    let too_deep = format!("{}{}", "[".repeat(9999), "]".repeat(9999));
    assert!(json_loads(&too_deep).is_none());
    assert!(json_loads(&"1".repeat(4300)).is_some() && json_loads(&"1".repeat(4301)).is_none());
    assert!(json_loads("\u{feff}{}").is_none() && json_loads("{\"a\":1,}").is_none() && json_loads("[1,]").is_none());
    assert_eq!(
        json_loads(r#"{"a": 1, "a": 2}"#).map(|v| v.repr()),
        Some("{'a': 2}".to_string())
    );
}
