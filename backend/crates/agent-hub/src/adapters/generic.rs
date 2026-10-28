//! Port of backend/src/agent_hub/adapters/generic.py: the declarative `generic` adapter (id `generic`). Add ANY client
//! (Cursor, Codex CLI, Gemini CLI, ...) with a YAML spec, no code. Owner: WS-A.
//!
//! * unknown keys are ERRORS and the message carries the precise path (`spec.skills.layoutt: unknown key`); the spec
//!   models are validated here with pydantic 2.13's lax-mode rules and messages (smart unions `str | int | float |
//!   bool` and `str | list[str]`, `Keys should be strings`, bytes accepted as str), golden-tested against pydantic;
//! * templating is a tiny explicit substituter over `{placeholder}` tokens (allow-listed names per context): no
//!   format strings, no eval, no attribute access;
//! * every emitted path goes through `_util::safe_relpath`; names are validated by fullmatch before they touch a path;
//! * [`validate_spec`] and [`dry_render`] are what the core's `POST /clients/validate-spec` calls; [`caps_for_spec`]
//!   is what the resolver uses for a generic client's caps;
//! * TOML (`mcp.format: toml`) is a hand-ported minimal emitter ([`toml_tables`]) delivered as a hash marker block.

use std::sync::{Arc, LazyLock};

use hub_core::models::CAPABILITIES;
use hub_core::py::{os as pyos, text};
use hub_core::pyd::{ErrItem, Loc};
use hub_core::value::PyValue;
use indexmap::IndexMap;
use regex::{Captures, Regex};

use super::_util as u;
use super::base::{
    AdapterCaps, AdapterError, AdapterResult, AgentItem, Artifact, Check, ClientAdapter, ClientConfig, CommandRunner,
    Diag, DiscoveredContent, Expected, FileSystem, InstructionItem, McpItem, MemoryItem, PermissionSet,
    RenderContext, RenderResult, SkillItem,
};
use super::claude_code::agent_item;

pub const ID: &str = "generic";

const TIERS: [&str; 3] = ["fast", "standard", "deep"];
const DECISIONS: [&str; 3] = ["allow", "ask", "deny"];
const RULE_KINDS: [&str; 6] = ["tool", "command", "path_read", "path_write", "egress_host", "mcp_server"];
const TRANSPORTS: [&str; 3] = ["stdio", "http", "sse"];
const AGENT_SOURCES: [&str; 4] = ["name", "description", "mode", "body"];
const MCP_PLACEHOLDERS: [&str; 9] = [
    "name",
    "command",
    "args",
    "argv",
    "url",
    "env",
    "env_names",
    "transport",
    "pinned_ref",
];
const RULE_PLACEHOLDERS: [&str; 2] = ["match", "tool"];

static PH: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"\{([a-z_][a-z0-9_]*)\}"));
static PH_FULL: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^\{([a-z_][a-z0-9_]*)\}$"));
static KEYPATH: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^[A-Za-z0-9_-]+(\.[A-Za-z0-9_-]+)*$"));
static TOML_BARE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^[A-Za-z0-9_-]+$"));

// ---- the spec, after validation ------------------------------------------------------------------------------------------
/// `SkillsSpec`.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillsSpec {
    pub root: String,
    pub path: String,
    /// "dir_per_skill" | "flat"
    pub layout: String,
    pub filename: String,
}

/// `AgentsSpec`. Dict fields keep their insertion order.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentsSpec {
    pub root: String,
    pub path: String,
    pub filename: String,
    /// "md_frontmatter" | "json" | "yaml"
    pub format: String,
    /// output key -> source
    pub fields: Vec<(String, String)>,
    /// str | int | float | bool values
    pub static_fields: Vec<(String, PyValue)>,
    pub tools_field: String,
    /// "list" | "csv" | "bool_map"
    pub tools_format: String,
    pub model_field: Option<String>,
    pub tool_map: Vec<(String, Option<String>)>,
    pub model_tiers: Vec<(String, String)>,
}

/// `InstructionsSpec`.
#[derive(Debug, Clone, PartialEq)]
pub struct InstructionsSpec {
    pub root: String,
    pub file: String,
    /// "own" | "block" | "concat"
    pub mode: String,
    /// "md" | "hash"
    pub marker: String,
}

/// `McpSpec`.
#[derive(Debug, Clone, PartialEq)]
pub struct McpSpec {
    pub root: String,
    pub file: String,
    /// "json" | "yaml" | "toml"
    pub format: String,
    pub key_path: String,
    /// transport -> template object (a dict)
    pub entry: Vec<(String, PyValue)>,
    /// "dollar_brace" | "env_ref" | "names_only"
    pub env_style: String,
    pub wrapper: Option<Vec<String>>,
}

/// A rule template: one string or a list of them.
#[derive(Debug, Clone, PartialEq)]
pub enum RuleTemplate {
    One(String),
    Many(Vec<String>),
}

impl RuleTemplate {
    fn all(&self) -> Vec<&str> {
        match self {
            RuleTemplate::One(s) => vec![s.as_str()],
            RuleTemplate::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

/// `PermissionsSpec`.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionsSpec {
    pub root: String,
    pub file: String,
    /// "json" | "yaml"
    pub format: String,
    /// decision -> dotted key path
    pub lists: Vec<(String, String)>,
    pub rule_templates: Vec<(String, RuleTemplate)>,
}

/// `MemorySpec`.
#[derive(Debug, Clone, PartialEq)]
pub struct MemorySpec {
    pub root: String,
    pub path: String,
    pub filename: String,
    pub frontmatter: bool,
}

/// `Spec`.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub notes: String,
    pub tool_map: Vec<(String, Option<String>)>,
    pub model_tiers: Vec<(String, String)>,
    pub skills: Option<SkillsSpec>,
    pub agents: Option<AgentsSpec>,
    pub instructions: Option<InstructionsSpec>,
    pub mcp: Option<McpSpec>,
    pub permissions: Option<PermissionsSpec>,
    pub memory: Option<MemorySpec>,
}

fn lookup<'a, V>(pairs: &'a [(String, V)], key: &str) -> Option<&'a V> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// `{**a, **b}` of two str-keyed dicts.
fn merged<V: Clone>(a: &[(String, V)], b: &[(String, V)]) -> Vec<(String, V)> {
    let mut out: Vec<(String, V)> = a.to_vec();
    for (k, v) in b {
        match out.iter_mut().find(|(x, _)| x == k) {
            Some(slot) => slot.1 = v.clone(),
            None => out.push((k.clone(), v.clone())),
        }
    }
    out
}

// ---- validation: pydantic 2.13 (lax) for the spec models ------------------------------------------------------------------
enum Ft {
    Str,
    OptStr,
    Bool,
    /// `Literal[...]` of strings
    Lit(&'static [&'static str]),
    /// `Literal[1]` (accepts 1, True and 1.0, which compare equal to 1)
    One,
    OptListStr,
    /// `dict[str, V]`
    Dict(Box<Ft>),
    /// `dict[str, Any]`
    Any,
    /// `str | int | float | bool`
    Scalar,
    /// `str | list[str]`
    StrOrList,
    /// `Model | None`
    OptModel(fn() -> ModelDef),
}

struct Fd {
    name: &'static str,
    ft: Ft,
    default: Option<fn() -> PyValue>,
}

struct ModelDef {
    name: &'static str,
    fields: Vec<Fd>,
}

fn req(name: &'static str, ft: Ft) -> Fd {
    Fd { name, ft, default: None }
}

fn opt(name: &'static str, ft: Ft, default: fn() -> PyValue) -> Fd {
    Fd {
        name,
        ft,
        default: Some(default),
    }
}

fn empty_dict() -> PyValue {
    PyValue::Dict(Vec::new())
}

fn none() -> PyValue {
    PyValue::None
}

fn skills_model() -> ModelDef {
    ModelDef {
        name: "SkillsSpec",
        fields: vec![
            req("root", Ft::Str),
            req("path", Ft::Str),
            opt("layout", Ft::Lit(&["dir_per_skill", "flat"]), || PyValue::str("dir_per_skill")),
            opt("filename", Ft::Str, || PyValue::str("SKILL.md")),
        ],
    }
}

fn agents_model() -> ModelDef {
    ModelDef {
        name: "AgentsSpec",
        fields: vec![
            req("root", Ft::Str),
            req("path", Ft::Str),
            opt("filename", Ft::Str, || PyValue::str("{name}.md")),
            opt("format", Ft::Lit(&["md_frontmatter", "json", "yaml"]), || PyValue::str("md_frontmatter")),
            opt("fields", Ft::Dict(Box::new(Ft::Str)), || {
                PyValue::Dict(vec![
                    (PyValue::str("name"), PyValue::str("name")),
                    (PyValue::str("description"), PyValue::str("description")),
                ])
            }),
            opt("static", Ft::Dict(Box::new(Ft::Scalar)), empty_dict),
            opt("tools_field", Ft::Str, || PyValue::str("tools")),
            opt("tools_format", Ft::Lit(&["list", "csv", "bool_map"]), || PyValue::str("csv")),
            opt("model_field", Ft::OptStr, none),
            opt("tool_map", Ft::Dict(Box::new(Ft::OptStr)), empty_dict),
            opt("model_tiers", Ft::Dict(Box::new(Ft::Str)), empty_dict),
        ],
    }
}

fn instructions_model() -> ModelDef {
    ModelDef {
        name: "InstructionsSpec",
        fields: vec![
            req("root", Ft::Str),
            req("file", Ft::Str),
            opt("mode", Ft::Lit(&["own", "block", "concat"]), || PyValue::str("block")),
            opt("marker", Ft::Lit(&["md", "hash"]), || PyValue::str("md")),
        ],
    }
}

fn mcp_model() -> ModelDef {
    ModelDef {
        name: "McpSpec",
        fields: vec![
            req("root", Ft::Str),
            req("file", Ft::Str),
            opt("format", Ft::Lit(&["json", "yaml", "toml"]), || PyValue::str("json")),
            opt("key_path", Ft::Str, || PyValue::str("mcpServers")),
            req("entry", Ft::Dict(Box::new(Ft::Dict(Box::new(Ft::Any))))),
            opt("env_style", Ft::Lit(&["dollar_brace", "env_ref", "names_only"]), || {
                PyValue::str("dollar_brace")
            }),
            opt("wrapper", Ft::OptListStr, none),
        ],
    }
}

fn permissions_model() -> ModelDef {
    ModelDef {
        name: "PermissionsSpec",
        fields: vec![
            req("root", Ft::Str),
            req("file", Ft::Str),
            opt("format", Ft::Lit(&["json", "yaml"]), || PyValue::str("json")),
            req("lists", Ft::Dict(Box::new(Ft::Str))),
            req("rule_templates", Ft::Dict(Box::new(Ft::StrOrList))),
        ],
    }
}

fn memory_model() -> ModelDef {
    ModelDef {
        name: "MemorySpec",
        fields: vec![
            req("root", Ft::Str),
            req("path", Ft::Str),
            opt("filename", Ft::Str, || PyValue::str("{id}.md")),
            opt("frontmatter", Ft::Bool, || PyValue::Bool(true)),
        ],
    }
}

fn spec_model() -> ModelDef {
    ModelDef {
        name: "Spec",
        fields: vec![
            opt("version", Ft::One, || PyValue::Int(1)),
            opt("notes", Ft::Str, || PyValue::str("")),
            opt("tool_map", Ft::Dict(Box::new(Ft::OptStr)), empty_dict),
            opt("model_tiers", Ft::Dict(Box::new(Ft::Str)), empty_dict),
            opt("skills", Ft::OptModel(skills_model), none),
            opt("agents", Ft::OptModel(agents_model), none),
            opt("instructions", Ft::OptModel(instructions_model), none),
            opt("mcp", Ft::OptModel(mcp_model), none),
            opt("permissions", Ft::OptModel(permissions_model), none),
            opt("memory", Ft::OptModel(memory_model), none),
        ],
    }
}

fn perr(loc: &[Loc], kind: &str, msg: &str, input: &PyValue) -> ErrItem {
    ErrItem {
        loc: loc.to_vec(),
        msg: msg.into(),
        kind: kind.into(),
        input: input.clone(),
    }
}

// pydantic's lax rules live in hub-core (one implementation for every model; A-10)
use hub_core::pyd::{key_loc, lax_str};

/// pydantic 2.13 lax `bool` ([`hub_core::pyd::lax_bool`]), pushing its error.
fn lax_bool(v: &PyValue, loc: &[Loc], errs: &mut Vec<ErrItem>) -> PyValue {
    match hub_core::pyd::lax_bool(v) {
        Ok(b) => PyValue::Bool(b),
        Err((kind, msg)) => {
            errs.push(perr(loc, kind, msg, v));
            PyValue::None
        }
    }
}

fn with(loc: &mut Vec<Loc>, l: Loc, f: impl FnOnce(&mut Vec<Loc>)) {
    loc.push(l);
    f(loc);
    loc.pop();
}

fn validate_list_str(v: &PyValue, loc: &mut Vec<Loc>, errs: &mut Vec<ErrItem>) -> PyValue {
    let (PyValue::List(items) | PyValue::Set(items)) = v else {
        errs.push(perr(loc, "list_type", "Input should be a valid list", v));
        return PyValue::None;
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, it) in items.iter().enumerate() {
        match lax_str(it) {
            Ok(s) => out.push(PyValue::Str(s)),
            Err((kind, msg)) => with(loc, Loc::Index(i), |l| errs.push(perr(l, kind, msg, it))),
        }
    }
    PyValue::List(out)
}

fn validate_ft(ft: &Ft, v: &PyValue, loc: &mut Vec<Loc>, errs: &mut Vec<ErrItem>) -> PyValue {
    match ft {
        Ft::OptStr | Ft::OptListStr | Ft::OptModel(_) if v.is_none() => PyValue::None,
        Ft::Any => v.clone(),
        Ft::Str => match lax_str(v) {
            Ok(s) => PyValue::Str(s),
            Err((kind, msg)) => {
                errs.push(perr(loc, kind, msg, v));
                PyValue::None
            }
        },
        Ft::OptStr => validate_ft(&Ft::Str, v, loc, errs),
        Ft::Bool => lax_bool(v, loc, errs),
        Ft::Lit(members) => match v {
            PyValue::Str(s) if members.contains(&s.as_str()) => v.clone(),
            _ => {
                let parts: Vec<String> = members.iter().map(|m| text::repr(m)).collect();
                let expected = match parts.split_last() {
                    Some((last, [])) => last.clone(),
                    Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
                    None => String::new(),
                };
                errs.push(perr(loc, "literal_error", &format!("Input should be {expected}"), v));
                PyValue::None
            }
        },
        Ft::One => match v {
            PyValue::Int(1) | PyValue::Bool(true) => PyValue::Int(1),
            PyValue::Float(f) if *f == 1.0 => PyValue::Int(1),
            _ => {
                errs.push(perr(loc, "literal_error", "Input should be 1", v));
                PyValue::None
            }
        },
        Ft::OptListStr => validate_list_str(v, loc, errs),
        Ft::Dict(inner) => {
            let Some(d) = v.as_dict() else {
                errs.push(perr(loc, "dict_type", "Input should be a valid dictionary", v));
                return PyValue::None;
            };
            let mut out: Vec<(PyValue, PyValue)> = Vec::with_capacity(d.len());
            for (k, val) in d {
                loc.push(key_loc(k));
                let key = match lax_str(k) {
                    Ok(s) => PyValue::Str(s),
                    Err((kind, msg)) => {
                        with(loc, Loc::Key("[key]".into()), |l| errs.push(perr(l, kind, msg, k)));
                        PyValue::None
                    }
                };
                let vv = validate_ft(inner, val, loc, errs);
                loc.pop();
                match out.iter_mut().find(|(ok, _)| *ok == key && !key.is_none()) {
                    Some(slot) => slot.1 = vv,
                    None => out.push((key, vv)),
                }
            }
            PyValue::Dict(out)
        }
        Ft::Scalar => match v {
            PyValue::Str(_) | PyValue::Int(_) | PyValue::BigInt(_) | PyValue::Float(_) | PyValue::Bool(_) => v.clone(),
            PyValue::Bytes(b) if std::str::from_utf8(b).is_ok() => PyValue::Str(u::decode_replace(b)),
            _ => {
                for (tag, kind, msg) in [
                    ("str", "string_type", "Input should be a valid string"),
                    ("int", "int_type", "Input should be a valid integer"),
                    ("float", "float_type", "Input should be a valid number"),
                    ("bool", "bool_type", "Input should be a valid boolean"),
                ] {
                    with(loc, Loc::Key(tag.into()), |l| errs.push(perr(l, kind, msg, v)));
                }
                PyValue::None
            }
        },
        Ft::StrOrList => {
            if let Ok(s) = lax_str(v) {
                return PyValue::Str(s);
            }
            let mut list_errs = Vec::new();
            let out = with_ret(loc, Loc::Key("list[str]".into()), |l| validate_list_str(v, l, &mut list_errs));
            if list_errs.is_empty() {
                return out;
            }
            with(loc, Loc::Key("str".into()), |l| {
                errs.push(perr(l, "string_type", "Input should be a valid string", v));
            });
            errs.extend(list_errs);
            PyValue::None
        }
        Ft::OptModel(model) => validate_model(&model(), v, loc, errs),
    }
}

fn with_ret<T>(loc: &mut Vec<Loc>, l: Loc, f: impl FnOnce(&mut Vec<Loc>) -> T) -> T {
    loc.push(l);
    let out = f(loc);
    loc.pop();
    out
}

fn validate_model(m: &ModelDef, v: &PyValue, loc: &mut Vec<Loc>, errs: &mut Vec<ErrItem>) -> PyValue {
    let Some(d) = v.as_dict() else {
        let msg = format!("Input should be a valid dictionary or instance of {}", m.name);
        errs.push(perr(loc, "model_type", &msg, v));
        return PyValue::None;
    };
    let mut out: Vec<(PyValue, PyValue)> = Vec::new();
    for f in &m.fields {
        let given = d.iter().find(|(k, _)| k.as_str() == Some(f.name)).map(|(_, x)| x);
        loc.push(Loc::Key(f.name.into()));
        match (given, f.default) {
            (Some(x), _) => {
                let vv = validate_ft(&f.ft, x, loc, errs);
                out.push((PyValue::str(f.name), vv));
            }
            (None, Some(def)) => out.push((PyValue::str(f.name), def())),
            (None, None) => errs.push(perr(loc, "missing", "Field required", v)),
        }
        loc.pop();
    }
    for (k, val) in d {
        loc.push(key_loc(k));
        match k.as_str() {
            None => errs.push(perr(loc, "invalid_key", "Keys should be strings", k)),
            Some(s) if !m.fields.iter().any(|f| f.name == s) => {
                errs.push(perr(loc, "extra_forbidden", "Extra inputs are not permitted", val));
            }
            Some(_) => {}
        }
        loc.pop();
    }
    PyValue::Dict(out)
}

fn s_of(v: Option<&PyValue>) -> String {
    v.and_then(PyValue::as_str).unwrap_or_default().to_string()
}

fn str_pairs(v: Option<&PyValue>) -> Vec<(String, String)> {
    v.and_then(PyValue::as_dict)
        .unwrap_or_default()
        .iter()
        .map(|(k, x)| (k.py_str(), x.py_str()))
        .collect()
}

fn opt_str_pairs(v: Option<&PyValue>) -> Vec<(String, Option<String>)> {
    v.and_then(PyValue::as_dict)
        .unwrap_or_default()
        .iter()
        .map(|(k, x)| (k.py_str(), x.as_str().map(str::to_string)))
        .collect()
}

impl Spec {
    fn from_validated(v: &PyValue) -> Spec {
        let section = |name: &str| v.get(name).filter(|x| !x.is_none());
        Spec {
            notes: s_of(v.get("notes")),
            tool_map: opt_str_pairs(v.get("tool_map")),
            model_tiers: str_pairs(v.get("model_tiers")),
            skills: section("skills").map(|s| SkillsSpec {
                root: s_of(s.get("root")),
                path: s_of(s.get("path")),
                layout: s_of(s.get("layout")),
                filename: s_of(s.get("filename")),
            }),
            agents: section("agents").map(|a| AgentsSpec {
                root: s_of(a.get("root")),
                path: s_of(a.get("path")),
                filename: s_of(a.get("filename")),
                format: s_of(a.get("format")),
                fields: str_pairs(a.get("fields")),
                static_fields: a
                    .get("static")
                    .and_then(PyValue::as_dict)
                    .unwrap_or_default()
                    .iter()
                    .map(|(k, x)| (k.py_str(), x.clone()))
                    .collect(),
                tools_field: s_of(a.get("tools_field")),
                tools_format: s_of(a.get("tools_format")),
                model_field: a.get("model_field").and_then(PyValue::as_str).map(str::to_string),
                tool_map: opt_str_pairs(a.get("tool_map")),
                model_tiers: str_pairs(a.get("model_tiers")),
            }),
            instructions: section("instructions").map(|i| InstructionsSpec {
                root: s_of(i.get("root")),
                file: s_of(i.get("file")),
                mode: s_of(i.get("mode")),
                marker: s_of(i.get("marker")),
            }),
            mcp: section("mcp").map(|m| McpSpec {
                root: s_of(m.get("root")),
                file: s_of(m.get("file")),
                format: s_of(m.get("format")),
                key_path: s_of(m.get("key_path")),
                entry: m
                    .get("entry")
                    .and_then(PyValue::as_dict)
                    .unwrap_or_default()
                    .iter()
                    .map(|(k, x)| (k.py_str(), x.clone()))
                    .collect(),
                env_style: s_of(m.get("env_style")),
                wrapper: m
                    .get("wrapper")
                    .and_then(PyValue::as_list)
                    .map(|l| l.iter().map(PyValue::py_str).collect()),
            }),
            permissions: section("permissions").map(|p| PermissionsSpec {
                root: s_of(p.get("root")),
                file: s_of(p.get("file")),
                format: s_of(p.get("format")),
                lists: str_pairs(p.get("lists")),
                rule_templates: p
                    .get("rule_templates")
                    .and_then(PyValue::as_dict)
                    .unwrap_or_default()
                    .iter()
                    .map(|(k, x)| {
                        let t = match x {
                            PyValue::List(l) => RuleTemplate::Many(l.iter().map(PyValue::py_str).collect()),
                            other => RuleTemplate::One(other.py_str()),
                        };
                        (k.py_str(), t)
                    })
                    .collect(),
            }),
            memory: section("memory").map(|m| MemorySpec {
                root: s_of(m.get("root")),
                path: s_of(m.get("path")),
                filename: s_of(m.get("filename")),
                frontmatter: matches!(m.get("frontmatter"), Some(PyValue::Bool(true))),
            }),
        }
    }
}

/// `_pydantic_diags(exc)`.
fn pydantic_diags(errs: &[ErrItem]) -> Vec<Diag> {
    errs.iter()
        .map(|e| {
            let mut parts = vec!["spec".to_string()];
            parts.extend(e.loc.iter().map(ToString::to_string));
            let path = parts.join(".");
            let msg = if e.kind == "extra_forbidden" { "unknown key" } else { e.msg.as_str() };
            u::diag("error", "spec_invalid", format!("{path}: {msg}"), Some(&path))
        })
        .collect()
}

fn err(path: &str, msg: &str) -> Diag {
    u::diag("error", "spec_invalid", format!("{path}: {msg}"), Some(path))
}

fn sorted_join(allowed: &[&str]) -> String {
    let mut v = allowed.to_vec();
    v.sort_unstable();
    v.join(", ")
}

/// `_check_template(s, allowed, path, out)`.
fn check_template(s: &str, allowed: &[&str], path: &str, out: &mut Vec<Diag>) {
    for m in PH.captures_iter(s) {
        let name = m.get(1).map_or("", |x| x.as_str());
        if !allowed.contains(&name) {
            out.push(err(
                path,
                &format!("unknown placeholder {{{name}}} (allowed: {})", sorted_join(allowed)),
            ));
        }
    }
    let rest = PH.replace_all(s, "");
    if rest.contains('{') || rest.contains('}') {
        out.push(err(path, "stray brace; write placeholders as {name}"));
    }
}

/// `_check_template_tree(obj, allowed, path, out)`.
fn check_template_tree(obj: &PyValue, allowed: &[&str], path: &str, out: &mut Vec<Diag>) {
    match obj {
        PyValue::Str(s) => check_template(s, allowed, path, out),
        PyValue::Dict(d) => {
            for (k, v) in d {
                check_template(&k.py_str(), &[], &format!("{path}.<key {}>", k.repr()), out);
                check_template_tree(v, allowed, &format!("{path}.{}", k.py_str()), out);
            }
        }
        PyValue::List(l) => {
            for (i, v) in l.iter().enumerate() {
                check_template_tree(v, allowed, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

fn check_relpath(s: &str, path: &str, out: &mut Vec<Diag>) {
    if let Err(exc) = u::safe_relpath(&[s]) {
        out.push(err(path, &format!("unsafe relative path: {exc}")));
    }
}

fn check_maps<A, B>(tool_map: &[(String, A)], tiers: &[(String, B)], path: &str, out: &mut Vec<Diag>) {
    for (k, _) in tool_map {
        if !CAPABILITIES.contains(&k.as_str()) {
            out.push(err(
                &format!("{path}.tool_map.{k}"),
                &format!("unknown capability (allowed: {})", CAPABILITIES.join(", ")),
            ));
        }
    }
    for (k, _) in tiers {
        if !TIERS.contains(&k.as_str()) {
            out.push(err(
                &format!("{path}.model_tiers.{k}"),
                &format!("unknown tier (allowed: {})", TIERS.join(", ")),
            ));
        }
    }
}

/// `_semantic(spec)`.
fn semantic(spec: &Spec) -> Vec<Diag> {
    let mut out = Vec::new();
    check_maps(&spec.tool_map, &spec.model_tiers, "spec", &mut out);
    if let Some(s) = &spec.skills {
        check_relpath(&s.path, "spec.skills.path", &mut out);
        if s.layout == "flat" {
            check_template(&s.filename, &["name"], "spec.skills.filename", &mut out);
            if !s.filename.contains("{name}") {
                out.push(err("spec.skills.filename", "flat layout needs {name} in the filename"));
            }
        } else {
            check_relpath(&s.filename, "spec.skills.filename", &mut out);
            check_template(&s.filename, &[], "spec.skills.filename", &mut out);
        }
    }
    if let Some(a) = &spec.agents {
        check_relpath(&a.path, "spec.agents.path", &mut out);
        check_template(&a.filename, &["name"], "spec.agents.filename", &mut out);
        if !a.filename.contains("{name}") {
            out.push(err("spec.agents.filename", "needs {name}"));
        }
        check_maps(&a.tool_map, &a.model_tiers, "spec.agents", &mut out);
        for (k, src) in &a.fields {
            if !AGENT_SOURCES.contains(&src.as_str()) {
                out.push(err(
                    &format!("spec.agents.fields.{k}"),
                    &format!(
                        "unknown source {} (allowed: {})",
                        text::repr(src),
                        AGENT_SOURCES.join(", ")
                    ),
                ));
            }
        }
        let maps_body = a.fields.iter().any(|(_, v)| v == "body");
        if a.format == "md_frontmatter" && maps_body {
            out.push(err(
                "spec.agents.fields",
                "md_frontmatter puts the body after the frontmatter; do not map a field to 'body'",
            ));
        }
        if a.format != "md_frontmatter" && !maps_body {
            out.push(err(
                "spec.agents.fields",
                &format!(
                    "{} agents need one field mapped to 'body' or the instructions are lost",
                    a.format
                ),
            ));
        }
        if a.model_field.is_some() && a.model_tiers.is_empty() && spec.model_tiers.is_empty() {
            out.push(err("spec.agents.model_field", "set but no model_tiers are defined"));
        }
    }
    if let Some(i) = &spec.instructions {
        check_relpath(&i.file, "spec.instructions.file", &mut out);
    }
    if let Some(m) = &spec.mcp {
        check_relpath(&m.file, "spec.mcp.file", &mut out);
        if !KEYPATH.is_match(&m.key_path) {
            out.push(err(
                "spec.mcp.key_path",
                "must be dotted identifiers, e.g. mcpServers or mcp.servers",
            ));
        }
        for (tr, tmpl) in &m.entry {
            if !TRANSPORTS.contains(&tr.as_str()) {
                out.push(err(
                    &format!("spec.mcp.entry.{tr}"),
                    &format!("unknown transport (allowed: {})", TRANSPORTS.join(", ")),
                ));
            }
            check_template_tree(tmpl, &MCP_PLACEHOLDERS, &format!("spec.mcp.entry.{tr}"), &mut out);
        }
        if m.entry.is_empty() {
            out.push(err("spec.mcp.entry", "at least one transport template is required"));
        }
    }
    if let Some(p) = &spec.permissions {
        check_relpath(&p.file, "spec.permissions.file", &mut out);
        for (d, kp) in &p.lists {
            if !DECISIONS.contains(&d.as_str()) {
                out.push(err(
                    &format!("spec.permissions.lists.{d}"),
                    &format!("unknown decision (allowed: {})", DECISIONS.join(", ")),
                ));
            }
            if !KEYPATH.is_match(kp) {
                out.push(err(&format!("spec.permissions.lists.{d}"), "must be a dotted key path"));
            }
        }
        if p.lists.is_empty() {
            out.push(err("spec.permissions.lists", "at least one decision list is required"));
        }
        for (k, rt) in &p.rule_templates {
            if !RULE_KINDS.contains(&k.as_str()) {
                out.push(err(
                    &format!("spec.permissions.rule_templates.{k}"),
                    &format!("unknown rule kind (allowed: {})", RULE_KINDS.join(", ")),
                ));
            }
            for (i, t) in rt.all().into_iter().enumerate() {
                check_template(
                    t,
                    &RULE_PLACEHOLDERS,
                    &format!("spec.permissions.rule_templates.{k}[{i}]"),
                    &mut out,
                );
            }
        }
    }
    if let Some(m) = &spec.memory {
        check_relpath(&m.path, "spec.memory.path", &mut out);
        check_template(&m.filename, &["id"], "spec.memory.filename", &mut out);
        if !m.filename.contains("{id}") {
            out.push(err("spec.memory.filename", "needs {id}"));
        }
    }
    out
}

/// `parse_spec(spec)`: the validated spec (None when there is any problem) and every problem found.
pub fn parse_spec(spec: Option<&PyValue>) -> (Option<Spec>, Vec<Diag>) {
    let Some(spec @ PyValue::Dict(_)) = spec else {
        return (None, vec![err("spec", "must be a mapping")]);
    };
    let mut errs = Vec::new();
    let validated = validate_model(&spec_model(), spec, &mut Vec::new(), &mut errs);
    if !errs.is_empty() {
        return (None, pydantic_diags(&errs));
    }
    let parsed = Spec::from_validated(&validated);
    let diags = semantic(&parsed);
    (if diags.is_empty() { Some(parsed) } else { None }, diags)
}

/// `validate_spec(spec)`: all problems with a spec (empty = valid). Used by POST /clients/validate-spec.
pub fn validate_spec(spec: &PyValue) -> Vec<Diag> {
    parse_spec(Some(spec)).1
}

/// `dry_render(spec, ctx)`: render sample content with `spec` regardless of `ctx.client.spec` (the UI's live preview).
pub fn dry_render(spec: &PyValue, ctx: &RenderContext) -> AdapterResult<RenderResult> {
    let mut c = ctx.clone();
    c.client.adapter = ID.into();
    c.client.spec = Some(spec.clone());
    GenericSpecAdapter.render(&c)
}

/// `caps_for_spec(spec)`: the capabilities a spec implies (`caps()` cannot know them). The resolver uses it for a
/// generic client (and falls back to the adapter's own caps on `Err`, as Python does on an exception); it never fails
/// today, the `Result` mirrors Python's `try`.
pub fn caps_for_spec(spec: &PyValue) -> AdapterResult<AdapterCaps> {
    let (parsed, _) = parse_spec(Some(spec));
    let Some(p) = parsed else {
        return Ok(AdapterCaps {
            notes: "invalid spec".into(),
            ..AdapterCaps::default()
        });
    };
    let (agent_tools, agent_tiers) = p
        .agents
        .as_ref()
        .map(|a| (a.tool_map.clone(), a.model_tiers.clone()))
        .unwrap_or_default();
    let tm = merged(&p.tool_map, &agent_tools);
    let tiers = merged(&p.model_tiers, &agent_tiers);
    Ok(AdapterCaps {
        skills: p.skills.is_some(),
        agents: p.agents.is_some(),
        instructions: p.instructions.is_some(),
        mcp: p.mcp.is_some(),
        permissions: p.permissions.is_some(),
        egress: false,
        memory: p.memory.is_some(),
        tool_map: CAPABILITIES
            .iter()
            .map(|c| ((*c).to_string(), lookup(&tm, c).cloned().flatten()))
            .collect(),
        model_tiers: tiers.into_iter().collect(),
        notes: p.notes,
    })
}

// ---- the tiny substituter ----------------------------------------------------------------------------------------------
/// The values a template may reference (`None` = Python None).
pub type Vals = IndexMap<&'static str, PyValue>;

fn list_dict_error(name: &str) -> AdapterError {
    AdapterError::new(
        "ValueError",
        format!("placeholder {{{name}}} is a list/dict and must be the whole value"),
    )
}

/// `_subst_str(s, vals)`: `Ok(None)` = `_DROP` (a whole-value placeholder with no value).
pub fn subst_str(s: &str, vals: &Vals) -> AdapterResult<Option<PyValue>> {
    if let Some(m) = PH_FULL.captures(s) {
        let name = m.get(1).map_or("", |x| x.as_str());
        let v = vals.get(name).cloned().unwrap_or(PyValue::None);
        let empty_dict = matches!(&v, PyValue::Dict(d) if d.is_empty());
        return Ok(if v.is_none() || empty_dict { None } else { Some(v) });
    }
    let mut failure: Option<AdapterError> = None;
    let out = PH.replace_all(s, |c: &Captures<'_>| {
        let name = c.get(1).map_or("", |x| x.as_str());
        match vals.get(name) {
            None | Some(PyValue::None) => String::new(),
            Some(PyValue::List(_) | PyValue::Dict(_)) => {
                if failure.is_none() {
                    failure = Some(list_dict_error(name));
                }
                String::new()
            }
            Some(v) => v.py_str(),
        }
    });
    match failure {
        Some(e) => Err(e),
        None => Ok(Some(PyValue::Str(out.into_owned()))),
    }
}

/// `subst(obj, vals)`: recursive placeholder substitution; values that resolve to nothing are dropped from their
/// dict/list. `Ok(None)` = the whole value was dropped.
pub fn subst(obj: &PyValue, vals: &Vals) -> AdapterResult<Option<PyValue>> {
    match obj {
        PyValue::Str(s) => subst_str(s, vals),
        PyValue::Dict(d) => {
            let mut out: Vec<(PyValue, PyValue)> = Vec::new();
            for (k, v) in d {
                if let Some(r) = subst(v, vals)? {
                    let key = PyValue::Str(k.py_str());
                    match out.iter_mut().find(|(x, _)| *x == key) {
                        Some(slot) => slot.1 = r,
                        None => out.push((key, r)),
                    }
                }
            }
            Ok(Some(PyValue::Dict(out)))
        }
        PyValue::List(l) => {
            let mut out = Vec::new();
            for v in l {
                if let Some(r) = subst(v, vals)? {
                    out.push(r);
                }
            }
            Ok(Some(PyValue::List(out)))
        }
        other => Ok(Some(other.clone())),
    }
}

/// `_fmt(template, **vals)`.
fn fmt(template: &str, vals: &[(&'static str, &str)]) -> AdapterResult<String> {
    let v: Vals = vals.iter().map(|(k, x)| (*k, PyValue::str(*x))).collect();
    Ok(subst_str(template, &v)?.map(|r| r.py_str()).unwrap_or_default())
}

// ---- TOML ------------------------------------------------------------------------------------------------------------------
fn toml_key(k: &str) -> String {
    if TOML_BARE.is_match(k) {
        k.to_string()
    } else {
        format!("\"{}\"", k.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// `_toml_val(v)`.
pub fn toml_val(v: &PyValue) -> AdapterResult<String> {
    Ok(match v {
        PyValue::Bool(b) => if *b { "true" } else { "false" }.into(),
        PyValue::Int(_) | PyValue::BigInt(_) | PyValue::Float(_) => v.py_str(),
        PyValue::List(l) => {
            let parts: AdapterResult<Vec<String>> = l.iter().map(toml_val).collect();
            format!("[{}]", parts?.join(", "))
        }
        PyValue::Dict(d) => {
            let mut parts = Vec::new();
            for (k, x) in u::sorted_items(d)? {
                parts.push(format!("{} = {}", toml_key(&k.py_str()), toml_val(&x)?));
            }
            format!("{{ {} }}", parts.join(", "))
        }
        other => {
            let s = other
                .py_str()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t");
            format!("\"{s}\"")
        }
    })
}

/// `toml_tables(prefix, servers)`: `[<prefix>.<name>]` tables (servers sorted by name; keys in template order).
pub fn toml_tables(prefix: &str, servers: &[(String, PyValue)]) -> AdapterResult<String> {
    let mut sorted: Vec<&(String, PyValue)> = servers.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut lines: Vec<String> = Vec::new();
    for (name, table) in sorted {
        lines.push(format!("[{prefix}.{}]", toml_key(name)));
        for (k, v) in table.as_dict().unwrap_or_default() {
            lines.push(format!("{} = {}", toml_key(&k.py_str()), toml_val(v)?));
        }
        lines.push(String::new());
    }
    Ok(format!("{}\n", text::rstrip(&lines.join("\n"))))
}

// ---- the adapter ---------------------------------------------------------------------------------------------------------
/// `GenericSpecAdapter`.
#[derive(Debug, Default, Clone, Copy)]
pub struct GenericSpecAdapter;

/// `{}` built in insertion order with Python dict assignment (an existing key keeps its place).
fn dict_set(d: &mut Vec<(PyValue, PyValue)>, k: &str, v: PyValue) {
    match d.iter_mut().find(|(x, _)| x.as_str() == Some(k)) {
        Some(slot) => slot.1 = v,
        None => d.push((PyValue::str(k), v)),
    }
}

/// `set_dotted` on a Python dict.
fn set_dotted_py(doc: &mut Vec<(PyValue, PyValue)>, path: &str, value: PyValue) {
    let parts: Vec<&str> = path.split('.').collect();
    let Some((first, rest)) = parts.split_first() else {
        return;
    };
    if rest.is_empty() {
        dict_set(doc, first, value);
        return;
    }
    let idx = match doc.iter().position(|(k, v)| k.as_str() == Some(first) && matches!(v, PyValue::Dict(_))) {
        Some(i) => i,
        None => {
            dict_set(doc, first, PyValue::Dict(Vec::new()));
            doc.iter().position(|(k, _)| k.as_str() == Some(first)).unwrap_or(0)
        }
    };
    if let Some((_, PyValue::Dict(inner))) = doc.get_mut(idx) {
        set_dotted_py(inner, &rest.join("."), value);
    }
}

impl GenericSpecAdapter {
    fn skills(s: &SkillsSpec, skills: &[SkillItem], diags: &mut Vec<Diag>) -> AdapterResult<Vec<Artifact>> {
        if s.layout == "dir_per_skill" {
            return u::skill_artifacts(skills, &s.root, &s.path, diags, &s.filename);
        }
        let mut out = Vec::new();
        let mut sorted: Vec<&SkillItem> = skills.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for sk in sorted {
            let Some(skill_md) = sk.files.get("SKILL.md").filter(|_| u::valid_name(&sk.name)) else {
                diags.push(u::diag(
                    "error",
                    "invalid_skill",
                    "invalid name or missing SKILL.md",
                    Some(&sk.name),
                ));
                continue;
            };
            if sk.files.len() > 1 {
                diags.push(u::diag(
                    "warn",
                    "extra_files_dropped",
                    "flat layout cannot carry supporting files; only SKILL.md is written",
                    Some(&sk.name),
                ));
            }
            let path = u::safe_relpath(&[&s.path, &fmt(&s.filename, &[("name", &sk.name)])?])?;
            let mut a = Artifact::new(&s.root, &path, skill_md.clone(), "skill");
            a.source_ids = vec![sk.name.clone()];
            out.push(a);
        }
        Ok(out)
    }

    fn tools(
        spec: &Spec,
        a: &AgentsSpec,
        ag: &AgentItem,
        cfg: &ClientConfig,
        diags: &mut Vec<Diag>,
    ) -> Vec<String> {
        let tmap = merged(&spec.tool_map, &a.tool_map);
        let caps = u::effective_caps(
            ag.capabilities
                .iter()
                .map(String::as_str)
                .filter(|c| CAPABILITIES.contains(c)),
            ag.read_only,
        );
        let mut natives: Vec<String> = Vec::new();
        for c in &caps {
            match lookup(&tmap, c).cloned().flatten() {
                None => diags.push(u::diag(
                    u::unsupported_severity(cfg),
                    "unsupported_capability",
                    format!("capability {} has no native tool in this spec", text::repr(c)),
                    Some(&ag.name),
                )),
                Some(n) => {
                    if !natives.contains(&n) {
                        natives.push(n);
                    }
                }
            }
        }
        natives
    }

    fn agents(
        spec: &Spec,
        a: &AgentsSpec,
        cfg: &ClientConfig,
        agents: &[AgentItem],
        diags: &mut Vec<Diag>,
    ) -> AdapterResult<Vec<Artifact>> {
        let mut out = Vec::new();
        let tmap = merged(&spec.tool_map, &a.tool_map);
        let tiers = merged(&spec.model_tiers, &a.model_tiers);
        let dup = u::dedupe(agents.iter().map(|g| g.name.as_str()), "agent", diags);
        let mut sorted: Vec<&AgentItem> = agents.iter().collect();
        sorted.sort_by(|x, y| x.name.cmp(&y.name));
        for ag in sorted {
            let name = ag.name.as_str();
            if dup.contains(name) {
                continue;
            }
            if !u::valid_name(name) {
                diags.push(u::diag(
                    "error",
                    "invalid_name",
                    format!("agent name {} is not a valid identifier", text::repr(name)),
                    Some(name),
                ));
                continue;
            }
            if !u::check_description(&ag.description, name, diags) {
                continue;
            }
            if text::char_len(&ag.body) > u::MAX_BODY_CHARS {
                diags.push(u::diag("error", "body_too_large", "agent body exceeds the size cap", Some(name)));
                continue;
            }
            let natives = Self::tools(spec, a, ag, cfg, diags);
            let desc = u::oneline(&ag.description, name, diags);
            let src = |s: &str| match s {
                "name" => ag.name.clone(),
                "description" => desc.clone(),
                "mode" => ag.mode.clone(),
                _ => ag.body.clone(),
            };
            let mut doc: Vec<(PyValue, PyValue)> = Vec::new();
            for (k, v) in &a.fields {
                dict_set(&mut doc, k, PyValue::Str(src(v)));
            }
            for (k, v) in &a.static_fields {
                dict_set(&mut doc, k, v.clone());
            }
            let tools_value = match a.tools_format.as_str() {
                "csv" => PyValue::Str(natives.join(", ")),
                "list" => PyValue::List(natives.iter().map(|n| PyValue::str(n.clone())).collect()),
                _ => {
                    let all = u::sorted_set(tmap.iter().filter_map(|(_, v)| v.clone()).filter(|v| !v.is_empty()));
                    PyValue::Dict(
                        all.into_iter()
                            .map(|n| {
                                let on = natives.contains(&n);
                                (PyValue::Str(n), PyValue::Bool(on))
                            })
                            .collect(),
                    )
                }
            };
            dict_set(&mut doc, &a.tools_field, tools_value);
            if natives.is_empty() {
                diags.push(u::diag(
                    "warn",
                    "empty_toolset",
                    "agent has no native tools in this client; the client may default to ALL tools",
                    Some(name),
                ));
            }
            if let (Some(field), Some(tier)) = (a.model_field.as_deref().filter(|f| !f.is_empty()), &ag.model_tier) {
                match lookup(&tiers, tier).filter(|m| !m.is_empty()) {
                    Some(model) => dict_set(&mut doc, field, PyValue::str(model.clone())),
                    None => diags.push(u::diag(
                        "info",
                        "no_model_mapping",
                        format!("no model mapped for tier {}", text::repr(tier)),
                        Some(name),
                    )),
                }
            }
            let path = u::safe_relpath(&[&a.path, &fmt(&a.filename, &[("name", name)])?])?;
            let doc = PyValue::Dict(doc);
            let content = match a.format.as_str() {
                "md_frontmatter" => u::to_bytes(&u::render_frontmatter(&doc, &ag.body)),
                "json" => u::dump_json_py(&doc)?,
                _ => u::dump_yaml(&doc),
            };
            let mut art = Artifact::new(&a.root, &path, content, "agent");
            art.source_ids = vec![name.to_string()];
            out.push(art);
        }
        Ok(out)
    }

    fn instructions(i: &InstructionsSpec, ctx: &RenderContext, diags: &mut Vec<Diag>) -> AdapterResult<Option<Artifact>> {
        let ids: Vec<String> = u::sorted_instructions(&ctx.instructions)
            .iter()
            .map(|x| x.id.clone())
            .collect();
        let path = u::safe_relpath(&[&i.file])?;
        if i.mode == "block" {
            let Ok(block) = u::wrap_block(&u::render_instructions(&ctx.instructions, true), &i.marker) else {
                diags.push(u::diag(
                    "error",
                    "marker_injection",
                    format!("{path}: {}; instructions not rendered", u::MarkerInjection),
                    None,
                ));
                return Ok(None);
            };
            let mut a = Artifact::new(&i.root, &path, u::to_bytes(&block), "instruction");
            a.source_ids = ids;
            a.merge = "block".into();
            return Ok(Some(a));
        }
        let s = format!("{}\n", u::render_instructions(&ctx.instructions, i.mode == "own"));
        let mut a = Artifact::new(&i.root, &path, u::to_bytes(&s), "instruction");
        a.source_ids = ids;
        Ok(Some(a))
    }

    fn mcp(m: &McpSpec, items: &[McpItem], diags: &mut Vec<Diag>) -> AdapterResult<Option<Artifact>> {
        let mut servers: Vec<(String, PyValue)> = Vec::new();
        let mut sorted: Vec<&McpItem> = items.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        let wrapper: Vec<String> = m.wrapper.clone().unwrap_or_default();
        for it in sorted {
            let name = it.name.as_str();
            if !u::valid_name(name) {
                diags.push(u::diag(
                    "error",
                    "invalid_name",
                    format!("mcp name {} is not valid", text::repr(name)),
                    Some(name),
                ));
                continue;
            }
            let Some(tmpl) = lookup(&m.entry, &it.transport) else {
                diags.push(u::diag(
                    "error",
                    "unsupported_transport",
                    format!("spec.mcp.entry has no template for {}", text::repr(&it.transport)),
                    Some(name),
                ));
                continue;
            };
            if it.scan_status != "clean" {
                diags.push(u::diag(
                    "info",
                    "mcp_not_clean",
                    format!("scan_status={}; the core blocks enabling unscanned servers", it.scan_status),
                    Some(name),
                ));
            }
            let command = it.command.as_deref().filter(|c| !c.is_empty());
            let sandboxed = it.sandbox_profile.as_deref().is_some_and(|p| !p.is_empty());
            let mut argv: Vec<String> = command.into_iter().map(str::to_string).collect();
            argv.extend(it.args.iter().cloned());
            if sandboxed && wrapper.is_empty() {
                u::sandbox_missing(name, diags);
                continue;
            }
            if sandboxed {
                argv = wrapper.iter().cloned().chain(argv).collect();
            }
            let mut env_names = it.env_names.clone();
            env_names.sort();
            let env: Vec<(PyValue, PyValue)> = match m.env_style.as_str() {
                "dollar_brace" => env_names
                    .iter()
                    .map(|n| (PyValue::str(n.clone()), PyValue::Str(format!("${{{n}}}"))))
                    .collect(),
                "env_ref" => env_names
                    .iter()
                    .map(|n| (PyValue::str(n.clone()), PyValue::Str(format!("{{env:{n}}}"))))
                    .collect(),
                _ => Vec::new(),
            };
            let strs = |v: &[String]| PyValue::List(v.iter().map(|s| PyValue::str(s.clone())).collect());
            let opt = |s: &Option<String>| s.clone().map_or(PyValue::None, PyValue::Str);
            let mut vals: Vals = IndexMap::new();
            vals.insert("name", PyValue::str(name));
            vals.insert("command", opt(&it.command));
            vals.insert("args", if command.is_some() { strs(&it.args) } else { PyValue::None });
            vals.insert("argv", if argv.is_empty() { PyValue::None } else { strs(&argv) });
            vals.insert("url", opt(&it.url));
            vals.insert("env", if env.is_empty() { PyValue::None } else { PyValue::Dict(env) });
            vals.insert(
                "env_names",
                if env_names.is_empty() {
                    PyValue::None
                } else {
                    strs(&env_names)
                },
            );
            vals.insert("transport", PyValue::str(it.transport.clone()));
            vals.insert("pinned_ref", opt(&it.pinned_ref));
            if command.is_some() && sandboxed {
                vals.insert("command", PyValue::str(argv[0].clone()));
                vals.insert("args", strs(&argv[1..]));
            }
            let value = subst(tmpl, &vals)?.unwrap_or(PyValue::None);
            match servers.iter_mut().find(|(n, _)| n == name) {
                Some(slot) => slot.1 = value,
                None => servers.push((name.to_string(), value)),
            }
        }
        let ids = u::sorted_set(servers.iter().map(|(n, _)| n.clone()));
        if m.format == "toml" {
            let body = toml_tables(&m.key_path, &servers)?;
            let Ok(block) = u::wrap_block(&body, "hash") else {
                diags.push(u::diag(
                    "error",
                    "marker_injection",
                    format!("{}: {}; mcp block not rendered", m.file, u::MarkerInjection),
                    None,
                ));
                return Ok(None);
            };
            let mut a = Artifact::new(&m.root, &u::safe_relpath(&[&m.file])?, u::to_bytes(&block), "mcp");
            a.source_ids = ids;
            a.merge = "block".into();
            return Ok(Some(a));
        }
        let mut doc: Vec<(PyValue, PyValue)> = Vec::new();
        let servers_value = PyValue::Dict(servers.into_iter().map(|(k, v)| (PyValue::Str(k), v)).collect());
        set_dotted_py(&mut doc, &m.key_path, servers_value);
        let doc = PyValue::Dict(doc);
        let (content, merge) = if m.format == "json" {
            (u::dump_json_py(&doc)?, "json_keys")
        } else {
            (u::dump_yaml(&doc), "yaml_keys")
        };
        let mut a = Artifact::new(&m.root, &u::safe_relpath(&[&m.file])?, content, "mcp");
        a.source_ids = ids;
        a.merge = merge.into();
        a.managed_keys = vec![m.key_path.clone()];
        Ok(Some(a))
    }

    fn permissions(
        spec: &Spec,
        p: &PermissionsSpec,
        cfg: &ClientConfig,
        perms: &PermissionSet,
        diags: &mut Vec<Diag>,
    ) -> AdapterResult<Artifact> {
        let mut lists: Vec<(String, Vec<String>)> = Vec::new();
        for (d, _) in &p.lists {
            if !lists.iter().any(|(x, _)| x == d) {
                lists.push((d.clone(), Vec::new()));
            }
        }
        for r in &perms.rules {
            let tool = lookup(&spec.tool_map, &r.matches).cloned().flatten();
            let t = lookup(&p.rule_templates, &r.kind);
            let Some(t) = t.filter(|_| !(r.kind == "tool" && tool.is_none())) else {
                diags.push(u::diag(
                    u::unsupported_severity(cfg),
                    "unsupported_rule",
                    format!("rule {}:{} has no mapping in spec.permissions", r.kind, r.matches),
                    None,
                ));
                continue;
            };
            let Some((_, set)) = lists.iter_mut().find(|(d, _)| *d == r.decision) else {
                diags.push(u::diag(
                    u::unsupported_severity(cfg),
                    "unsupported_decision",
                    format!(
                        "client has no {} list; rule {}:{} dropped",
                        text::repr(&r.decision),
                        r.kind,
                        r.matches
                    ),
                    None,
                ));
                continue;
            };
            let tool_s = tool.unwrap_or_default();
            for tmpl in t.all() {
                let s = fmt(tmpl, &[("match", &r.matches), ("tool", &tool_s)])?;
                if !set.contains(&s) {
                    set.push(s);
                }
            }
        }
        let mut doc: Vec<(PyValue, PyValue)> = Vec::new();
        for (d, kp) in &p.lists {
            let mut entries = lookup(&lists, d).cloned().unwrap_or_default();
            entries.sort();
            set_dotted_py(
                &mut doc,
                kp,
                PyValue::List(entries.into_iter().map(PyValue::Str).collect()),
            );
        }
        let doc = PyValue::Dict(doc);
        let (content, merge) = if p.format == "json" {
            (u::dump_json_py(&doc)?, "json_keys")
        } else {
            (u::dump_yaml(&doc), "yaml_keys")
        };
        let mut managed: Vec<String> = p.lists.iter().map(|(_, kp)| kp.clone()).collect();
        managed.sort();
        let mut a = Artifact::new(&p.root, &u::safe_relpath(&[&p.file])?, content, "rule");
        a.source_ids = u::sorted_set(perms.rules.iter().map(|r| format!("{}:{}", r.kind, r.matches)));
        a.merge = merge.into();
        a.managed_keys = managed;
        Ok(a)
    }

    fn memory(mem: &MemorySpec, items: &[MemoryItem], diags: &mut Vec<Diag>) -> AdapterResult<Vec<Artifact>> {
        let mut out = Vec::new();
        let mut sorted: Vec<&MemoryItem> = items.iter().collect();
        sorted.sort_by(|a, b| a.id.cmp(&b.id));
        for it in sorted {
            if !u::valid_name(&it.id) {
                diags.push(u::diag(
                    "error",
                    "invalid_name",
                    format!("memory id {} is not valid", text::repr(&it.id)),
                    Some(&it.id),
                ));
                continue;
            }
            let s = if mem.frontmatter {
                let fields = PyValue::Dict(vec![
                    (PyValue::str("id"), PyValue::str(it.id.clone())),
                    (PyValue::str("type"), PyValue::str(it.kind.clone())),
                    (PyValue::str("title"), PyValue::str(it.title.clone())),
                ]);
                u::render_frontmatter(&fields, &it.body)
            } else {
                format!("{}\n", text::rstrip(&it.body))
            };
            let path = u::safe_relpath(&[&mem.path, &fmt(&mem.filename, &[("id", &it.id)])?])?;
            let mut a = Artifact::new(&mem.root, &path, u::to_bytes(&s), "memory");
            a.source_ids = vec![it.id.clone()];
            out.push(a);
        }
        Ok(out)
    }

    fn discover_agents(
        spec: &Spec,
        a: &AgentsSpec,
        cfg: &ClientConfig,
        fs: &dyn FileSystem,
    ) -> AdapterResult<Vec<AgentItem>> {
        let Some(base) = u::fs_path(cfg, &a.root, &a.path)? else {
            return Ok(Vec::new());
        };
        if !fs.exists(&base) || !fs.is_dir(&base) {
            return Ok(Vec::new());
        }
        // `"^" + re.escape(filename).replace(re.escape("{name}"), "(?P<name>...)") + "$"` with Python's `$`
        // (end, or before a final newline)
        let pattern = format!(
            "^{}\n?$",
            regex::escape(&a.filename).replace(&regex::escape("{name}"), "(?P<name>[a-z0-9][a-z0-9._-]{0,63})")
        );
        let pat = Regex::new(&pattern).map_err(|e| AdapterError::new("error", e.to_string()))?;
        let tmap = merged(&spec.tool_map, &a.tool_map);
        let mut rev: Vec<(String, &str)> = Vec::new();
        for c in CAPABILITIES.iter().rev() {
            if let Some(n) = lookup(&tmap, c).cloned().flatten().filter(|n| !n.is_empty()) {
                match rev.iter_mut().find(|(k, _)| *k == n) {
                    Some(slot) => slot.1 = c,
                    None => rev.push((n, c)),
                }
            }
        }
        let desc_key = a
            .fields
            .iter()
            .find(|(_, v)| v == "description")
            .map_or("description", |(k, _)| k.as_str());
        let mut out = Vec::new();
        for fname in u::listdir_sorted(fs, &base)? {
            let Some(mt) = pat.captures(&fname) else {
                continue;
            };
            let (fm, body) = u::parse_frontmatter(&u::read_text(fs, &pyos::join(&base, &fname), &mut Vec::new()))?;
            let names: Vec<String> = match fm.get(&a.tools_field) {
                Some(PyValue::Str(s)) => s
                    .split(',')
                    .map(text::strip)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
                    .collect(),
                Some(PyValue::List(l)) => l.iter().map(PyValue::py_str).collect(),
                Some(PyValue::Dict(d)) => d.iter().filter(|(_, v)| u::truthy(v)).map(|(k, _)| k.py_str()).collect(),
                _ => Vec::new(),
            };
            let caps = u::ordered_caps(
                names
                    .iter()
                    .filter_map(|t| rev.iter().find(|(k, _)| k == t).map(|(_, c)| *c)),
            );
            let read_only = !caps.iter().any(|c| c == "write" || c == "edit");
            out.push(AgentItem {
                description: u::cap_str(fm.get(desc_key), u::MAX_DESC_ERR),
                capabilities: caps,
                read_only,
                body,
                ..agent_item(mt.name("name").map_or("", |x| x.as_str()))
            });
        }
        Ok(out)
    }
}

impl ClientAdapter for GenericSpecAdapter {
    fn id(&self) -> &str {
        ID
    }

    fn display_name(&self) -> &str {
        "Generic (declarative spec)"
    }

    fn docs(&self) -> &str {
        "" // the Python class has no docstring
    }

    fn caps(&self) -> AdapterCaps {
        AdapterCaps {
            skills: true,
            agents: true,
            instructions: true,
            mcp: true,
            permissions: true,
            egress: false,
            memory: true,
            notes: "Capabilities depend on the client's spec; use caps_for_spec(spec).".into(),
            ..AdapterCaps::default()
        }
    }

    fn default_config(&self) -> ClientConfig {
        let d = |pairs: &[(&str, &str)]| {
            PyValue::Dict(pairs.iter().map(|(k, v)| (PyValue::str(*k), PyValue::str(*v))).collect())
        };
        ClientConfig {
            id: "my-client".into(),
            adapter: ID.into(),
            display_name: "My client".into(),
            description: "Starter spec: skills + an instructions block. See docs/ADDING-A-CLIENT.md.".into(),
            roots: [("project".to_string(), "~/path/to/project".to_string())].into_iter().collect(),
            spec: Some(PyValue::Dict(vec![
                (PyValue::str("version"), PyValue::Int(1)),
                (
                    PyValue::str("skills"),
                    d(&[
                        ("root", "project"),
                        ("path", ".myclient/skills"),
                        ("layout", "dir_per_skill"),
                        ("filename", "SKILL.md"),
                    ]),
                ),
                (
                    PyValue::str("instructions"),
                    d(&[("root", "project"), ("file", "MYCLIENT.md"), ("mode", "block")]),
                ),
            ])),
            strict: false,
            ..ClientConfig::default()
        }
    }

    fn render(&self, ctx: &RenderContext) -> AdapterResult<RenderResult> {
        let cfg = &ctx.client;
        let (spec, sdiags) = parse_spec(cfg.spec.as_ref());
        let Some(spec) = spec else {
            let diagnostics = if sdiags.is_empty() {
                vec![err("spec", "generic client has no spec")]
            } else {
                sdiags
            };
            return Ok(RenderResult {
                artifacts: Vec::new(),
                diagnostics,
            });
        };
        let mut diags: Vec<Diag> = Vec::new();
        let mut arts: Vec<Artifact> = Vec::new();
        let root_ok = |section: &str, root: &str, diags: &mut Vec<Diag>| {
            if cfg.roots.contains_key(root) {
                return true;
            }
            let mut roots: Vec<&String> = cfg.roots.keys().collect();
            roots.sort();
            let shown = format!(
                "[{}]",
                roots.iter().map(|r| text::repr(r)).collect::<Vec<_>>().join(", ")
            );
            diags.push(u::diag(
                "error",
                "root_missing",
                format!(
                    "spec.{section}.root {} is not one of the client's roots {shown}",
                    text::repr(root)
                ),
                None,
            ));
            false
        };
        let missing = |section: &str, diags: &mut Vec<Diag>| {
            diags.push(u::diag(
                u::unsupported_severity(cfg),
                "unsupported_capability",
                format!("this client's spec has no {section} section"),
                None,
            ));
        };
        if !ctx.skills.is_empty() {
            match &spec.skills {
                None => missing("skills", &mut diags),
                Some(s) if root_ok("skills", &s.root, &mut diags) => {
                    arts.extend(Self::skills(s, &ctx.skills, &mut diags)?);
                }
                Some(_) => {}
            }
        }
        if !ctx.agents.is_empty() {
            match &spec.agents {
                None => missing("agents", &mut diags),
                Some(a) if root_ok("agents", &a.root, &mut diags) => {
                    arts.extend(Self::agents(&spec, a, cfg, &ctx.agents, &mut diags)?);
                }
                Some(_) => {}
            }
        }
        if !ctx.instructions.is_empty() {
            match &spec.instructions {
                None => missing("instructions", &mut diags),
                Some(i) if root_ok("instructions", &i.root, &mut diags) => {
                    arts.extend(Self::instructions(i, ctx, &mut diags)?);
                }
                Some(_) => {}
            }
        }
        if !ctx.mcp_servers.is_empty() {
            match &spec.mcp {
                None => missing("mcp", &mut diags),
                Some(m) if root_ok("mcp", &m.root, &mut diags) => {
                    arts.extend(Self::mcp(m, &ctx.mcp_servers, &mut diags)?);
                }
                Some(_) => {}
            }
        }
        if !ctx.permissions.rules.is_empty() {
            match &spec.permissions {
                None => missing("permissions", &mut diags),
                Some(p) if root_ok("permissions", &p.root, &mut diags) => {
                    arts.push(Self::permissions(&spec, p, cfg, &ctx.permissions, &mut diags)?);
                }
                Some(_) => {}
            }
        }
        if !ctx.memory.is_empty() {
            match &spec.memory {
                None => missing("memory", &mut diags),
                Some(m) if root_ok("memory", &m.root, &mut diags) => {
                    arts.extend(Self::memory(m, &ctx.memory, &mut diags)?);
                }
                Some(_) => {}
            }
        }
        arts.sort_by(|a, b| (&a.root, &a.path).cmp(&(&b.root, &b.path)));
        Ok(RenderResult {
            artifacts: arts,
            diagnostics: diags,
        })
    }

    /// Best effort: dir-per-skill skills, md_frontmatter agents and a single-file instruction.
    fn discover(&self, cfg: &ClientConfig, fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent> {
        let mut res = DiscoveredContent::default();
        let (spec, diags) = parse_spec(cfg.spec.as_ref());
        let Some(spec) = spec else {
            res.notes.extend(diags.into_iter().map(|d| d.message));
            return Ok(res);
        };
        match &spec.skills {
            Some(s) if s.layout == "dir_per_skill" && s.filename == "SKILL.md" => {
                res.skills = u::discover_skills(fs, cfg, &s.root, &s.path, &mut res.notes)?;
            }
            Some(_) => res
                .notes
                .push("skills discovery supports dir_per_skill with filename SKILL.md only".into()),
            None => {}
        }
        match &spec.agents {
            Some(a) if a.format == "md_frontmatter" => res.agents = Self::discover_agents(&spec, a, cfg, fs)?,
            Some(_) => res.notes.push("agent discovery supports md_frontmatter only".into()),
            None => {}
        }
        if let Some(i) = spec.instructions.as_ref().filter(|i| i.mode != "block")
            && let Some(p) = u::fs_path(cfg, &i.root, &i.file)?.filter(|p| !p.is_empty())
            && fs.exists(&p)
        {
            res.instructions.push(InstructionItem {
                id: "imported".into(),
                title: pyos::basename(&p).to_string(),
                order: 0,
                body: u::read_text(fs, &p, &mut Vec::new()),
            });
        }
        if spec.mcp.is_some() || spec.permissions.is_some() || spec.memory.is_some() {
            res.notes
                .push("mcp/permissions/memory discovery is not implemented for generic specs".into());
        }
        Ok(res)
    }

    fn verify(
        &self,
        cfg: &ClientConfig,
        expected: &[Expected],
        fs: &dyn FileSystem,
        _run: &dyn CommandRunner,
    ) -> AdapterResult<Vec<Check>> {
        u::verify_expected(cfg, expected, fs)
    }
}

/// The registry entry (Python: module attribute `ADAPTER`).
pub fn adapter() -> Option<Result<Arc<dyn ClientAdapter>, String>> {
    Some(Ok(Arc::new(GenericSpecAdapter)))
}
