//! WS-B1 hub-level golden: tests/golden/b1_hub.json (recorded from the PYTHON Hub with the test FakeAdapter by
//! backend/parity/gen/gen_hub_golden.py) replayed on the Rust Hub with the same FakeAdapter (tests/b1_support): client
//! create/update/views, listing filters, item details, the matrix and toggles, effective and rendered views, discover and
//! import, the inbox. Normalisation is the generator's (temp dir, timestamps, saved random ids).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod b1_support;

use std::collections::BTreeMap;

use agent_hub::services::content::py_class;
use agent_hub::services::hub::content_ops::ListFilter;
use b1_support::{T, hub};
use hub_core::models::Member;
use hub_core::value::PyValue;
use hub_core::{HubResult, Problem};
use indexmap::IndexMap;
use serde_json::{Value, json};

const GOLDEN: &str = include_str!("golden/b1_hub.json");
const TIME_KEYS: [&str; 7] = [
    "adopted_at",
    "last_applied_at",
    "last_verified_at",
    "updated",
    "created",
    "imported_at",
    "observed_at",
];

/// test_organize.py `native`: a client dir that already has content.
fn native_home(t: &T) {
    let home = t.root.join("client-home");
    std::fs::create_dir_all(home.join("skills/native-skill")).unwrap();
    std::fs::write(
        home.join("skills/native-skill/SKILL.md"),
        "---\nname: native-skill\ndescription: From client\n---\nbody\n",
    )
    .unwrap();
    std::fs::write(home.join("skills/native-skill/x.txt"), "x").unwrap();
    std::fs::create_dir_all(home.join("skills/Bad Name")).unwrap();
    std::fs::write(
        home.join("skills/Bad Name/SKILL.md"),
        "---\nname: Bad Name\ndescription: odd\n---\nb\n",
    )
    .unwrap();
    std::fs::create_dir(home.join("agents")).unwrap();
    std::fs::write(home.join("agents/helper.md"), "helper body").unwrap();
    std::fs::write(
        home.join(".mcp.json"),
        r#"{"mcpServers": {"srv": {"command": "run-srv"}}}"#,
    )
    .unwrap();
    std::fs::write(
        home.join("settings.json"),
        r#"{"permissions": {"deny": ["tool:web_fetch"], "ask": ["command:git *"]}}"#,
    )
    .unwrap();
    std::fs::write(home.join("CLAUDE.md"), "Native instructions.\n").unwrap();
}

fn tv<X: serde::Serialize>(v: &X) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn pmap(v: &Value) -> IndexMap<String, PyValue> {
    v.as_object()
        .map(|m| m.iter().map(|(k, x)| (k.clone(), PyValue::from_json(x))).collect())
        .unwrap_or_default()
}

fn op(t: &T, op: &str, a: &Value) -> HubResult<Value> {
    let h = &t.hub;
    let s = |k: &str| a[k].as_str().unwrap_or_default().to_string();
    let pv = |k: &str| PyValue::from_json(&a[k]);
    Ok(match op {
        "put_client" => h.put_client(&s("cid"), &pmap(&a["payload"]), a["create"].as_bool().unwrap_or(false))?,
        "client_view" => h.client_view(&s("cid"))?,
        "list_clients" => json!(h.list_clients()?),
        "delete_client" => {
            h.delete_client(&s("cid"))?;
            Value::Null
        }
        "client_status" => h.client_status(&s("cid")),
        "work_put" => {
            h.work.put(&s("kind"), &s("name"), &pv("payload"), None)?;
            Value::Null
        }
        "work_put_collection" => tv(&h.work.put_collection(&s("cid"), &pv("payload"))?),
        "work_put_profile" => tv(&h.work.put_profile(&s("cid"), &pv("payload"))?),
        "list_items" => {
            let f = &a["filters"];
            let os = |k: &str| f[k].as_str().map(str::to_string);
            h.list_items(
                &s("kind"),
                &ListFilter {
                    q: os("q"),
                    group: os("group"),
                    tag: os("tag"),
                    client: os("client"),
                    enabled: f["enabled"].as_bool(),
                    source: os("source"),
                    issues: f["issues"].as_bool(),
                    sort: os("sort").unwrap_or_else(|| "name".into()),
                    limit: f["limit"].as_i64().unwrap_or(100),
                    cursor: os("cursor"),
                },
            )?
        }
        "item_detail" => h.item_detail(&s("kind"), &s("name"))?,
        "validate_content" => h.validate_content()?,
        "matrix" => h.organizer.matrix()?,
        "toggle" => h.organizer.toggle(
            &s("client"),
            &s("kind"),
            &s("name"),
            a["enabled"].as_bool().unwrap_or(false),
        )?,
        "bulk_toggle" => {
            let clients: Vec<String> = serde_json::from_value(a["clients"].clone()).unwrap_or_default();
            let items: Vec<Member> = serde_json::from_value(a["items"].clone()).unwrap_or_default();
            json!(
                h.organizer
                    .bulk_toggle(&clients, &items, a["enabled"].as_bool().unwrap_or(false))?
            )
        }
        "effective" => h.effective(&s("cid"), &s("source"))?,
        "commit" => {
            h.commit(&s("message"), "t")?;
            Value::Null
        }
        "rendered" => h.rendered(&s("cid"), &s("kind"), &s("name"), &s("source"))?,
        "discover" => h.importer.discover(&s("cid"))?,
        "import_items" => {
            let sel: Option<Vec<(String, String)>> = a
                .get("selection")
                .filter(|v| !v.is_null())
                .map(|v| serde_json::from_value(v.clone()).unwrap_or_default());
            let ov: IndexMap<(String, String), String> = a["overrides"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|x| {
                    let g = |i: usize| x[i].as_str().unwrap_or_default().to_string();
                    ((g(0), g(1)), g(2))
                })
                .collect();
            h.import_items(
                &s("client"),
                sel.as_deref(),
                a["on_conflict"].as_str(),
                a["enable"].as_bool().unwrap_or(true),
                &ov,
                a["override_reason"].as_str(),
                "t",
            )?
        }
        "validate_spec" => h.validate_spec(&pmap(&a["body"]))?,
        "inbox_add" => h.inbox.add(&s("client"), &s("title"), &s("body"), &s("mtype"))?,
        "inbox_list" => json!(h.inbox.list(a["client"].as_str())?),
        "inbox_promote" => {
            let edits = a.get("edits").filter(|e| !e.is_null()).map(PyValue::from_json);
            h.inbox.promote(&s("iid"), edits.as_ref())?
        }
        "inbox_reject" => h.inbox.reject(&s("iid"))?,
        "get_profile" => tv(&h.work.get_profile(&s("cid"))?),
        "get_client" => h.work.get_client(&s("cid"))?.dump(true),
        "names" => json!(h.work.names(&s("kind"))?),
        other => return Err(Problem::bad_request(format!("unknown op {other}"))),
    })
}

fn err_out(p: &Problem) -> Value {
    json!({"error": py_class(p), "status": p.status, "code": p.code, "detail": p.detail,
           "extra": Value::Object((*p.extra).clone())})
}

fn norm_s(s: &str, base: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = s.replace(base, "<base>");
    for (k, val) in vars {
        out = out.replace(val.as_str(), &format!("${{{k}}}"));
    }
    out
}

fn norm(v: &Value, base: &str, vars: &BTreeMap<String, String>) -> Value {
    match v {
        Value::String(s) => Value::String(norm_s(s, base, vars)),
        Value::Array(a) => Value::Array(a.iter().map(|x| norm(x, base, vars)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| {
                    let timed = TIME_KEYS.contains(&k.as_str()) && x.as_f64().is_some_and(|f| f != 0.0);
                    let x = if timed {
                        json!("<time>")
                    } else if k == "scan_digest" && x.is_string() {
                        json!("<digest>") // HMAC under each hub's random attest.key
                    } else {
                        norm(x, base, vars)
                    };
                    (norm_s(k, base, vars), x)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn fill(v: &Value, base: &str, vars: &BTreeMap<String, String>) -> Value {
    match v {
        Value::String(s) => {
            let mut out = s.replace("<base>", base);
            for (k, val) in vars {
                out = out.replace(&format!("${{{k}}}"), val);
            }
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| fill(x, base, vars)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| {
                    (
                        fill(&json!(k), base, vars).as_str().unwrap_or_default().to_string(),
                        fill(x, base, vars),
                    )
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn first_diff(a: &Value, b: &Value, path: &str) -> Option<String> {
    if a == b {
        return None;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys()) {
                if let Some(d) = first_diff(
                    x.get(k).unwrap_or(&Value::Null),
                    y.get(k).unwrap_or(&Value::Null),
                    &format!("{path}.{k}"),
                ) {
                    return Some(d);
                }
            }
            Some(format!("{path}: key sets differ"))
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x
            .iter()
            .zip(y)
            .enumerate()
            .find_map(|(i, (u, v))| first_diff(u, v, &format!("{path}[{i}]"))),
        _ => Some(format!("{path}: {a} != {b}")),
    }
}

#[test]
fn b1_hub_golden() {
    let doc: Value = serde_json::from_str(GOLDEN).unwrap();
    let mut diffs = Vec::new();
    for sc in doc["scenarios"].as_array().unwrap() {
        let name = sc["name"].as_str().unwrap_or_default();
        let t = hub().unwrap();
        native_home(&t);
        let base = t.root.canonicalize().unwrap().to_string_lossy().into_owned();
        let mut vars: BTreeMap<String, String> = BTreeMap::new();
        for (i, st) in sc["steps"].as_array().unwrap().iter().enumerate() {
            let args = fill(&st["args"], &base, &vars);
            let o = st["op"].as_str().unwrap_or_default();
            let got = match op(&t, o, &args) {
                Ok(v) => {
                    if let (Some(var), Some(id)) = (st["save"].as_str(), v["id"].as_str()) {
                        vars.insert(var.to_string(), id.to_string());
                    }
                    json!({"ok": v})
                }
                Err(p) => err_out(&p),
            };
            let got = norm(&got, &base, &vars);
            if let Some(d) = first_diff(&st["expect"], &got, "") {
                diffs.push(format!(
                    "{name}#{i} {o} {}\n   {}",
                    hub_core::py::text::take_chars(&args.to_string(), 140),
                    hub_core::py::text::take_chars(&d, 700)
                ));
            }
        }
        t.hub.close();
    }
    assert!(diffs.is_empty(), "{} differences:\n{}", diffs.len(), diffs.join("\n"));
}
