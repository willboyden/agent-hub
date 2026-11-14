//! hub-core's own checks for the browser UI (frontend/js/hubwasm.js loads the module `make web` builds from this crate).
//! No logic of its own: each export calls the function the server calls and returns JSON text. Every export returns a
//! string and never throws; every input is capped at [`MAX_INPUT`] bytes here as well. The server stays authoritative:
//! it checks again on every write.
//!
//! Export contract (docs/adr/0013-shared-rust-checks-in-the-browser.md):
//! * [`version`]: the workspace version the module was built from (shown by the mock's `/health` and checked by the
//!   node tests; the hub's own `/health` reports a fixed parity version, so the page does not compare them: a stale
//!   glue file is caught by its missing exports instead, and every static file is served `no-cache`).
//! * [`check_config`]: for `hub-env` and `knowledge`, exactly the body `POST /config-files/{file}/validate` answers
//!   (a problem+json body for a refused draft, as the route sends). For `client-spec`, the local half (YAML, client
//!   model, id) plus `"server_check": true` when the server still has checks to add (adapter, roots, dry run).
//! * [`yaml_load`]: PyYAML's safe loader (anchors and merge keys expanded, YAML 1.1 types) with the strict loader's
//!   size, depth and node limits: `{"ok": true, "value": ...}` or `{"ok": false, "message": ..., "line": n|null}`.
//! * [`yaml_scalar_type`]: how PyYAML types a plain scalar: `null|bool|int|float|timestamp|str` (also `merge` for
//!   `<<` and `value` for `=`; `error` for an input over the cap).
//! * [`validate_meta`]: the item model's errors as `[{"loc", "msg", "type"}]` (FastAPI's shape; `[]` = valid).

use hub_core::config_check::{self, ClientLocal};
use hub_core::pyd::Loc;
use hub_core::value::PyValue;
use hub_core::{models, yaml, yamlsafe};
use serde_json::{Value, json};
use wasm_bindgen::prelude::wasm_bindgen;

/// Largest input any export accepts (the config editor's draft cap).
pub const MAX_INPUT: usize = config_check::MAX_TEXT;

/// The workspace version the module was built from (see the module documentation: the page does not compare it with
/// the hub's `/health`).
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// The config editor's check of a draft, run in the browser. See the module documentation.
#[wasm_bindgen]
pub fn check_config(file: &str, text: &str) -> String {
    let out = match file {
        "client-spec" => match config_check::check_client_local(text) {
            Ok(ClientLocal::Done(v)) => with(v, "server_check", json!(false)),
            Ok(ClientLocal::Checked { diags, id, doc }) => {
                let summary = json!({
                    "id": id.as_deref().map(hub_core::common::scrub),
                    "adapter": hub_core::common::scrub(&doc.adapter),
                });
                with(
                    config_check::result("client-spec", diags, &summary),
                    "server_check",
                    json!(true),
                )
            }
            Err(p) => p.body(),
        },
        _ => config_check::check(file, text).unwrap_or_else(|p| p.body()),
    };
    out.to_string()
}

fn with(mut v: Value, key: &str, val: Value) -> Value {
    if let Value::Object(m) = &mut v {
        m.insert(key.to_string(), val);
    }
    v
}

fn load_error(message: &str, line: Option<usize>) -> String {
    json!({"ok": false, "message": message, "line": line}).to_string()
}

/// A YAML document loaded the way PyYAML's `safe_load` loads it. See the module documentation.
#[wasm_bindgen]
pub fn yaml_load(text: &str) -> String {
    if text.len() > MAX_INPUT {
        return load_error("document too large", None);
    }
    match yaml::load(text, yaml::Loader::Safe) {
        Err(e) => load_error(&e.problem, e.problem_mark.as_ref().map(|m| m.line + 1)),
        Ok(v) => match yamlsafe::check_limits(&v) {
            Err(e) => load_error(&e.0, None),
            Ok(()) => json!({"ok": true, "value": v.to_json()}).to_string(),
        },
    }
}

/// How PyYAML types a plain (unquoted) scalar. See the module documentation.
#[wasm_bindgen]
pub fn yaml_scalar_type(s: &str) -> String {
    if s.len() > MAX_INPUT {
        return "error".into();
    }
    let tag = yaml::plain_scalar_tag(s);
    tag.strip_prefix("tag:yaml.org,2002:").unwrap_or("str").to_string()
}

fn model_of(kind: &str) -> Option<&'static hub_core::pyd::Model> {
    match kind {
        "skill" => Some(models::skill_meta()),
        "agent" => Some(models::agent_meta()),
        "instruction" => Some(models::instruction_meta()),
        "memory" => Some(models::memory_meta()),
        "mcp" => Some(models::mcp_doc()),
        "rule" => Some(models::rule_doc()),
        _ => None,
    }
}

fn one_error(kind: &str, msg: &str) -> String {
    json!([{"loc": [], "msg": msg, "type": kind}]).to_string()
}

/// An item's fields checked by its content model (what the server checks when the item is saved). See the module
/// documentation.
#[wasm_bindgen]
pub fn validate_meta(kind: &str, json_text: &str) -> String {
    let Some(model) = model_of(kind) else {
        return one_error("unknown_kind", "no model for this kind");
    };
    if json_text.len() > MAX_INPUT {
        return one_error("too_large", "the input is larger than 64 KiB");
    }
    let Ok(v) = serde_json::from_str::<Value>(json_text) else {
        return one_error("json_invalid", "the input is not JSON");
    };
    match hub_core::pyd::validate(model, &PyValue::from_json(&v)) {
        Ok(_) => "[]".into(),
        Err(e) => e.errors_json(&[] as &[Loc]).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap_or(Value::Null)
    }

    fn app_root() -> std::path::PathBuf {
        // crates/hub-wasm -> backend -> the checkout
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
    }

    /// The browser gets byte for byte what the server's route answers: the same function, the same JSON.
    #[test]
    fn check_config_is_the_route_body() {
        for (file, rel) in [
            ("hub-env", "deploy/agent-hub.env.example"),
            ("knowledge", "deploy/knowledge/config.example.yaml"),
            ("knowledge", "deploy/knowledge/container-config.yaml"),
        ] {
            let text = std::fs::read_to_string(app_root().join(rel)).unwrap_or_default();
            assert!(!text.is_empty(), "{rel}");
            let route = config_check::check(file, &text).unwrap_or_else(|p| p.body());
            assert_eq!(check_config(file, &text), route.to_string(), "{rel}");
            assert_eq!(parse(&check_config(file, &text))["ok"], true, "{rel}");
        }
        for (file, text) in [
            ("hub-env", "HUB_PORT=x\nHUB_KNOWLEDGE_ADMIN_KEY=abcdefghijklmnop\n"),
            ("knowledge", "port: &p 8795\nother: *p\n"),
            ("knowledge", "port: x\n"),
        ] {
            let route = config_check::check(file, text).unwrap_or_else(|p| p.body());
            assert_eq!(check_config(file, text), route.to_string(), "{text}");
        }
        // refusals are the route's problem bodies
        let big = "#".repeat(MAX_INPUT + 1);
        assert_eq!(parse(&check_config("hub-env", &big))["status"], 413);
        assert_eq!(parse(&check_config("floor", "x: 1"))["code"], "floor_locked");
        assert_eq!(parse(&check_config("nope", "x: 1"))["code"], "unknown_config_file");
        assert_eq!(parse(&check_config("client-spec", &big))["status"], 413);
    }

    #[test]
    fn env_secrets_are_checked_and_never_echoed() {
        let r = parse(&check_config(
            "hub-env",
            "HUB_KNOWLEDGE_ADMIN_KEY=hunter2-secret-value\nHUB_PORT=not-a-port\n",
        ));
        assert_eq!(r["ok"], false, "{r}");
        assert!(!r.to_string().contains("hunter2"), "{r}");
    }

    #[test]
    fn client_spec_local_half() {
        let r = parse(&check_config(
            "client-spec",
            "id: my-client\nadapter: generic\ndisplay_name: Mine\nroots: {project: ~/p}\nspec: {skills: {root: project, dir: skills}}\n",
        ));
        assert_eq!(r["file"], "client-spec", "{r}");
        assert_eq!(r["server_check"], true, "{r}");
        assert_eq!(r["summary"]["adapter"], "generic", "{r}");
        let r = parse(&check_config("client-spec", "id: hub\nadapter: [\n"));
        assert_eq!(r["ok"], false, "{r}");
        assert_eq!(r["server_check"], false, "{r}");
        let r = parse(&check_config("client-spec", "id: Bad Id\nadapter: generic\n"));
        assert_eq!(r["ok"], false, "{r}");
    }

    #[test]
    fn yaml_load_has_pyyaml_types_and_expands_anchors() {
        let r = parse(&yaml_load(
            "base: &b {root: home}\nskills:\n  <<: *b\n  dir: skills\nflag: yes\nnone: null\ntilde: ~\nratio: 1.5\noct: 010\ntext: |\n  two\n  lines\n",
        ));
        assert_eq!(r["ok"], true, "{r}");
        let v = &r["value"];
        assert_eq!(v["skills"]["root"], "home");
        assert_eq!(v["flag"], true);
        assert_eq!(v["none"], Value::Null);
        assert_eq!(v["tilde"], Value::Null);
        assert_eq!(v["ratio"], 1.5);
        assert_eq!(v["oct"], 8);
        assert_eq!(v["text"], "two\nlines\n");
        let r = parse(&yaml_load("a: [1\n"));
        assert_eq!(r["ok"], false);
        assert!(r["line"].as_u64().is_some(), "{r}");
        // an alias bomb stops at the node limit
        let mut bomb = String::from("l0: &l0 [a, a, a, a, a, a, a, a, a, a]\n");
        for i in 1..8 {
            bomb.push_str(&format!(
                "l{i}: &l{i} [{}]\n",
                vec![format!("*l{}", i - 1); 10].join(", ")
            ));
        }
        assert_eq!(parse(&yaml_load(&bomb))["ok"], false);
        assert_eq!(parse(&yaml_load(&"#".repeat(MAX_INPUT + 1)))["ok"], false);
    }

    #[test]
    fn scalar_types_are_pyyamls() {
        for (s, t) in [
            ("yes", "bool"),
            ("Off", "bool"),
            ("~", "null"),
            ("", "null"),
            ("0755", "int"),
            ("1:30", "int"),
            ("1.5", "float"),
            ("1e3", "str"),
            ("1.0e+3", "float"),
            (".inf", "float"),
            ("2026-11-04", "timestamp"),
            ("<<", "merge"),
            ("hello", "str"),
        ] {
            assert_eq!(yaml_scalar_type(s), t, "{s}");
        }
    }

    #[test]
    fn meta_rules_are_the_servers() {
        // the server's skill description rule: 1..1024 characters (no 20-character minimum)
        assert_eq!(
            validate_meta("skill", r#"{"name":"a-skill","description":"fifteen chars!!"}"#),
            "[]"
        );
        let e = parse(&validate_meta("skill", r#"{"name":"Bad Name","description":""}"#));
        let types: Vec<&str> = e
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x["type"].as_str())
            .collect();
        assert_eq!(types, ["value_error", "string_too_short"], "{e}");
        let long = format!(r#"{{"name":"a","description":"{}"}}"#, "x".repeat(1025));
        assert_eq!(parse(&validate_meta("skill", &long))[0]["type"], "string_too_long");
        let e = parse(&validate_meta(
            "agent",
            r#"{"name":"a","description":"d","capabilities":["read","fly"],"model_tier":"huge"}"#,
        ));
        assert_eq!(e.as_array().map(Vec::len), Some(2), "{e}");
        assert_eq!(parse(&validate_meta("nope", "{}"))[0]["type"], "unknown_kind");
        assert_eq!(parse(&validate_meta("skill", "{"))[0]["type"], "json_invalid");
        assert!(version().starts_with("0."));
    }
}
