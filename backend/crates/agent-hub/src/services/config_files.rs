//! The Settings page's config editor: draft, validate, export. The hub never writes, installs or chmods any of these
//! files and runs no command for them; the human installs a checked draft by hand (the UI shows the commands as text).
//!
//! * `hub-env`: the hub's own env file (`HUB_*`, `~/.config/agent-hub/env`). It can hold secrets (the knowledge
//!   admin key, `HUB_API_KEY`), so the current file is never served: only the shipped template, or a draft the person
//!   pastes. A draft is checked by [`crate::config::settings_from_env_text`], the server's own loader, with an EMPTY
//!   environment (nothing of the server's environment is merged in or interpolated).
//! * `knowledge`: the knowledge service's YAML config. The hub does not know its path, so only the shipped templates
//!   and pastes; checked by `hub_core::knowledge_config::validate`, the service's own validator.
//! * `client-spec`: a client file (`content/clients/<id>.yaml`), templates = `examples/clients/*.yaml`; checked by the
//!   content loader's YAML loader and model, then (for `adapter: generic`) the same dry run as
//!   `POST /clients/validate-spec`.
//! * `floor`: `policy/floor.yaml`, deliberately not editable through the API (docs/ARCHITECTURE.md): served read-only
//!   so the editor can explain every key; never validated or exported here.
//!
//! Every message (and every key path) that goes back passes through the hub's secret scrubber. The env-file check
//! never quotes a value: it names the key and the line. The YAML checks name keys, and a few name what the draft
//! gives as an adapter, a root or a spec item; loader errors about a value are reported in fixed words. At most
//! [`MAX_DIAGS`] findings are returned.
//!
//! The checks that need no hub state (`hub-env`, `knowledge`, and the YAML/model/id half of `client-spec`) live in
//! `hub_core::config_check`, which the browser also runs (crates/hub-wasm, ADR-0013); this module serves the templates
//! and the floor and adds the client-spec checks that need the hub (adapter registry, roots, the generic dry run).

use std::path::Path;

use hub_core::PyValue;
use hub_core::common::scrub;
use hub_core::{HubResult, Problem};
use serde_json::{Value, json};

/// The checks that need no hub state, their limits and the diagnostic helpers live in `hub_core::config_check` (so
/// the browser runs the same functions); re-exported here under their old paths.
pub use hub_core::config_check::{
    ClientLocal, Diags, MAX_DIAGS, MAX_KEYS, MAX_LINES, MAX_TEXT, check, check_client_local, check_hub_env,
    check_knowledge, diag, result, unknown_file, yaml_key_line,
};

/// The config files the editor knows, in the order the UI lists them.
pub const FILES: [&str; 4] = ["hub-env", "knowledge", "client-spec", "floor"];

/// Shipped templates: (file, template id, path under the app root). Nothing else is ever read for a template.
pub const TEMPLATES: [(&str, &str, &str); 6] = [
    ("hub-env", "default", "deploy/agent-hub.env.example"),
    ("knowledge", "host", "deploy/knowledge/config.example.yaml"),
    ("knowledge", "container", "deploy/knowledge/container-config.yaml"),
    ("client-spec", "cursor", "examples/clients/cursor.yaml"),
    ("client-spec", "codex-cli", "examples/clients/codex-cli.yaml"),
    ("client-spec", "gemini-cli", "examples/clients/gemini-cli.yaml"),
];

fn format_of(file: &str) -> &'static str {
    if file == "hub-env" { "env" } else { "yaml" }
}

/// GET /config-files: what the editor offers, and the paths the install commands need. `env_file` is where this
/// hub reads its env file (`AGENT_HUB_ENV_FILE` or `~/.config/agent-hub/env`).
pub fn catalog(content_dir: &Path, policy_file: &Path, env_file: &Path) -> Value {
    let items: Vec<Value> = FILES
        .iter()
        .map(|f| {
            let templates: Vec<Value> = TEMPLATES
                .iter()
                .filter(|(file, _, _)| file == f)
                .map(|(_, id, path)| json!({"id": id, "path": path}))
                .collect();
            let (path, current, editable) = match *f {
                "hub-env" => (env_file.display().to_string(), false, true),
                "knowledge" => (String::new(), false, true),
                "client-spec" => (content_dir.join("clients").display().to_string(), false, true),
                _ => (policy_file.display().to_string(), true, false),
            };
            json!({"id": f, "format": format_of(f), "editable": editable, "current": current, "path": path,
                   "templates": templates, "max_bytes": MAX_TEXT})
        })
        .collect();
    json!({"items": items})
}

/// A regular file (a symbolic link is refused, not followed), at most [`MAX_TEXT`] bytes, as UTF-8 text.
fn read_capped(path: &Path) -> HubResult<String> {
    let bytes = crate::services::fs::read_regular(path, false, MAX_TEXT as u64).map_err(|e| {
        if e.raw_os_error().is_none() && e.to_string() == "file too large" {
            Problem::unprocessable("the file is larger than the editor handles").code("file_too_large")
        } else {
            Problem::not_found("the file is not there or cannot be read").code("file_missing")
        }
    })?;
    String::from_utf8(bytes).map_err(|_| Problem::unprocessable("the file is not UTF-8 text").code("file_not_text"))
}

fn secret_in_file() -> Problem {
    Problem::unprocessable("the file contains text that looks like a secret, so it is not served")
        .code("secret_in_file")
}

/// GET /config-files/{file}/templates/{name}: a shipped template's text (fixed table, under the app root).
pub fn template_text(app_root: &Path, file: &str, name: &str) -> HubResult<Value> {
    if !FILES.contains(&file) {
        return Err(unknown_file(file));
    }
    let Some((_, id, rel)) = TEMPLATES.iter().find(|(f, id, _)| *f == file && *id == name) else {
        return Err(Problem::not_found("no such template").code("unknown_template"));
    };
    let text = read_capped(&app_root.join(rel))?;
    // a template names env vars (`router_api_key_env: KNOWLEDGE_EMBED_API_KEY`), which the scrubber's `name = value`
    // rule would mask; a token of a secret shape is refused
    if hub_core::common::has_secret_token(&text) {
        return Err(secret_in_file());
    }
    Ok(json!({"file": file, "template": id, "path": rel, "format": format_of(file), "text": text}))
}

/// GET /config-files/{file}/current: only the policy floor, which holds no secret by design. A file in which the
/// hub's secret scrubber would mask anything is refused rather than masked.
pub fn current_text(file: &str, policy_file: &Path) -> HubResult<Value> {
    if !FILES.contains(&file) {
        return Err(unknown_file(file));
    }
    if file != "floor" {
        return Err(Problem::forbidden(
            "this file can hold secrets or lives outside the hub, so the hub never serves it; start from a template or \
             paste your own copy",
        )
        .code("not_served"));
    }
    let text = read_capped(policy_file)?;
    if scrub(&text) != text {
        return Err(secret_in_file());
    }
    Ok(
        json!({"file": file, "format": "yaml", "path": policy_file.display().to_string(), "text": text,
              "editable": false}),
    )
}

// ---- client spec -------------------------------------------------------------------------------------------------

impl crate::services::hub::Hub {
    /// POST /config-files/client-spec/validate: the content loader's YAML loader and client model, the create-time
    /// id and adapter checks, then (generic adapter) the dry run of `POST /clients/validate-spec`. Writes nothing.
    pub fn check_client_file(&self, text: &str) -> HubResult<Value> {
        let file = "client-spec";
        let (mut diags, id, cdoc) = match check_client_local(text)? {
            ClientLocal::Done(r) => return Ok(r),
            ClientLocal::Checked { diags, id, doc } => (diags, id, doc),
        };
        if self.registry.get(&cdoc.adapter).is_none() {
            diags.push(diag(
                "error",
                &format!(
                    "adapter {} is not installed in this hub",
                    hub_core::py::text::repr(&cdoc.adapter)
                ),
                yaml_key_line(text, "adapter"),
                Some(vec![json!("adapter")]),
            ));
        }
        if let Err(e) = crate::services::resolver::client_config(&cdoc, None) {
            diags.push(diag(
                "error",
                &format!("invalid roots: {e}"),
                yaml_key_line(text, "roots"),
                Some(vec![json!("roots")]),
            ));
        }
        let mut artifacts = Vec::new();
        if cdoc.adapter == "generic" && diags.is_empty() {
            let mut body = indexmap::IndexMap::new();
            body.insert("spec".to_string(), cdoc.spec.clone().unwrap_or(PyValue::None));
            body.insert(
                "roots".to_string(),
                PyValue::Dict(
                    cdoc.roots
                        .iter()
                        .map(|(k, r)| (PyValue::str(k.as_str()), PyValue::str(r.as_str())))
                        .collect(),
                ),
            );
            body.insert("strict".to_string(), PyValue::Bool(cdoc.strict));
            match self.validate_spec(&body) {
                Ok(r) => {
                    for d in r["diagnostics"].as_array().into_iter().flatten() {
                        let item = d["item"].as_str().unwrap_or("");
                        let msg = d["message"].as_str().unwrap_or("");
                        let sev = d["severity"].as_str().unwrap_or("info");
                        let text_msg = if item.is_empty() {
                            msg.to_string()
                        } else {
                            format!("{item}: {msg}")
                        };
                        diags.push_with(sev, || diag(sev, &text_msg, yaml_key_line(text, "spec"), None));
                    }
                    for a in r["artifacts"].as_array().into_iter().flatten() {
                        artifacts.push(json!({"root": a["root"], "path": a["path"], "kind": a["kind"]}));
                    }
                }
                Err(p) => {
                    let errs = p
                        .extra
                        .get("errors")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if errs.is_empty() {
                        diags.push_with("error", || diag("error", &p.detail, yaml_key_line(text, "spec"), None));
                    }
                    for e in errs {
                        let msg = e
                            .get("message")
                            .or_else(|| e.get("msg"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let path = e.get("path").and_then(Value::as_str).unwrap_or("");
                        let m = if path.is_empty() {
                            msg.to_string()
                        } else {
                            format!("{path}: {msg}")
                        };
                        diags.push_with("error", || diag("error", &m, yaml_key_line(text, "spec"), None));
                    }
                }
            }
        }
        Ok(result(
            file,
            diags,
            &json!({"id": id.as_deref().map(scrub), "adapter": scrub(&cdoc.adapter), "artifacts": artifacts}),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sev(v: &Value) -> Vec<(String, String)> {
        v["diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|d| {
                (
                    d["severity"].as_str().unwrap_or("").into(),
                    d["message"].as_str().unwrap_or("").into(),
                )
            })
            .collect()
    }

    #[test]
    fn hub_env_errors_name_the_key_and_line_never_the_value() -> HubResult<()> {
        let r = check_hub_env("# x\nHUB_PORT=hunter2-not-a-port\nHUB_ALLOWED_HOSTS=nope\n")?;
        assert_eq!(r["ok"], false);
        let text = r.to_string();
        assert!(!text.contains("hunter2"), "{text}");
        let d = r["diagnostics"].as_array().cloned().unwrap_or_default();
        assert!(
            d.iter()
                .any(|x| x["line"] == 3 && x["message"].as_str().unwrap_or("").contains("JSON list")),
            "{d:?}"
        );
        let r = check_hub_env("HUB_PORT=x\n")?;
        let d = r["diagnostics"].as_array().cloned().unwrap_or_default();
        assert!(
            d.iter().any(|x| x["line"] == 1
                && x["message"]
                    .as_str()
                    .unwrap_or("")
                    .starts_with("HUB_PORT: Input should be a valid integer")),
            "{d:?}"
        );
        Ok(())
    }

    #[test]
    fn hub_env_never_reads_the_server_environment() -> HubResult<()> {
        // HOME and PATH exist in every test process; a draft that interpolates them must not get their values
        let r = check_hub_env("HUB_GIT_AUTHOR_NAME=${HOME}\nHUB_PORT=${PATH}\n")?;
        let text = r.to_string();
        if let Ok(p) = std::env::var("PATH") {
            assert!(!text.contains(&p));
        }
        assert!(sev(&r).iter().any(|(s, m)| s == "info" && m.contains("${...}")));
        Ok(())
    }

    #[test]
    fn hub_env_warnings_and_secrets() -> HubResult<()> {
        let r = check_hub_env(
            "HUB_PORT=9000\nHUB_PROT=1\nHUB_ALLOW_NONLOOPBACK=1\nHUB_KNOWLEDGE_ADMIN_KEY=abcdefghijklmnop\nHUB_PORT=9001\nnot a line\nHUB_HOST=0.0.0.0\n",
        )?;
        assert_eq!(r["ok"], true);
        let s = sev(&r);
        let has = |sv: &str, part: &str| s.iter().any(|(a, b)| a == sv && b.contains(part));
        assert!(has("warn", "HUB_PROT is not a setting"));
        assert!(has(
            "warn",
            "HUB_ALLOW_NONLOOPBACK is read only from the process environment"
        ));
        assert!(has("info", "HUB_KNOWLEDGE_ADMIN_KEY is a secret"));
        assert!(has("warn", "set again (first on line 1)"));
        assert!(has("warn", "not KEY=value"));
        assert!(has("warn", "non-loopback"));
        assert!(!r.to_string().contains("abcdefghijklmnop"));
        Ok(())
    }

    #[test]
    fn knowledge_uses_the_services_validator() -> HubResult<()> {
        let r = check_knowledge("port: x\nrouter_api_key_env: sk-live-1234567890abcdef\nzzz: 1\n")?;
        assert_eq!(r["ok"], false);
        let d = r["diagnostics"].as_array().cloned().unwrap_or_default();
        assert!(d.iter().any(|x| x["line"] == 1 && x["path"] == json!(["port"])));
        assert!(
            d.iter()
                .any(|x| x["line"] == 3 && x["message"].as_str().unwrap_or("").contains("Extra inputs"))
        );
        assert!(!r.to_string().contains("sk-live"), "{r}");
        let r = check_knowledge("a: [1\n")?;
        assert_eq!(r["ok"], false);
        assert!(!r.to_string().contains("a: [1"), "the source line is never quoted: {r}");
        let ok = check_knowledge("port: 8795\nrouter_url: http://127.0.0.1:4000\n")?;
        assert_eq!(ok["ok"], true, "{ok}");
        Ok(())
    }

    #[test]
    fn shipped_templates_load_and_check_clean() -> HubResult<()> {
        let root = crate::config::app_root();
        for (file, id, _) in TEMPLATES {
            let t = template_text(&root, file, id)?;
            let text = t["text"].as_str().unwrap_or_default().to_string();
            assert!(!text.is_empty(), "{file}/{id}");
            if file != "client-spec" {
                let r = check(file, &text)?;
                assert_eq!(r["ok"], true, "{file}/{id}: {r}");
            }
        }
        assert_eq!(
            template_text(&root, "hub-env", "../../etc/passwd")
                .map_err(|p| p.status)
                .err(),
            Some(404)
        );
        assert_eq!(
            template_text(&root, "nope", "default").map_err(|p| p.status).err(),
            Some(404)
        );
        Ok(())
    }

    #[test]
    fn only_the_floor_is_served_and_it_cannot_be_validated() -> HubResult<()> {
        let floor = crate::config::app_root().join("policy/floor.yaml");
        let r = current_text("floor", &floor)?;
        assert!(r["text"].as_str().unwrap_or_default().contains("deny_path_read"));
        for f in ["hub-env", "knowledge", "client-spec"] {
            assert_eq!(
                current_text(f, &floor).map_err(|p| p.code).err().as_deref(),
                Some("not_served")
            );
        }
        assert_eq!(
            check("floor", "version: 1\n").map_err(|p| p.code).err().as_deref(),
            Some("floor_locked")
        );
        let d = tempfile::tempdir().map_err(|_| Problem::internal())?;
        let f = d.path().join("floor.yaml");
        std::fs::write(&f, "version: 1\n# token: ghp_abcdefghijklmnopqrstu\n").map_err(|_| Problem::internal())?;
        assert_eq!(
            current_text("floor", &f).map_err(|p| p.code).err().as_deref(),
            Some("secret_in_file")
        );
        Ok(())
    }

    #[test]
    fn oversized_drafts_are_refused() {
        let big = "#".repeat(MAX_TEXT + 1);
        assert_eq!(check_hub_env(&big).map_err(|p| p.status).err(), Some(413));
        assert_eq!(check_knowledge(&big).map_err(|p| p.status).err(), Some(413));
    }

    #[test]
    fn hub_env_expansion_chain_is_cut_short() -> HubResult<()> {
        // five levels, each value 100 copies of the one before: 10^10 x 100 bytes if expanded
        let mut text = format!("HUB_A0={}\n", "x".repeat(100));
        for lvl in 1..5 {
            text.push_str(&format!(
                "HUB_A{lvl}={}\n",
                format!("${{HUB_A{}}}", lvl - 1).repeat(100)
            ));
        }
        text.push_str("HUB_GIT_AUTHOR_NAME=${HUB_A4}\n");
        let start = std::time::Instant::now();
        let r = check_hub_env(&text)?;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(r["ok"], false);
        assert!(
            sev(&r)
                .iter()
                .any(|(s, m)| s == "error" && m.contains("expand to more than 256 KiB")),
            "{r}"
        );
        Ok(())
    }

    #[test]
    fn hub_env_line_and_finding_caps() -> HubResult<()> {
        let many = "HUB_X\n".repeat(MAX_LINES);
        assert_eq!(
            check_hub_env(&many).map_err(|p| (p.status, p.code)).err(),
            Some((413, "too_many_lines".to_string()))
        );
        let start = std::time::Instant::now();
        let r = check_hub_env(&"not a binding\n".repeat(MAX_LINES - 1))?;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        let d = r["diagnostics"].as_array().cloned().unwrap_or_default();
        assert_eq!(d.len(), MAX_DIAGS + 1);
        assert!(
            d.last()
                .is_some_and(|x| x["message"].as_str().unwrap_or("").contains("more findings"))
        );
        // an error past the cap still makes the draft not ok
        let r = check_hub_env(&format!("{}HUB_PORT=x\n", "not a binding\n".repeat(MAX_DIAGS + 5)))?;
        assert_eq!(r["ok"], false, "{r}");
        Ok(())
    }

    #[test]
    fn knowledge_drafts_with_aliases_are_not_expanded() -> HubResult<()> {
        // a merge-key bomb: each level merges the one before ten times
        let mut text = String::from("l0: &l0 {a: 1, b: 2, c: 3, d: 4}\n");
        for i in 1..9 {
            let refs = vec![format!("*l{}", i - 1); 10].join(", ");
            text.push_str(&format!("l{i}: &l{i}\n  <<: [{refs}]\n"));
        }
        let start = std::time::Instant::now();
        let r = check_knowledge(&text)?;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(r["summary"]["checked"], false, "{r}");
        assert_eq!(r["ok"], false, "a check that looked at nothing is not a pass: {r}");
        assert!(
            sev(&r)
                .iter()
                .any(|(s, m)| s == "warn" && m.contains("does not expand anchors")),
            "{r}"
        );
        Ok(())
    }

    #[test]
    fn knowledge_summary_names_at_most_max_keys() -> HubResult<()> {
        let text: String = (0..MAX_KEYS + 50).map(|i| format!("k{i}: 1\n")).collect();
        let r = check_knowledge(&text)?;
        assert_eq!(r["summary"]["keys"].as_array().map(Vec::len), Some(MAX_KEYS), "{r}");
        assert_eq!(r["summary"]["key_count"], MAX_KEYS + 50, "{r}");
        Ok(())
    }

    #[test]
    fn knowledge_drafts_too_deep_for_the_strict_loader_are_not_expanded() -> HubResult<()> {
        // ~340 nested flow lists stop the strict loader (RecursionError at 328) before it reaches the anchors below;
        // the safe loader would go on (it allows ~492) and expand the merge-key bomb
        let mut text = format!("deep: {}1{}\n", "[".repeat(340), "]".repeat(340));
        text.push_str("l0: &l0 {a: 1, b: 2, c: 3, d: 4}\n");
        for i in 1..9 {
            let refs = vec![format!("*l{}", i - 1); 10].join(", ");
            text.push_str(&format!("l{i}: &l{i}\n  <<: [{refs}]\n"));
        }
        let start = std::time::Instant::now();
        let r = check_knowledge(&text)?;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(r["summary"]["checked"], false, "{r}");
        assert_eq!(r["ok"], false, "{r}");
        assert_eq!(r["diagnostics"].as_array().map(Vec::len), Some(1), "{r}");
        assert!(
            sev(&r).iter().any(|(_, m)| m.contains("nested too deeply to check")),
            "{r}"
        );
        Ok(())
    }

    #[test]
    fn loader_errors_and_key_paths_never_quote_the_draft() -> HubResult<()> {
        let r = check_knowledge("port: !!bool maybe-secret-value\n")?;
        assert_eq!(r["ok"], false, "{r}");
        assert!(!r.to_string().contains("maybe-secret-value"), "{r}");
        let r = check_knowledge("ghp_abcdefghijklmnopqrstu: 1\n")?;
        assert_eq!(r["ok"], false, "{r}");
        assert!(!r.to_string().contains("ghp_abcdefghijklmnopqrstu"), "{r}");
        Ok(())
    }

    #[test]
    fn a_symlinked_or_secret_bearing_file_is_not_read() -> HubResult<()> {
        let d = tempfile::tempdir().map_err(|_| Problem::internal())?;
        let real = d.path().join("real.yaml");
        std::fs::write(&real, "version: 1\n").map_err(|_| Problem::internal())?;
        let link = d.path().join("link.yaml");
        std::os::unix::fs::symlink(&real, &link).map_err(|_| Problem::internal())?;
        assert_eq!(
            read_capped(&link).map_err(|p| p.code).err().as_deref(),
            Some("file_missing")
        );
        assert!(read_capped(&real).is_ok());
        std::fs::write(&real, "x".repeat(MAX_TEXT + 1)).map_err(|_| Problem::internal())?;
        assert_eq!(
            read_capped(&real).map_err(|p| p.code).err().as_deref(),
            Some("file_too_large")
        );
        Ok(())
    }
}
