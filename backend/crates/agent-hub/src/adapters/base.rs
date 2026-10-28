//! adapters/base.py: the client-adapter PORT. LOCKED: the core (planner, applier, API, UI) depends only on this module.
//! Adapters are PURE with respect to the client: `render` performs no I/O; `discover`/`verify`/`observe` go through
//! the injected [`FileSystem`] / [`CommandRunner`] ports. Owner: CORE (LOCKED). Field order of every struct is the
//! pydantic field order (it is the `model_dump()` key order the API returns).

use std::io;

use hub_core::value::PyValue;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

pub use hub_core::models::CAPABILITIES;

/// Bytes keyed by relative path, insertion-ordered like a Python dict.
pub type Files = IndexMap<String, Vec<u8>>;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SkillItem {
    pub name: String,
    pub description: String,
    /// relpath -> bytes; always contains "SKILL.md"
    pub files: Files,
    pub groups: Vec<String>,
    pub tags: Vec<String>,
    /// hub.yaml `source`, e.g. "import:video-agent"
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AgentItem {
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub model_tier: Option<String>,
    pub mode: String,
    pub read_only: bool,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct InstructionItem {
    pub id: String,
    pub title: String,
    pub order: i64,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct McpItem {
    pub name: String,
    /// "stdio" | "http" | "sse"
    pub transport: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    /// names only; VALUES never live in the hub
    pub env_names: Vec<String>,
    pub sandbox_profile: Option<String>,
    pub egress_hosts: Vec<String>,
    /// "unscanned" | "clean" | "findings"
    pub scan_status: String,
    pub pinned_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: String,
    /// "user" | "feedback" | "project" | "reference"
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub body: String,
}

/// One abstract permission/egress/approval rule after floor enforcement.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rule {
    /// "tool" | "command" | "path_read" | "path_write" | "egress_host" | "mcp_server"
    pub kind: String,
    #[serde(rename = "match")]
    pub matches: String,
    /// "allow" | "ask" | "deny"
    pub decision: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionSet {
    pub rules: Vec<Rule>,
    pub default_tool_decision: String,
    pub network_default: String,
}

impl Default for PermissionSet {
    fn default() -> Self {
        Self {
            rules: Vec::new(),
            default_tool_decision: "ask".into(),
            network_default: "deny".into(),
        }
    }
}

/// content/clients/<id>.yaml as the adapter sees it (roots already expanded by the core).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClientConfig {
    pub id: String,
    pub adapter: String,
    pub display_name: String,
    pub description: String,
    pub roots: IndexMap<String, String>,
    /// `dict[str, Any]` (a PyValue::Dict with str keys)
    pub params: PyValue,
    /// declarative spec for GenericSpecAdapter (a PyValue::Dict) or None
    pub spec: Option<PyValue>,
    pub strict: bool,
    pub manage: IndexMap<String, bool>,
    pub icon: Option<String>,
    pub color: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            adapter: String::new(),
            display_name: String::new(),
            description: String::new(),
            roots: IndexMap::new(),
            params: PyValue::Dict(Vec::new()),
            spec: None,
            strict: true,
            manage: IndexMap::new(),
            icon: None,
            color: None,
        }
    }
}

impl ClientConfig {
    /// `cfg.params.get(key)`.
    pub fn param(&self, key: &str) -> Option<&PyValue> {
        self.params.get(key)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RenderContext {
    pub client: ClientConfig,
    pub skills: Vec<SkillItem>,
    pub agents: Vec<AgentItem>,
    pub instructions: Vec<InstructionItem>,
    pub mcp_servers: Vec<McpItem>,
    pub memory: Vec<MemoryItem>,
    pub permissions: PermissionSet,
    /// named URLs from content/endpoints.yaml
    pub endpoints: IndexMap<String, String>,
}

/// One file (or one hub-managed slice of a file) an adapter wants delivered.
#[derive(Debug, Clone, PartialEq)]
pub struct Artifact {
    pub root: String,
    pub path: String,
    pub content: Vec<u8>,
    pub mode: u32,
    /// ItemKind: skill | agent | instruction | mcp | rule | memory
    pub kind: String,
    pub source_ids: Vec<String>,
    /// "own" | "json_keys" | "yaml_keys" | "block"
    pub merge: String,
    pub managed_keys: Vec<String>,
}

impl Artifact {
    /// An `own` artifact with mode 0o644 (the pydantic defaults).
    pub fn new(root: &str, path: &str, content: Vec<u8>, kind: &str) -> Self {
        Self {
            root: root.into(),
            path: path.into(),
            content,
            mode: 0o644,
            kind: kind.into(),
            source_ids: Vec::new(),
            merge: "own".into(),
            managed_keys: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Diag {
    /// "info" | "warn" | "error"
    pub severity: String,
    pub code: String,
    pub message: String,
    pub item: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RenderResult {
    pub artifacts: Vec<Artifact>,
    /// errors block apply when ClientConfig.strict
    pub diagnostics: Vec<Diag>,
}

/// What the client can consume (`caps().model_dump()` is served by GET /adapters).
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct AdapterCaps {
    pub skills: bool,
    pub agents: bool,
    pub instructions: bool,
    pub mcp: bool,
    pub permissions: bool,
    pub egress: bool,
    pub memory: bool,
    /// canonical capability -> native tool name; None = unsupported
    pub tool_map: IndexMap<String, Option<String>>,
    /// tier -> native model id
    pub model_tiers: IndexMap<String, String>,
    pub notes: String,
}

/// Reverse direction: what the client already has natively (used by import).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DiscoveredContent {
    pub skills: Vec<SkillItem>,
    pub agents: Vec<AgentItem>,
    pub instructions: Vec<InstructionItem>,
    pub mcp_servers: Vec<McpItem>,
    pub permissions: Option<PermissionSet>,
    pub notes: Vec<String>,
    /// merged by the core into the client's params on import when not already set (a PyValue::Dict)
    pub suggested_params: Vec<(String, PyValue)>,
}

pub const OBSERVED_ORIGINS: [&str; 6] = ["client", "shipped", "agent", "lab_sync", "hub", "archived"];

/// One skill a client REALLY has, as read by `observe` (Addendum F). Untrusted data.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ObservedSkill {
    /// relative to the client's skills root, posix ("devops/foo" or "foo")
    pub path: String,
    pub name: String,
    pub description: String,
    /// one of OBSERVED_ORIGINS
    pub origin: String,
    pub files: Files,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub skills: Vec<ObservedSkill>,
    pub source: String,
    pub notes: Vec<String>,
    /// False when a cap truncated the read or a source could not be reached
    pub complete: bool,
}

impl Default for Observation {
    fn default() -> Self {
        Self {
            skills: Vec::new(),
            source: String::new(),
            notes: Vec::new(),
            complete: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Expected {
    pub root: String,
    pub path: String,
    pub sha256: String,
    pub kind: String,
    pub source_ids: Vec<String>,
    pub merge: String,
    pub managed_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Read-only file access for discover/verify/observe (fakes in tests, `services::fs::RealFs` for real).
pub trait FileSystem: Send + Sync {
    fn read_bytes(&self, path: &str) -> io::Result<Vec<u8>>;
    fn exists(&self, path: &str) -> bool;
    fn listdir(&self, path: &str) -> io::Result<Vec<String>>;
    fn is_dir(&self, path: &str) -> bool;
    /// stat only, so callers can cap BEFORE reading (`None` = the port cannot tell; Python's optional `size`)
    fn size(&self, path: &str) -> Option<io::Result<u64>>;
}

/// argv-only command execution (never a shell). Returns (rc, stdout, stderr); `Err` where Python's runner raised OSError.
pub trait CommandRunner: Send + Sync {
    fn run(&self, argv: &[String], timeout: f64) -> io::Result<(i32, String, String)>;
}

/// An exception escaping an adapter method: Python callers report `type(exc).__name__` (and sometimes `str(exc)`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{py_type}: {message}")]
pub struct AdapterError {
    pub py_type: String,
    pub message: String,
}

impl AdapterError {
    pub fn new(py_type: &str, message: impl Into<String>) -> Self {
        Self {
            py_type: py_type.into(),
            message: message.into(),
        }
    }
    /// What a phase-1 stub raises.
    pub fn not_ported(module: &str) -> Self {
        Self::new("NotImplementedError", format!("not ported: {module}"))
    }
}

pub type AdapterResult<T> = Result<T, AdapterError>;

/// The port. Supporting a new client = a declarative generic spec OR one new file under src/adapters/ implementing
/// this trait and exposing `pub fn adapter() -> Option<Result<Arc<dyn ClientAdapter>, String>>` (see adapters/mod.rs).
pub trait ClientAdapter: Send + Sync {
    /// e.g. "claude-code"
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    /// The Python class docstring, `.strip()`ped (GET /adapters `docs`, first 4000 characters are served).
    fn docs(&self) -> &str;
    fn caps(&self) -> AdapterCaps;
    /// A starter ClientConfig (roots as documented defaults) used by the UI's 'add client' wizard.
    fn default_config(&self) -> ClientConfig;
    /// PURE. Map resolved content to native artifacts; emit Diag for anything unsupported or lossy.
    fn render(&self, ctx: &RenderContext) -> AdapterResult<RenderResult>;
    /// Read what the client natively has, for import. Read-only.
    fn discover(&self, cfg: &ClientConfig, fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent>;
    /// Prove the client actually sees what was delivered.
    fn verify(
        &self,
        cfg: &ClientConfig,
        expected: &[Expected],
        fs: &dyn FileSystem,
        run: &dyn CommandRunner,
    ) -> AdapterResult<Vec<Check>>;
    /// Addendum F: every skill the client really has. Default: what discover() finds, origin `client`.
    fn observe(&self, cfg: &ClientConfig, fs: &dyn FileSystem, _run: &dyn CommandRunner) -> AdapterResult<Observation> {
        let dc = self.discover(cfg, fs)?;
        Ok(Observation {
            skills: dc
                .skills
                .into_iter()
                .map(|s| ObservedSkill {
                    path: s.name.clone(),
                    name: s.name,
                    description: s.description,
                    origin: "client".into(),
                    files: s.files,
                })
                .collect(),
            source: "the client's skill sources (discover)".into(),
            notes: dc.notes,
            complete: true,
        })
    }
}
