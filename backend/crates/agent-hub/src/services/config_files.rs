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

use std::path::Path;

use hub_core::PyValue;
use hub_core::common::scrub;
use hub_core::knowledge_config;
use hub_core::pyd::{Loc, ValidationError, loc_str};
use hub_core::{HubResult, Problem, dotenv, yaml};
use serde_json::{Value, json};

use crate::config::{SettingsError, settings_env_names, settings_from_env_text};

/// Largest draft (and template, and floor file) the editor handles: the content loader's spec cap.
pub const MAX_TEXT: usize = 64 * 1024;

/// Most lines an env-file draft may have (a real one has a few dozen); more is refused before any parsing.
pub const MAX_LINES: usize = 2000;

/// Most findings one check returns; the rest are counted in a closing note.
pub const MAX_DIAGS: usize = 200;

/// Most top-level key names a knowledge check returns in its summary (`key_count` has them all).
const MAX_KEYS: usize = 200;

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

fn unknown_file(file: &str) -> Problem {
    Problem::not_found(format!(
        "no such config file: {}",
        hub_core::py::text::take_chars(file, 40)
    ))
    .code("unknown_config_file")
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

// ---- diagnostics -------------------------------------------------------------------------------------------------

/// One finding. `line` is 1-based; `path` is the key path in the file (YAML) or the variable name (env).
fn diag(severity: &str, message: &str, line: Option<usize>, path: Option<Vec<Value>>) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("severity".into(), json!(severity));
    m.insert("message".into(), json!(scrub(message)));
    if let Some(l) = line {
        m.insert("line".into(), json!(l));
    }
    if let Some(p) = path {
        let p = p
            .into_iter()
            .map(|v| match v {
                Value::String(s) => Value::String(scrub(&s)),
                other => other,
            })
            .collect();
        m.insert("path".into(), Value::Array(p));
    }
    Value::Object(m)
}

/// The findings of one check, capped at [`MAX_DIAGS`]: past the cap a finding is only counted (its text is not even
/// built, see [`Diags::push_with`]), and whether any was an error still decides `ok`.
#[derive(Default)]
struct Diags {
    shown: Vec<Value>,
    more: usize,
    error: bool,
}

impl Diags {
    fn push(&mut self, d: Value) {
        let sev = d["severity"].as_str().unwrap_or("").to_string();
        self.push_with(&sev, || d);
    }

    /// Count a finding of `severity`; build it (`f`) only while under the cap.
    fn push_with(&mut self, severity: &str, f: impl FnOnce() -> Value) {
        if severity == "error" {
            self.error = true;
        }
        if self.shown.len() < MAX_DIAGS {
            self.shown.push(f());
        } else {
            self.more += 1;
        }
    }
}

fn result(file: &str, diags: Diags, summary: &Value) -> Value {
    let ok = !diags.error;
    let mut shown = diags.shown;
    if diags.more > 0 {
        shown.push(diag(
            "info",
            &format!("{} more findings are not shown; fix these and check again", diags.more),
            None,
            None,
        ));
    }
    json!({"file": file, "ok": ok, "diagnostics": shown, "summary": summary})
}

/// One finding as the only result.
fn single(file: &str, d: Value, summary: &Value) -> Value {
    let mut ds = Diags::default();
    ds.push(d);
    result(file, ds, summary)
}

fn loc_json(loc: &[Loc]) -> Vec<Value> {
    loc.iter()
        .map(|l| match l {
            Loc::Key(k) => json!(k),
            Loc::Index(i) => json!(i),
            Loc::Int(i) => json!(i),
        })
        .collect()
}

fn too_large(text: &str) -> HubResult<()> {
    if text.len() > MAX_TEXT {
        return Err(Problem::new(
            413,
            "payload_too_large",
            "the draft is larger than the editor handles (64 KiB)",
        ));
    }
    Ok(())
}

/// The 1-based line of a top-level YAML key (`key:` at column 0), for errors the validator reports by key.
fn yaml_key_line(text: &str, key: &str) -> Option<usize> {
    text.lines().enumerate().find_map(|(i, l)| {
        let rest = l.strip_prefix(key)?;
        rest.trim_start().starts_with(':').then_some(i + 1)
    })
}

/// A Python exception that escapes the YAML loader, in fixed words: its own text can quote the value (a `KeyError`
/// for `!!bool maybe` is `'maybe'`).
fn python_exception_text(exc_type: &str) -> String {
    match exc_type {
        "RecursionError" => "the document is nested too deeply".into(),
        "MemoryError" => "the document expands to too many values".into(),
        t => format!("a value cannot be converted to the type its tag or form asks for ({t})"),
    }
}

/// A YAML syntax error as a diagnostic: PyYAML's problem line and its mark, never the full message (which quotes
/// the offending source line); an escaping Python exception in fixed words.
fn yaml_error_diag(e: &yaml::YamlError) -> Value {
    let line = e.problem_mark.as_ref().map(|m| m.line + 1);
    let text = e
        .python_exception()
        .map_or_else(|| e.problem.clone(), python_exception_text);
    diag("error", &format!("YAML: {text}"), line, None)
}

// ---- hub env file ------------------------------------------------------------------------------------------------

/// `dotenv::parse` output: (key, value) in file order.
type Bindings = Vec<(String, Option<String>)>;
/// Where a setting is bound: (line, the name as written).
type LineOf<'a> = &'a dyn Fn(&str) -> Option<(Option<usize>, String)>;

/// Names that are read only from the process environment (setting them in the env file has no effect).
const PROCESS_ONLY: [&str; 4] = [
    "hub_allow_nonloopback",
    "hub_python",
    "agent_hub_app_root",
    "agent_hub_env_file",
];
/// Settings that hold a secret.
const SECRET_FIELDS: [&str; 2] = ["knowledge_admin_key", "knowledge_token"];

/// POST /config-files/hub-env/validate.
pub fn check_hub_env(text: &str) -> HubResult<Value> {
    too_large(text)?;
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() > MAX_LINES {
        return Err(Problem::new(
            413,
            "payload_too_large",
            format!("the draft has more than {MAX_LINES} lines; an env file this long is not checked"),
        )
        .code("too_many_lines"));
    }
    let known = settings_env_names();
    let field_of = |key: &str| {
        let k = key.to_lowercase();
        known.iter().find(|(_, names)| names.contains(&k)).map(|(f, _)| *f)
    };
    // Where each binding is: parse line by line when that gives the same bindings as the whole text (no multi-line
    // quoted values); otherwise lines are unknown and a malformed line cannot be pointed at.
    let whole = dotenv::parse(text);
    let per_line: Vec<(usize, Bindings)> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| (i + 1, dotenv::parse(l)))
        .collect();
    let line_mode = per_line.iter().map(|(_, b)| b.len()).sum::<usize>() == whole.len();
    let mut diags = Diags::default();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut set_fields: Vec<String> = Vec::new();
    let bindings: Vec<(Option<usize>, String, Option<String>)> = if line_mode {
        per_line
            .iter()
            .flat_map(|(n, b)| b.iter().map(move |(k, v)| (Some(*n), k.clone(), v.clone())))
            .collect()
    } else {
        whole.into_iter().map(|(k, v)| (None, k, v)).collect()
    };
    if line_mode {
        for (n, b) in &per_line {
            let raw = lines.get(n - 1).copied().unwrap_or("").trim();
            if b.is_empty() && !raw.is_empty() && !raw.starts_with('#') {
                diags.push(diag(
                    "warn",
                    "this line is not KEY=value, so it is skipped when the file is read",
                    Some(*n),
                    None,
                ));
            }
        }
    }
    for (line, key, value) in &bindings {
        let lower = key.to_lowercase();
        let path = Some(vec![json!(key)]);
        if let Some(first) = seen.get(&lower) {
            diags.push(diag(
                "warn",
                &format!("{key} is set again (first on line {first}); the later value wins"),
                *line,
                path.clone(),
            ));
        } else {
            seen.insert(lower.clone(), line.unwrap_or(0));
        }
        if value.is_none() {
            diags.push(diag(
                "warn",
                &format!("{key} has no `=` and no value, so it is ignored"),
                *line,
                path,
            ));
            continue;
        }
        if value.as_deref().is_some_and(|v| v.contains("${")) {
            diags.push(diag(
                "info",
                &format!(
                    "{key} uses ${{...}}: at start the hub fills it from its own environment; this check left it empty"
                ),
                *line,
                path.clone(),
            ));
        }
        match field_of(key) {
            Some(f) => {
                if !set_fields.iter().any(|x| x == f) {
                    set_fields.push(f.to_string());
                }
                if SECRET_FIELDS.contains(&f) {
                    diags.push(diag(
                        "info",
                        &format!("{key} is a secret: keep this file mode 0600 and out of any repository"),
                        *line,
                        path,
                    ));
                }
            }
            None if lower == "hub_api_key" => diags.push(diag(
                "info",
                "HUB_API_KEY is read by hubctl, not by the server; a key in a file can be read by every process of \
                 your user (docs/OPERATIONS.md, Key handling)",
                *line,
                path,
            )),
            None if PROCESS_ONLY.contains(&lower.as_str()) => diags.push(diag(
                "warn",
                &format!("{key} is read only from the process environment; in this file it has no effect"),
                *line,
                path,
            )),
            None if lower.starts_with("hub_") => diags.push(diag(
                "warn",
                &format!("{key} is not a setting the hub reads, so it is ignored (check the spelling)"),
                *line,
                path,
            )),
            None => diags.push(diag("info", &format!("{key} is not read by the hub"), *line, path)),
        }
    }
    let line_of = |field: &str| {
        let names = known
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, n)| n.clone())
            .unwrap_or_default();
        bindings
            .iter()
            .rev()
            .find(|(_, k, v)| v.is_some() && names.contains(&k.to_lowercase()))
            .map(|(l, k, _)| (*l, k.clone()))
    };
    match settings_from_env_text(text) {
        Ok(s) => {
            if !crate::config::is_loopback_host(&s.host) {
                let (l, k) = line_of("host").unwrap_or((None, "HUB_HOST".into()));
                diags.push(diag(
                    "warn",
                    "the hub refuses to start on a non-loopback address unless HUB_ALLOW_NONLOOPBACK=1 is set in its \
                     process environment; put an authenticating reverse proxy in front if you do",
                    l,
                    Some(vec![json!(k)]),
                ));
            }
            if s.trust_loopback {
                let (l, k) = line_of("trust_loopback").unwrap_or((None, "HUB_TRUST_LOOPBACK".into()));
                diags.push(diag(
                    "warn",
                    "with HUB_TRUST_LOOPBACK on, every local process is admin without a key (docs/SECURITY.md \
                     section 4)",
                    l,
                    Some(vec![json!(k)]),
                ));
            }
        }
        Err(SettingsError::Invalid(e)) => {
            for d in settings_errors(&e, &line_of) {
                diags.push(d);
            }
        }
        Err(SettingsError::Expansion(_)) => diags.push(diag(
            "error",
            &format!(
                "the ${{...}} references in this draft expand to more than {} KiB; the check stops there",
                crate::config::MAX_DRAFT_EXPANSION / 1024
            ),
            None,
            None,
        )),
        Err(SettingsError::Parse(field, _)) => {
            let (l, k) = line_of(&field).unwrap_or((None, format!("HUB_{}", field.to_uppercase())));
            diags.push(diag(
                "error",
                &format!("{k} must be a JSON list, for example [\"hub.example:8792\"]"),
                l,
                Some(vec![json!(k)]),
            ));
        }
    }
    Ok(result("hub-env", diags, &json!({"settings": set_fields})))
}

fn settings_errors(e: &ValidationError, line_of: LineOf<'_>) -> Vec<Value> {
    e.errors
        .iter()
        .map(|it| {
            let field = match it.loc.first() {
                Some(Loc::Key(k)) => k.clone(),
                _ => String::new(),
            };
            let (l, k) = line_of(&field).unwrap_or((None, format!("HUB_{}", field.to_uppercase())));
            diag("error", &format!("{k}: {}", it.msg), l, Some(vec![json!(k)]))
        })
        .collect()
}

// ---- knowledge service config ------------------------------------------------------------------------------------

/// POST /config-files/knowledge/validate.
pub fn check_knowledge(text: &str) -> HubResult<Value> {
    too_large(text)?;
    // The service reads its file with PyYAML's safe loader, which expands anchors, aliases and merge keys. Expanding
    // them here would let a small draft grow without bound, so a draft that uses them is not checked at all: the
    // strict loader (no anchors or aliases, size, depth and node caps) looks first, and the safe loader runs only when
    // the strict one proved there is no anchor or alias. It proves it by succeeding, or by failing after composing the
    // whole document (a constructor error, a duplicate key, the depth and node caps), or on a syntax error, which the
    // safe loader meets at the same event, before it constructs anything. The one failure that stops the strict
    // composer earlier than the safe one is its lower nesting limit (328 against 492 levels): such a draft is not
    // checked either. (The safe loader also caps what merge keys may copy, for the service's own file.)
    match hub_core::yamlsafe::load_exact(text, MAX_TEXT) {
        Err(hub_core::yamlsafe::LoadError::Yaml(e))
            if e.0 == "YAML anchors are not allowed" || e.0 == "YAML aliases are not allowed" =>
        {
            return Ok(not_checked(
                "this draft uses YAML anchors or aliases: this checker does not expand anchors, aliases or merge keys \
                 (the service would), so nothing in it was checked; write the values out in full to check it",
            ));
        }
        Err(hub_core::yamlsafe::LoadError::Python {
            exc_type: "RecursionError",
            ..
        }) => {
            return Ok(not_checked(
                "this draft is nested too deeply to check (more than about 320 levels), so nothing in it was checked",
            ));
        }
        _ => {}
    }
    let doc = match yaml::load(text, yaml::Loader::Safe) {
        Ok(d) => d,
        Err(e) => return Ok(single("knowledge", yaml_error_diag(&e), &json!({}))),
    };
    let raw = match doc {
        PyValue::None => PyValue::Dict(Vec::new()),
        d @ PyValue::Dict(_) => d,
        _ => {
            return Ok(single(
                "knowledge",
                diag("error", "config must be a mapping", None, None),
                &json!({}),
            ));
        }
    };
    let mut diags = Diags::default();
    match knowledge_config::validate(&raw, "~") {
        Err(e) => {
            for it in &e.errors {
                diags.push_with("error", || validation_diag(text, it));
            }
        }
        Ok(cfg) => {
            if !["127.0.0.1", "::1", "localhost"].contains(&cfg.host.as_str()) {
                diags.push(diag(
                    "warn",
                    "the service refuses a non-loopback host unless it is started with --allow-bind-any (only inside \
                     a container on an internal network)",
                    yaml_key_line(text, "host"),
                    Some(vec![json!("host")]),
                ));
            }
            if raw
                .as_dict()
                .is_some_and(|d| !d.iter().any(|(k, _)| k.as_str() == Some("router_url")))
            {
                diags.push(diag(
                    "info",
                    "router_url is not set: the service uses KNOWLEDGE_ROUTER_URL from its environment, else \
                     http://127.0.0.1:4000",
                    None,
                    None,
                ));
            }
        }
    }
    let (keys, key_count) = raw.as_dict().map_or((Vec::new(), 0), |d| {
        (
            d.iter().take(MAX_KEYS).map(|(k, _)| scrub(&k.py_str())).collect(),
            d.len(),
        )
    });
    Ok(result(
        "knowledge",
        diags,
        &json!({"keys": keys, "key_count": key_count}),
    ))
}

/// A knowledge draft that was not checked: one warning, `checked: false`, and `ok: false` (a check that looked at
/// nothing is not a pass).
fn not_checked(message: &str) -> Value {
    let mut r = single(
        "knowledge",
        diag("warn", message, None, None),
        &json!({"checked": false}),
    );
    r["ok"] = json!(false);
    r
}

/// One validation error of a YAML file: the key path and pydantic's reason, at the line of its top-level key.
fn validation_diag(text: &str, it: &hub_core::pyd::ErrItem) -> Value {
    let top = match it.loc.first() {
        Some(Loc::Key(k)) => k.clone(),
        _ => String::new(),
    };
    diag(
        "error",
        &format!("{}: {}", loc_str(&it.loc), it.msg),
        yaml_key_line(text, &top),
        Some(loc_json(&it.loc)),
    )
}

// ---- client spec -------------------------------------------------------------------------------------------------

impl crate::services::hub::Hub {
    /// POST /config-files/client-spec/validate: the content loader's YAML loader and client model, the create-time
    /// id and adapter checks, then (generic adapter) the dry run of `POST /clients/validate-spec`. Writes nothing.
    pub fn check_client_file(&self, text: &str) -> HubResult<Value> {
        too_large(text)?;
        let file = "client-spec";
        let doc = match hub_core::yamlsafe::load_exact(text, MAX_TEXT) {
            Ok(d) => d,
            Err(e) => {
                // the strict loader's message (an escaping Python exception in fixed words), with the line from the
                // YAML parser when it has one
                let msg = match e {
                    hub_core::yamlsafe::LoadError::Yaml(y) => y.0,
                    hub_core::yamlsafe::LoadError::Python { exc_type, .. } => python_exception_text(exc_type),
                };
                let line = yaml::load(text, yaml::Loader::Strict)
                    .err()
                    .and_then(|y| y.problem_mark.map(|m| m.line + 1));
                return Ok(single(file, diag("error", &msg, line, None), &json!({})));
            }
        };
        let Some(map) = doc.as_dict() else {
            return Ok(single(
                file,
                diag("error", "a client file must be a mapping", None, None),
                &json!({}),
            ));
        };
        let get = |k: &str| {
            map.iter()
                .find(|(key, _)| key.as_str() == Some(k))
                .map(|(_, v)| v.clone())
        };
        let mut diags = Diags::default();
        let id = get("id").and_then(|v| v.as_str().map(str::to_string));
        match id.as_deref() {
            None => diags.push(diag(
                "error",
                "id is required: it must equal the file name (clients/<id>.yaml)",
                None,
                Some(vec![json!("id")]),
            )),
            Some("hub") => diags.push(diag(
                "error",
                "the client id `hub` is reserved",
                yaml_key_line(text, "id"),
                Some(vec![json!("id")]),
            )),
            Some(i) if !hub_core::ids::valid_id(i) => diags.push(diag(
                "error",
                &format!("id {}", hub_core::ids::IDENT_MESSAGE),
                yaml_key_line(text, "id"),
                Some(vec![json!("id")]),
            )),
            Some(_) => {}
        }
        let v = match hub_core::pyd::validate(hub_core::models::client_doc(), &doc) {
            Ok(v) => v,
            Err(e) => {
                for it in &e.errors {
                    diags.push_with("error", || validation_diag(text, it));
                }
                return Ok(result(file, diags, &json!({"id": id.as_deref().map(scrub)})));
            }
        };
        let cdoc = hub_core::models::ClientDoc::from_validated(&v);
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
        if cdoc.adapter == "generic" && diags.shown.is_empty() {
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

/// POST /config-files/{file}/validate for the files that need no hub state.
pub fn check(file: &str, text: &str) -> HubResult<Value> {
    match file {
        "hub-env" => check_hub_env(text),
        "knowledge" => check_knowledge(text),
        "floor" => Err(Problem::forbidden(
            "the policy floor is not editable through the hub: change policy/floor.yaml in git, with review",
        )
        .code("floor_locked")),
        _ => Err(unknown_file(file)),
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
