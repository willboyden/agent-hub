//! The config editor's checks that need no hub state (Settings > Config files): the hub env file (`hub-env`, checked
//! by [`crate::settings`], the server's own loader, with an EMPTY environment), the knowledge service's config
//! (`knowledge`, checked by [`crate::knowledge_config`], the service's own validator), and the local half of a client
//! file (`client-spec`: the content loader's YAML loader, the client model and the id checks; the adapter, roots and
//! the generic dry run need the hub and stay in `agent_hub::services::config_files`). Moved verbatim from that module
//! (which re-exports everything here) so the browser runs the same functions (crates/hub-wasm): the JSON is exactly
//! the body of `POST /config-files/{file}/validate`.
//!
//! Every message (and every key path) that goes back passes through the hub's secret scrubber. The env-file check
//! never quotes a value: it names the key and the line. The YAML checks name keys; loader errors about a value are
//! reported in fixed words. At most [`MAX_DIAGS`] findings are returned. Owner: CORE (LOCKED).

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::PyValue;
use crate::common::scrub;
use crate::knowledge_config;
use crate::pyd::{Loc, ValidationError, loc_str};
use crate::settings::{self, SettingsError};
use crate::{HubResult, Problem, dotenv, yaml};

/// Largest draft (and template, and floor file) the editor handles: the content loader's spec cap.
pub const MAX_TEXT: usize = 64 * 1024;

/// Most lines an env-file draft may have (a real one has a few dozen); more is refused before any parsing.
pub const MAX_LINES: usize = 2000;

/// Most findings one check returns; the rest are counted in a closing note.
pub const MAX_DIAGS: usize = 200;

/// Most top-level key names a knowledge check returns in its summary (`key_count` has them all).
pub const MAX_KEYS: usize = 200;

/// A file name the editor does not know (404 `unknown_config_file`; the name is cut to 40 characters).
pub fn unknown_file(file: &str) -> Problem {
    Problem::not_found(format!(
        "no such config file: {}",
        crate::py::text::take_chars(file, 40)
    ))
    .code("unknown_config_file")
}

// ---- diagnostics -------------------------------------------------------------------------------------------------

/// One finding. `line` is 1-based; `path` is the key path in the file (YAML) or the variable name (env).
pub fn diag(severity: &str, message: &str, line: Option<usize>, path: Option<Vec<Value>>) -> Value {
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
pub struct Diags {
    shown: Vec<Value>,
    more: usize,
    error: bool,
}

impl Diags {
    pub fn push(&mut self, d: Value) {
        let sev = d["severity"].as_str().unwrap_or("").to_string();
        self.push_with(&sev, || d);
    }

    /// True when no finding was recorded yet (shown or counted).
    pub fn is_empty(&self) -> bool {
        self.shown.is_empty() && self.more == 0
    }

    /// Count a finding of `severity`; build it (`f`) only while under the cap.
    pub fn push_with(&mut self, severity: &str, f: impl FnOnce() -> Value) {
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

pub fn result(file: &str, diags: Diags, summary: &Value) -> Value {
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
pub fn single(file: &str, d: Value, summary: &Value) -> Value {
    let mut ds = Diags::default();
    ds.push(d);
    result(file, ds, summary)
}

pub fn loc_json(loc: &[Loc]) -> Vec<Value> {
    loc.iter()
        .map(|l| match l {
            Loc::Key(k) => json!(k),
            Loc::Index(i) => json!(i),
            Loc::Int(i) => json!(i),
        })
        .collect()
}

pub fn too_large(text: &str) -> HubResult<()> {
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
pub fn yaml_key_line(text: &str, key: &str) -> Option<usize> {
    text.lines().enumerate().find_map(|(i, l)| {
        let rest = l.strip_prefix(key)?;
        rest.trim_start().starts_with(':').then_some(i + 1)
    })
}

/// A Python exception that escapes the YAML loader, in fixed words: its own text can quote the value (a `KeyError`
/// for `!!bool maybe` is `'maybe'`).
pub fn python_exception_text(exc_type: &str) -> String {
    match exc_type {
        "RecursionError" => "the document is nested too deeply".into(),
        "MemoryError" => "the document expands to too many values".into(),
        t => format!("a value cannot be converted to the type its tag or form asks for ({t})"),
    }
}

/// A YAML syntax error as a diagnostic: PyYAML's problem line and its mark, never the full message (which quotes
/// the offending source line); an escaping Python exception in fixed words.
pub fn yaml_error_diag(e: &yaml::YamlError) -> Value {
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
    let known = settings::settings_env_names();
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
    // the default policy file is never looked at by this check, so no app root is needed
    match settings::settings_from_env_text(text, &PathBuf::new) {
        Ok(s) => {
            if !settings::is_loopback_host(&s.host) {
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
                settings::MAX_DRAFT_EXPANSION / 1024
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
    match crate::yamlsafe::load_exact(text, MAX_TEXT) {
        Err(crate::yamlsafe::LoadError::Yaml(e))
            if e.0 == "YAML anchors are not allowed" || e.0 == "YAML aliases are not allowed" =>
        {
            return Ok(not_checked(
                "this draft uses YAML anchors or aliases: this checker does not expand anchors, aliases or merge keys \
                 (the service would), so nothing in it was checked; write the values out in full to check it",
            ));
        }
        Err(crate::yamlsafe::LoadError::Python {
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
pub fn validation_diag(text: &str, it: &crate::pyd::ErrItem) -> Value {
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

// ---- client spec: the half that needs no hub -----------------------------------------------------------------------

/// The local half of a client-file check.
pub enum ClientLocal {
    /// The check ends here (the YAML does not load, the file is not a mapping, or the client model refuses it): this
    /// is the whole result.
    Done(Value),
    /// The file loads and fits the client model. `diags` holds the id findings so far; the caller goes on with the
    /// checks that need a hub (the adapter, the roots, the generic dry run) and builds the result with [`result`].
    Checked {
        diags: Diags,
        id: Option<String>,
        doc: Box<crate::models::ClientDoc>,
    },
}

/// The first part of `POST /config-files/client-spec/validate`: the content loader's YAML loader and client model and
/// the create-time id checks. Writes nothing, reads nothing.
pub fn check_client_local(text: &str) -> HubResult<ClientLocal> {
    too_large(text)?;
    let file = "client-spec";
    let doc = match crate::yamlsafe::load_exact(text, MAX_TEXT) {
        Ok(d) => d,
        Err(e) => {
            // the strict loader's message (an escaping Python exception in fixed words), with the line from the
            // YAML parser when it has one
            let msg = match e {
                crate::yamlsafe::LoadError::Yaml(y) => y.0,
                crate::yamlsafe::LoadError::Python { exc_type, .. } => python_exception_text(exc_type),
            };
            let line = yaml::load(text, yaml::Loader::Strict)
                .err()
                .and_then(|y| y.problem_mark.map(|m| m.line + 1));
            return Ok(ClientLocal::Done(single(
                file,
                diag("error", &msg, line, None),
                &json!({}),
            )));
        }
    };
    let Some(map) = doc.as_dict() else {
        return Ok(ClientLocal::Done(single(
            file,
            diag("error", "a client file must be a mapping", None, None),
            &json!({}),
        )));
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
        Some(i) if !crate::ids::valid_id(i) => diags.push(diag(
            "error",
            &format!("id {}", crate::ids::IDENT_MESSAGE),
            yaml_key_line(text, "id"),
            Some(vec![json!("id")]),
        )),
        Some(_) => {}
    }
    let v = match crate::pyd::validate(crate::models::client_doc(), &doc) {
        Ok(v) => v,
        Err(e) => {
            for it in &e.errors {
                diags.push_with("error", || validation_diag(text, it));
            }
            return Ok(ClientLocal::Done(result(
                file,
                diags,
                &json!({"id": id.as_deref().map(scrub)}),
            )));
        }
    };
    let doc = crate::models::ClientDoc::from_validated(&v);
    Ok(ClientLocal::Checked {
        diags,
        id,
        doc: Box::new(doc),
    })
}

/// POST /config-files/{file}/validate for the files that need no hub state (`hub-env`, `knowledge`; the floor is
/// refused, every other name is unknown).
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
