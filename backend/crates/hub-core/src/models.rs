//! domain/models.py: content document schemas (validated with [`crate::pyd`], messages identical to pydantic) and
//! the typed views the services use. Every content model forbids unknown keys except `SkillMeta` (SKILL.md is the
//! client-owned standard: unknown frontmatter keys are kept verbatim, in order).
//! Owner: CORE (LOCKED). Result models (Finding, PlanFile, ClientPlan, Plan) are plain serde structs whose field order
//! is the pydantic field order, so `serde_json::to_value` + `py::json` reproduce `model_dump(_json)`.

use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{CONCERNS, ENV_NAME_RE, HOST_RE, IDENT_MESSAGE, valid_id};
use crate::pyd::{self, Extra, Field, Lit, Model, Ty, ValidationError, optional, required};
use crate::value::PyValue;

/// adapters/base.py CAPABILITIES (canonical tool vocabulary), in canonical order.
pub const CAPABILITIES: [&str; 12] = [
    "read",
    "write",
    "edit",
    "shell",
    "web_fetch",
    "web_search",
    "mcp",
    "subagent",
    "browser",
    "notebook",
    "todo",
    "image",
];

pub const KIND_LITS: &[Lit] = &[
    Lit::S("skill"),
    Lit::S("agent"),
    Lit::S("instruction"),
    Lit::S("mcp"),
    Lit::S("rule"),
    Lit::S("memory"),
];
const MEMORY_TYPES: &[Lit] = &[
    Lit::S("user"),
    Lit::S("feedback"),
    Lit::S("project"),
    Lit::S("reference"),
];

fn check_ident(v: &PyValue) -> Result<(), String> {
    match v.as_str() {
        Some(s) if valid_id(s) => Ok(()),
        _ => Err(IDENT_MESSAGE.into()),
    }
}

/// `Ident = Annotated[str, AfterValidator(check_ident)]`
pub fn ident() -> Ty {
    Ty::after(Ty::str(), check_ident)
}

fn short_check(v: &PyValue) -> Result<(), String> {
    match v.as_str() {
        Some(s) if s.chars().count() > 300 => Err("longer than 300 characters".into()),
        _ => Ok(()),
    }
}

/// `ShortStr = Annotated[str, AfterValidator(_short)]` (300 characters).
pub fn short_str() -> Ty {
    Ty::after(Ty::str(), short_check)
}

fn sized(min: Option<usize>, max: Option<usize>) -> Ty {
    Ty::Str {
        min,
        max,
        pattern: None,
    }
}

fn empty_list() -> PyValue {
    PyValue::List(Vec::new())
}
fn empty_dict() -> PyValue {
    PyValue::Dict(Vec::new())
}
fn empty_str() -> PyValue {
    PyValue::str("")
}
fn none() -> PyValue {
    PyValue::None
}
fn yes() -> PyValue {
    PyValue::Bool(true)
}
fn no() -> PyValue {
    PyValue::Bool(false)
}

macro_rules! model {
    ($static:ident, $getter:ident, $model:expr) => {
        pub static $static: LazyLock<Model> = LazyLock::new(|| $model);
        pub fn $getter() -> &'static Model {
            &$static
        }
    };
}

model!(
    HUB_META,
    hub_meta,
    Model {
        name: "HubMeta",
        fields: vec![
            optional("version", Ty::Lit(&[Lit::I(1)]), || PyValue::Int(1)),
            optional("name", short_str(), || PyValue::str("agent-hub-content")),
        ],
        extra: Extra::Forbid,
        check: None,
        transform: None,
    }
);

model!(
    SKILL_META,
    skill_meta,
    Model {
        name: "SkillMeta",
        fields: vec![
            required("name", ident()),
            required("description", sized(Some(1), Some(1024)))
        ],
        extra: Extra::Allow,
        check: None,
        transform: None,
    }
);

model!(
    SIDECAR,
    sidecar,
    Model {
        name: "Sidecar",
        fields: vec![
            optional("groups", Ty::list(ident()), empty_list),
            optional("tags", Ty::list(ident()), empty_list),
            optional("notes", sized(None, Some(2000)), empty_str),
            optional("source", short_str(), empty_str),
            optional("provenance", Ty::dict(Ty::str(), Ty::Any), empty_dict),
        ],
        extra: Extra::Forbid,
        check: None,
        transform: None,
    }
);

model!(
    INSTRUCTION_META,
    instruction_meta,
    Model {
        name: "InstructionMeta",
        fields: vec![
            required("id", ident()),
            required("title", short_str()),
            optional(
                "order",
                Ty::Int {
                    ge: Some(-100_000),
                    le: Some(100_000)
                },
                || PyValue::Int(100)
            ),
            optional("applies_to", Ty::list(ident()), empty_list),
            optional("tags", Ty::list(ident()), empty_list),
        ],
        extra: Extra::Forbid,
        check: None,
        transform: None,
    }
);

fn caps_check(v: &PyValue) -> Result<(), String> {
    let bad = v
        .as_list()
        .unwrap_or_default()
        .iter()
        .filter(|c| !c.as_str().is_some_and(|s| CAPABILITIES.contains(&s)))
        .count();
    if bad == 0 {
        return Ok(());
    }
    let mut sorted: Vec<&str> = CAPABILITIES.to_vec();
    sorted.sort_unstable();
    let list = sorted
        .iter()
        .map(|s| crate::py::text::repr(s))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!("{bad} unknown capability name(s); allowed: [{list}]"))
}

model!(
    AGENT_META,
    agent_meta,
    Model {
        name: "AgentMeta",
        fields: vec![
            required("name", ident()),
            required("description", sized(Some(1), Some(1024))),
            Field {
                name: "capabilities",
                ty: Ty::list(Ty::str()),
                default: Some(empty_list),
                check: Some(caps_check)
            },
            optional(
                "model_tier",
                Ty::opt(Ty::Lit(&[Lit::S("fast"), Lit::S("standard"), Lit::S("deep")])),
                none
            ),
            optional("mode", Ty::Lit(&[Lit::S("subagent"), Lit::S("primary")]), || {
                PyValue::str("subagent")
            }),
            optional("read_only", Ty::Bool, no),
            optional("tags", Ty::list(ident()), empty_list),
        ],
        extra: Extra::Forbid,
        check: None,
        transform: None,
    }
);

fn env_check(v: &PyValue) -> Result<(), String> {
    for n in v.as_list().unwrap_or_default() {
        if !n.as_str().is_some_and(|s| ENV_NAME_RE.is_match(s)) {
            return Err("env var NAMES expected, like API_TOKEN (values never live in the hub)".into());
        }
    }
    Ok(())
}

fn hosts_check(v: &PyValue) -> Result<(), String> {
    for h in v.as_list().unwrap_or_default() {
        if !h.as_str().is_some_and(|s| HOST_RE.is_match(s)) {
            return Err("invalid host pattern".into());
        }
    }
    Ok(())
}

fn mcp_shape(v: &PyValue) -> Result<(), String> {
    let transport = v.get("transport").and_then(PyValue::as_str).unwrap_or("");
    let command = v.get("command").and_then(PyValue::as_str).unwrap_or("");
    let url = v.get("url").and_then(PyValue::as_str).unwrap_or("");
    if transport == "stdio" && command.is_empty() {
        return Err("stdio transport needs `command`".into());
    }
    if transport != "stdio" && url.is_empty() {
        return Err(format!("{transport} transport needs `url`"));
    }
    if !url.is_empty() && !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("url must be http(s)".into());
    }
    Ok(())
}

model!(
    MCP_DOC,
    mcp_doc,
    Model {
        name: "McpDoc",
        fields: vec![
            required("name", ident()),
            required("transport", Ty::Lit(&[Lit::S("stdio"), Lit::S("http"), Lit::S("sse")])),
            optional("command", Ty::opt(short_str()), none),
            optional("args", Ty::list(short_str()), empty_list),
            optional("url", Ty::opt(short_str()), none),
            Field {
                name: "env_names",
                ty: Ty::list(Ty::str()),
                default: Some(empty_list),
                check: Some(env_check)
            },
            optional("sandbox_profile", Ty::opt(short_str()), none),
            Field {
                name: "egress_hosts",
                ty: Ty::list(Ty::str()),
                default: Some(empty_list),
                check: Some(hosts_check)
            },
            optional(
                "scan_status",
                Ty::Lit(&[Lit::S("unscanned"), Lit::S("clean"), Lit::S("findings")]),
                || PyValue::str("unscanned")
            ),
            optional("pinned_ref", Ty::opt(short_str()), none),
            optional("scan_digest", Ty::opt(Ty::str()), none),
            optional("groups", Ty::list(ident()), empty_list),
            optional("tags", Ty::list(ident()), empty_list),
            optional("notes", sized(None, Some(2000)), empty_str),
        ],
        extra: Extra::Forbid,
        check: Some(mcp_shape),
        transform: None,
    }
);

fn no_control(v: &PyValue) -> Result<(), String> {
    match v.as_str() {
        Some(s) if s.chars().any(|c| (c as u32) < 32) => Err("control characters not allowed".into()),
        _ => Ok(()),
    }
}

model!(
    RULE_DOC,
    rule_doc,
    Model {
        name: "RuleDoc",
        fields: vec![
            required("id", ident()),
            required("title", short_str()),
            required(
                "kind",
                Ty::Lit(&[
                    Lit::S("tool"),
                    Lit::S("command"),
                    Lit::S("path_read"),
                    Lit::S("path_write"),
                    Lit::S("egress_host"),
                    Lit::S("mcp_server"),
                ]),
            ),
            Field {
                name: "match",
                ty: sized(Some(1), Some(300)),
                default: None,
                check: Some(no_control)
            },
            required("decision", Ty::Lit(&[Lit::S("allow"), Lit::S("ask"), Lit::S("deny")])),
            optional("reason", Ty::opt(short_str()), none),
            optional("applies_to", Ty::list(ident()), empty_list),
            optional("tags", Ty::list(ident()), empty_list),
        ],
        extra: Extra::Forbid,
        check: None,
        transform: None,
    }
);

model!(
    MEMORY_META,
    memory_meta,
    Model {
        name: "MemoryMeta",
        fields: vec![
            required("id", ident()),
            required("type", Ty::Lit(MEMORY_TYPES)),
            required("title", short_str()),
            optional("tags", Ty::list(ident()), empty_list),
        ],
        extra: Extra::Forbid,
        check: None,
        transform: None,
    }
);

model!(
    INBOX_META,
    inbox_meta,
    Model {
        name: "InboxMeta",
        fields: vec![
            required("id", ident()),
            required("type", Ty::Lit(MEMORY_TYPES)),
            required("title", short_str()),
            required("client", ident()),
            optional("created", Ty::Float, || PyValue::Float(0.0)),
        ],
        extra: Extra::Forbid,
        check: None,
        transform: None,
    }
);

model!(
    MEMBER,
    member,
    Model {
        name: "Member",
        fields: vec![required("kind", Ty::Lit(KIND_LITS)), required("name", ident())],
        extra: Extra::Forbid,
        check: None,
        transform: None,
    }
);

fn dedupe_members(v: PyValue) -> PyValue {
    let PyValue::Dict(mut fields) = v else { return v };
    for (k, val) in &mut fields {
        if k.as_str() == Some("members")
            && let PyValue::List(items) = val
        {
            let mut seen: Vec<(String, String)> = Vec::new();
            items.retain(|m| {
                let key = (
                    m.get("kind").map(PyValue::py_str).unwrap_or_default(),
                    m.get("name").map(PyValue::py_str).unwrap_or_default(),
                );
                if seen.contains(&key) {
                    false
                } else {
                    seen.push(key);
                    true
                }
            });
        }
    }
    PyValue::Dict(fields)
}

model!(
    COLLECTION_DOC,
    collection_doc,
    Model {
        name: "CollectionDoc",
        fields: vec![
            required("id", ident()),
            required("title", short_str()),
            optional("description", sized(None, Some(2000)), empty_str),
            optional("icon", sized(None, Some(16)), empty_str),
            optional(
                "color",
                Ty::Str {
                    min: None,
                    max: Some(32),
                    pattern: Some("^[#A-Za-z0-9-]*$")
                },
                empty_str
            ),
            optional("order", Ty::int(), || PyValue::Int(100)),
            optional("members", Ty::list(Ty::Model(member)), empty_list),
        ],
        extra: Extra::Forbid,
        check: None,
        transform: Some(dedupe_members),
    }
);

fn manage_check(v: &PyValue) -> Result<(), String> {
    let bad = v
        .as_dict()
        .unwrap_or_default()
        .iter()
        .any(|(k, _)| !k.as_str().is_some_and(|s| CONCERNS.contains(&s)));
    if bad {
        let list = CONCERNS
            .iter()
            .map(|s| crate::py::text::repr(s))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!("unknown concern name(s); allowed: [{list}]"));
    }
    Ok(())
}

fn client_spec(v: &PyValue) -> Result<(), String> {
    let adapter = v.get("adapter").and_then(PyValue::as_str).unwrap_or("");
    let spec_empty = match v.get("spec") {
        None | Some(PyValue::None) => true,
        Some(PyValue::Dict(d)) => d.is_empty(),
        Some(_) => false,
    };
    if adapter == "generic" && spec_empty {
        return Err("adapter 'generic' requires `spec`".into());
    }
    Ok(())
}

model!(
    CLIENT_DOC,
    client_doc,
    Model {
        name: "ClientDoc",
        fields: vec![
            required("id", ident()),
            required("adapter", ident()),
            required("display_name", short_str()),
            optional("description", short_str(), empty_str),
            optional("roots", Ty::dict(ident(), short_str()), empty_dict),
            optional("params", Ty::dict(Ty::str(), Ty::Any), empty_dict),
            optional("spec", Ty::opt(Ty::dict(Ty::str(), Ty::Any)), none),
            optional("strict", Ty::Bool, yes),
            optional("icon", Ty::opt(short_str()), none),
            optional("color", Ty::opt(short_str()), none),
            Field {
                name: "manage",
                ty: Ty::dict(Ty::str(), Ty::Bool),
                default: Some(empty_dict),
                check: Some(manage_check)
            },
        ],
        extra: Extra::Forbid,
        check: Some(client_spec),
        transform: None,
    }
);

model!(
    SELECTION,
    selection,
    Model {
        name: "Selection",
        fields: ["skills", "agents", "mcp", "instructions", "rules", "memory"]
            .into_iter()
            .map(|n| optional(n, Ty::list(ident()), empty_list))
            .collect(),
        extra: Extra::Forbid,
        check: None,
        transform: None,
    }
);

fn empty_selection() -> PyValue {
    PyValue::Dict(
        ["skills", "agents", "mcp", "instructions", "rules", "memory"]
            .iter()
            .map(|n| (PyValue::str(*n), empty_list()))
            .collect(),
    )
}

model!(
    PROFILE_DOC,
    profile_doc,
    Model {
        name: "ProfileDoc",
        fields: vec![
            optional("collections", Ty::list(ident()), empty_list),
            optional("enable", Ty::Model(selection), empty_selection),
            optional("disable", Ty::Model(selection), empty_selection),
        ],
        extra: Extra::Forbid,
        check: None,
        transform: None,
    }
);

/// Model lookup by pydantic class name (golden tests, generic callers).
pub fn by_name(name: &str) -> Option<&'static Model> {
    Some(match name {
        "HubMeta" => hub_meta(),
        "SkillMeta" => skill_meta(),
        "Sidecar" => sidecar(),
        "InstructionMeta" => instruction_meta(),
        "AgentMeta" => agent_meta(),
        "McpDoc" => mcp_doc(),
        "RuleDoc" => rule_doc(),
        "MemoryMeta" => memory_meta(),
        "InboxMeta" => inbox_meta(),
        "Member" => member(),
        "CollectionDoc" => collection_doc(),
        "ClientDoc" => client_doc(),
        "Selection" => selection(),
        "ProfileDoc" => profile_doc(),
        _ => return None,
    })
}

/// Typed view of a validated `ClientDoc` (field order = pydantic's).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClientDoc {
    pub id: String,
    pub adapter: String,
    pub display_name: String,
    pub description: String,
    pub roots: indexmap::IndexMap<String, String>,
    /// `dict[str, Any]`
    pub params: PyValue,
    /// `dict[str, Any] | None`
    pub spec: Option<PyValue>,
    pub strict: bool,
    pub icon: Option<String>,
    pub color: Option<String>,
    pub manage: indexmap::IndexMap<String, bool>,
}

fn sget(v: &PyValue, k: &str) -> String {
    v.get(k).and_then(PyValue::as_str).unwrap_or_default().to_string()
}
fn oget(v: &PyValue, k: &str) -> Option<String> {
    v.get(k).and_then(PyValue::as_str).map(str::to_string)
}
fn lget(v: &PyValue, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(PyValue::as_list)
        .map(|l| l.iter().map(PyValue::py_str).collect())
        .unwrap_or_default()
}

impl ClientDoc {
    /// From the output of `pyd::validate(client_doc(), ..)`.
    pub fn from_validated(v: &PyValue) -> Self {
        let roots = v
            .get("roots")
            .and_then(PyValue::as_dict)
            .map(|d| d.iter().map(|(k, x)| (k.py_str(), x.py_str())).collect())
            .unwrap_or_default();
        let manage = v
            .get("manage")
            .and_then(PyValue::as_dict)
            .map(|d| {
                d.iter()
                    .map(|(k, x)| (k.py_str(), matches!(x, PyValue::Bool(true))))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            id: sget(v, "id"),
            adapter: sget(v, "adapter"),
            display_name: sget(v, "display_name"),
            description: sget(v, "description"),
            roots,
            params: v.get("params").cloned().unwrap_or(PyValue::Dict(Vec::new())),
            spec: v.get("spec").filter(|x| !x.is_none()).cloned(),
            strict: !matches!(v.get("strict"), Some(PyValue::Bool(false))),
            icon: oget(v, "icon"),
            color: oget(v, "color"),
            manage,
        }
    }

    /// `model_dump(mode="json", exclude_none=...)` (key order = field order).
    pub fn dump(&self, exclude_none: bool) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        let opt = |m: &mut serde_json::Map<String, Value>, k: &str, v: Option<Value>| match v {
            Some(v) => {
                m.insert(k.into(), v);
            }
            None if !exclude_none => {
                m.insert(k.into(), Value::Null);
            }
            None => {}
        };
        m.insert("id".into(), Value::String(self.id.clone()));
        m.insert("adapter".into(), Value::String(self.adapter.clone()));
        m.insert("display_name".into(), Value::String(self.display_name.clone()));
        m.insert("description".into(), Value::String(self.description.clone()));
        m.insert("roots".into(), serde_json::to_value(&self.roots).unwrap_or(Value::Null));
        m.insert("params".into(), self.params.to_json());
        opt(&mut m, "spec", self.spec.as_ref().map(PyValue::to_json));
        m.insert("strict".into(), Value::Bool(self.strict));
        opt(&mut m, "icon", self.icon.clone().map(Value::String));
        opt(&mut m, "color", self.color.clone().map(Value::String));
        m.insert(
            "manage".into(),
            serde_json::to_value(&self.manage).unwrap_or(Value::Null),
        );
        Value::Object(m)
    }
}

/// `Member` (kind, name).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Member {
    pub kind: String,
    pub name: String,
}

/// Typed view of a validated `CollectionDoc`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CollectionDoc {
    pub id: String,
    pub title: String,
    pub description: String,
    pub icon: String,
    pub color: String,
    pub order: i64,
    pub members: Vec<Member>,
}

impl CollectionDoc {
    pub fn from_validated(v: &PyValue) -> Self {
        Self {
            id: sget(v, "id"),
            title: sget(v, "title"),
            description: sget(v, "description"),
            icon: sget(v, "icon"),
            color: sget(v, "color"),
            order: if let Some(PyValue::Int(i)) = v.get("order") {
                *i
            } else {
                100
            },
            members: v
                .get("members")
                .and_then(PyValue::as_list)
                .map(|l| {
                    l.iter()
                        .map(|m| Member {
                            kind: sget(m, "kind"),
                            name: sget(m, "name"),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// `Selection` (per plural kind, the names).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Selection {
    pub skills: Vec<String>,
    pub agents: Vec<String>,
    pub mcp: Vec<String>,
    pub instructions: Vec<String>,
    pub rules: Vec<String>,
    pub memory: Vec<String>,
}

impl Selection {
    pub fn from_validated(v: &PyValue) -> Self {
        Self {
            skills: lget(v, "skills"),
            agents: lget(v, "agents"),
            mcp: lget(v, "mcp"),
            instructions: lget(v, "instructions"),
            rules: lget(v, "rules"),
            memory: lget(v, "memory"),
        }
    }
    /// `getattr(selection, plural)`.
    pub fn get(&self, plural: &str) -> &[String] {
        match plural {
            "skills" => &self.skills,
            "agents" => &self.agents,
            "mcp" => &self.mcp,
            "instructions" => &self.instructions,
            "rules" => &self.rules,
            "memory" => &self.memory,
            _ => &[],
        }
    }
}

/// Typed view of a validated `ProfileDoc`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProfileDoc {
    pub collections: Vec<String>,
    pub enable: Selection,
    pub disable: Selection,
}

impl ProfileDoc {
    pub fn from_validated(v: &PyValue) -> Self {
        Self {
            collections: lget(v, "collections"),
            enable: v.get("enable").map(Selection::from_validated).unwrap_or_default(),
            disable: v.get("disable").map(Selection::from_validated).unwrap_or_default(),
        }
    }
}

/// `Issue` (path, message, fix_hint).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub path: String,
    pub message: String,
    pub fix_hint: Option<String>,
}

impl Issue {
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
            fix_hint: None,
        }
    }
}

/// `issues_from(exc, path)`: one Issue per error; path = the given one, else the dotted loc.
pub fn issues_from(exc: &ValidationError, path: &str) -> Vec<Issue> {
    exc.errors
        .iter()
        .map(|e| {
            Issue::new(
                if path.is_empty() {
                    pyd::loc_str(&e.loc)
                } else {
                    path.to_string()
                },
                e.msg.clone(),
            )
        })
        .collect()
}

/// `Finding` (floor/audit finding).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub code: String,
    pub severity: String,
    pub message: String,
    pub item: Option<String>,
    pub concern: Option<String>,
    pub rule: Option<String>,
}

/// `PlanFile`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanFile {
    pub root: String,
    pub path: String,
    pub action: String,
    pub kind: String,
    #[serde(default)]
    pub source_ids: Vec<String>,
    #[serde(default)]
    pub diff: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default = "own")]
    pub merge: String,
    #[serde(default = "yes_bool")]
    pub managed: bool,
}

fn own() -> String {
    "own".into()
}
fn yes_bool() -> bool {
    true
}

/// `ClientPlan`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ClientPlan {
    pub client: String,
    #[serde(default)]
    pub files: Vec<PlanFile>,
    #[serde(default)]
    pub diagnostics: Vec<Value>,
    #[serde(default)]
    pub floor_violations: Vec<Finding>,
    /// `dict[str, int]`, insertion-ordered.
    #[serde(default)]
    pub summary: serde_json::Map<String, Value>,
    #[serde(default)]
    pub blocked: bool,
    #[serde(default)]
    pub blocked_reasons: Vec<String>,
    #[serde(default)]
    pub digest: String,
}

/// `Plan` (written to <data>/plans/<id>.json with `model_dump_json()`; read back with `model_validate_json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub content_hash: String,
    pub created_at: f64,
    pub clients: Vec<ClientPlan>,
    #[serde(default)]
    pub pending_changes: i64,
    #[serde(default)]
    pub preview: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_message_is_sorted_repr_list() {
        let v = PyValue::List(vec![PyValue::str("fly")]);
        let m = caps_check(&v).err().unwrap_or_default();
        assert!(m.starts_with("1 unknown capability name(s); allowed: ['browser', 'edit',"));
    }
}
