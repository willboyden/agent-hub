//! Port of backend/src/agent_hub/adapters/opencode.py: the opencode adapter (id `opencode`). Owner: WS-A.
//!
//!   agents        .opencode/agent/<name>.md   frontmatter: description, mode, temperature, tools{...}, permission{...}
//!   instructions  AGENTS.md                   merge=block (hub markers)
//!   opencode.json merge=json_keys on `instructions` / `mcp` / `permission`   ADVISORY by default
//!   skills        NONE by default; param `skills_via_claude_dir` renders them to .claude/skills

use std::sync::Arc;

use hub_core::py::text;
use hub_core::value::PyValue;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::_util as u;
use super::base::{
    AdapterCaps, AdapterResult, AgentItem, Artifact, Check, ClientAdapter, ClientConfig, CommandRunner, Diag,
    DiscoveredContent, Expected, FileSystem, InstructionItem, McpItem, PermissionSet, RenderContext, RenderResult,
    Rule,
};
use super::claude_code::{agent_item, flags_value, mcp_item};

pub const ID: &str = "opencode";

pub const TOOL_MAP: [(&str, Option<&str>); 12] = [
    ("read", Some("read")),
    ("write", Some("write")),
    ("edit", Some("edit")),
    ("shell", Some("bash")),
    ("web_fetch", Some("webfetch")),
    ("web_search", None),
    ("mcp", None),
    ("subagent", Some("task")),
    ("browser", None),
    ("notebook", None),
    ("todo", Some("todowrite")),
    ("image", None),
];
/// tools opencode enables by default; an agent that lacks the capability must switch the tool OFF explicitly.
const GATED: [&str; 7] = ["read", "write", "edit", "shell", "web_fetch", "subagent", "todo"];
pub const DEFAULT_MANAGE: [(&str, bool); 6] = [
    ("skills", false),
    ("agents", true),
    ("instructions", true),
    ("mcp", false),
    ("permissions", false),
    ("memory", false),
];

fn tool_of(cap: &str) -> Option<Option<&'static str>> {
    TOOL_MAP.iter().find(|(c, _)| *c == cap).map(|(_, t)| *t)
}

/// `OpenCodeAdapter`.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenCodeAdapter;

/// `_tiers(cfg)`: `{str(k): str(v) for k, v in params.model_tiers.items()}`.
fn tiers(cfg: &ClientConfig) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    if let Some(PyValue::Dict(d)) = cfg.param("model_tiers") {
        for (k, v) in d {
            out.insert(k.py_str(), v.py_str());
        }
    }
    out
}

fn json_art(kind: &str, doc: &Value, keys: &[&str], ids: Vec<String>) -> Artifact {
    let mut a = Artifact::new("project", "opencode.json", u::dump_json(doc), kind);
    a.source_ids = ids;
    a.merge = "json_keys".into();
    a.managed_keys = keys.iter().map(|k| (*k).to_string()).collect();
    a
}

impl OpenCodeAdapter {
    fn agent(ag: &AgentItem, cfg: &ClientConfig, root: &str, diags: &mut Vec<Diag>) -> Option<Artifact> {
        let name = ag.name.as_str();
        if !u::valid_name(name) {
            diags.push(u::diag(
                "error",
                "invalid_name",
                format!("agent name {} is not a valid identifier", text::repr(name)),
                Some(name),
            ));
            return None;
        }
        if !u::check_description(&ag.description, name, diags) {
            return None;
        }
        if text::char_len(&ag.body) > u::MAX_BODY_CHARS {
            diags.push(u::diag("error", "body_too_large", "agent body exceeds the size cap", Some(name)));
            return None;
        }
        for c in u::unknown_caps(&ag.capabilities) {
            diags.push(u::diag(
                "error",
                "unknown_capability",
                format!("unknown capability {}", text::repr(&c)),
                Some(name),
            ));
        }
        let caps = u::effective_caps(
            ag.capabilities.iter().map(String::as_str).filter(|c| tool_of(c).is_some()),
            ag.read_only,
        );
        for c in &caps {
            if c == "mcp" {
                diags.push(u::diag(
                    "info",
                    "mcp_not_gated",
                    "opencode does not gate MCP per agent; access follows opencode.json mcp",
                    Some(name),
                ));
            } else if tool_of(c) == Some(None) {
                diags.push(u::diag(
                    u::unsupported_severity(cfg),
                    "unsupported_capability",
                    format!("capability {} has no opencode tool", text::repr(c)),
                    Some(name),
                ));
            }
        }
        let mut tools: Vec<(String, bool)> = Vec::new();
        for c in GATED {
            if let Some(Some(native)) = tool_of(c)
                && !caps.iter().any(|x| x == c)
            {
                tools.push((native.to_string(), false));
            }
        }
        let mut perm: Vec<(&str, &str)> = Vec::new();
        if ag.read_only {
            perm.push(("edit", "deny"));
        }
        if caps.iter().any(|c| c == "shell") {
            perm.push(("bash", "ask"));
        }
        if caps.iter().any(|c| c == "web_fetch") {
            perm.push(("webfetch", "ask"));
        }
        #[allow(clippy::cast_precision_loss)]
        let temp = match cfg.param("temperature") {
            Some(PyValue::Float(f)) => *f,
            Some(PyValue::Int(i)) => *i as f64,
            Some(PyValue::Bool(b)) => f64::from(u8::from(*b)),
            Some(PyValue::BigInt(s)) => s.parse::<f64>().unwrap_or(f64::INFINITY),
            None | Some(_) => 0.1,
        };
        let mut fm: Vec<(PyValue, PyValue)> = vec![
            (PyValue::str("description"), PyValue::str(u::oneline(&ag.description, name, diags))),
            (PyValue::str("mode"), PyValue::str(ag.mode.clone())),
            (PyValue::str("temperature"), PyValue::Float(temp)),
        ];
        if let Some(tier) = &ag.model_tier {
            match tiers(cfg).get(tier).filter(|m| !m.is_empty()) {
                Some(model) => fm.push((PyValue::str("model"), PyValue::str(model.clone()))),
                None => diags.push(u::diag(
                    "info",
                    "no_model_mapping",
                    format!(
                        "no model mapped for tier {}; agent inherits the default model",
                        text::repr(tier)
                    ),
                    Some(name),
                )),
            }
        }
        if !tools.is_empty() {
            tools.sort();
            fm.push((
                PyValue::str("tools"),
                PyValue::Dict(tools.into_iter().map(|(k, v)| (PyValue::str(k), PyValue::Bool(v))).collect()),
            ));
        }
        if !perm.is_empty() {
            perm.sort_unstable();
            fm.push((
                PyValue::str("permission"),
                PyValue::Dict(perm.into_iter().map(|(k, v)| (PyValue::str(k), PyValue::str(v))).collect()),
            ));
        }
        let path = u::safe_relpath(&[".opencode/agent", &format!("{name}.md")]).ok()?;
        let content = u::render_frontmatter(&PyValue::Dict(fm), &ag.body);
        let mut a = Artifact::new(root, &path, u::to_bytes(&content), "agent");
        a.source_ids = vec![name.to_string()];
        Some(a)
    }

    fn mcp(ctx: &RenderContext, diags: &mut Vec<Diag>) -> Map<String, Value> {
        let wrapper = u::sandbox_prefix(&ctx.client.params);
        let mut out = Map::new();
        let mut sorted: Vec<&McpItem> = ctx.mcp_servers.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for m in sorted {
            let name = m.name.as_str();
            if !u::valid_name(name) {
                diags.push(u::diag(
                    "error",
                    "invalid_name",
                    format!("mcp name {} is not valid", text::repr(name)),
                    Some(name),
                ));
                continue;
            }
            if m.scan_status != "clean" {
                diags.push(u::diag(
                    "info",
                    "mcp_not_clean",
                    format!("scan_status={}; the core blocks enabling unscanned servers", m.scan_status),
                    Some(name),
                ));
            }
            let e = if m.transport == "stdio" {
                let Some(command) = m.command.as_deref().filter(|c| !c.is_empty()) else {
                    diags.push(u::diag("error", "mcp_no_command", "stdio server without a command", Some(name)));
                    continue;
                };
                let mut argv: Vec<String> = vec![command.to_string()];
                argv.extend(m.args.iter().cloned());
                if m.sandbox_profile.as_deref().is_some_and(|p| !p.is_empty()) {
                    if wrapper.is_empty() {
                        u::sandbox_missing(name, diags);
                        continue;
                    }
                    argv = wrapper.iter().cloned().chain(argv).collect();
                }
                let mut e = Map::new();
                e.insert("type".into(), Value::from("local"));
                e.insert("enabled".into(), Value::Bool(true));
                e.insert("command".into(), Value::from(argv));
                if !m.env_names.is_empty() {
                    let mut names = m.env_names.clone();
                    names.sort();
                    let env: Map<String, Value> = names
                        .into_iter()
                        .map(|n| {
                            let v = format!("{{env:{n}}}");
                            (n, Value::from(v))
                        })
                        .collect();
                    e.insert("environment".into(), Value::Object(env));
                }
                Value::Object(e)
            } else {
                let Some(url) = m.url.as_deref().filter(|x| !x.is_empty()) else {
                    diags.push(u::diag(
                        "error",
                        "mcp_no_url",
                        format!("{} server without a url", m.transport),
                        Some(name),
                    ));
                    continue;
                };
                json!({"type": "remote", "enabled": true, "url": url})
            };
            out.insert(name.to_string(), e);
        }
        out
    }

    fn permission(perms: &PermissionSet, diags: &mut Vec<Diag>) -> Map<String, Value> {
        let mut bash: IndexMap<String, String> = IndexMap::new();
        let mut top: Vec<(String, Value)> = Vec::new();
        let set_top = |top: &mut Vec<(String, Value)>, k: &str, v: Value| match top.iter_mut().find(|(x, _)| x == k) {
            Some(slot) => slot.1 = v,
            None => top.push((k.to_string(), v)),
        };
        let tool_name = |m: &str| match m {
            "write" | "edit" => Some("edit"),
            "web_fetch" => Some("webfetch"),
            _ => None,
        };
        for r in &perms.rules {
            match (r.kind.as_str(), r.matches.as_str()) {
                ("command", m) => {
                    bash.insert(m.to_string(), r.decision.clone());
                }
                ("tool", "shell") => {
                    bash.insert("*".into(), r.decision.clone());
                }
                ("tool", m) if tool_name(m).is_some() => {
                    set_top(&mut top, tool_name(m).unwrap_or_default(), Value::from(r.decision.clone()));
                }
                ("path_read" | "path_write", m) if r.decision == "deny" => {
                    bash.insert(format!("* {m}"), "deny".into());
                    diags.push(u::diag(
                        "info",
                        "approximated",
                        format!(
                            "{} deny {} rendered as a bash glob (opencode has no path ACL)",
                            r.kind,
                            text::repr(m)
                        ),
                        None,
                    ));
                }
                _ => diags.push(u::diag(
                    "warn",
                    "unsupported_rule",
                    format!("rule {}:{}:{} has no opencode equivalent", r.kind, r.matches, r.decision),
                    None,
                )),
            }
        }
        if !bash.is_empty() {
            let star = bash
                .get("*")
                .cloned()
                .unwrap_or_else(|| perms.default_tool_decision.clone());
            let mut ordered = Map::new();
            ordered.insert("*".into(), Value::from(star));
            for (k, v) in &bash {
                if k != "*" {
                    ordered.insert(k.clone(), Value::from(v.clone()));
                }
            }
            set_top(&mut top, "bash", Value::Object(ordered));
        }
        top.sort_by(|a, b| a.0.cmp(&b.0));
        top.into_iter().collect()
    }
}

fn discover_mcp(mcp: Option<&PyValue>, notes: &mut Vec<String>) -> AdapterResult<Vec<McpItem>> {
    let mut out = Vec::new();
    let Some(PyValue::Dict(d)) = mcp else {
        return Ok(out);
    };
    for (name, e) in u::sorted_items(d)? {
        let key = name.py_str();
        if !matches!(e, PyValue::Dict(_)) || !u::valid_name(&key) {
            notes.push(format!("skipped mcp server {}", name.repr()));
            continue;
        }
        if e.get("type").and_then(PyValue::as_str) == Some("remote") && e.get("url").is_some_and(u::truthy) {
            out.push(McpItem {
                url: Some(u::cap_str(e.get("url"), 2000)),
                ..mcp_item(&key, "http")
            });
        } else if let Some(PyValue::List(cmd)) = e.get("command")
            && !cmd.is_empty()
        {
            let cmd: Vec<String> = cmd.iter().map(PyValue::py_str).collect();
            let env = match e.get("environment") {
                Some(PyValue::Dict(env)) => u::sorted_set(env.iter().map(|(k, _)| k.py_str())),
                _ => Vec::new(),
            };
            out.push(McpItem {
                command: Some(cmd[0].clone()),
                args: cmd[1..].to_vec(),
                env_names: env,
                ..mcp_item(&key, "stdio")
            });
        }
    }
    Ok(out)
}

fn discover_permission(perm: &[(PyValue, PyValue)]) -> AdapterResult<PermissionSet> {
    let mut rules = Vec::new();
    let ok = |d: &PyValue| matches!(d.as_str(), Some("allow" | "ask" | "deny"));
    let rule = |kind: &str, m: &str, d: &str| Rule {
        kind: kind.into(),
        matches: m.into(),
        decision: d.into(),
        reason: None,
    };
    for (key, val) in u::sorted_items(perm)? {
        match (key.as_str(), &val) {
            (Some("bash"), PyValue::Dict(globs)) => {
                for (glob, d) in globs {
                    if !ok(d) {
                        continue;
                    }
                    let d = d.as_str().unwrap_or_default();
                    if glob.as_str() == Some("*") {
                        rules.push(rule("tool", "shell", d));
                    } else {
                        rules.push(rule("command", &glob.py_str(), d));
                    }
                }
            }
            (k, PyValue::Str(v)) if ok(&val) => {
                let cap = match k {
                    Some("edit") => Some("edit"),
                    Some("webfetch") => Some("web_fetch"),
                    _ => None,
                };
                if let Some(cap) = cap {
                    rules.push(rule("tool", cap, v));
                }
            }
            _ => {}
        }
    }
    Ok(PermissionSet {
        rules,
        ..PermissionSet::default()
    })
}

impl ClientAdapter for OpenCodeAdapter {
    fn id(&self) -> &str {
        ID
    }

    fn display_name(&self) -> &str {
        "opencode"
    }

    fn docs(&self) -> &str {
        "" // the Python class has no docstring
    }

    fn caps(&self) -> AdapterCaps {
        AdapterCaps {
            skills: false,
            agents: true,
            instructions: true,
            mcp: true,
            permissions: true,
            egress: false,
            memory: false,
            tool_map: TOOL_MAP.iter().map(|(c, t)| ((*c).to_string(), t.map(str::to_string))).collect(),
            model_tiers: IndexMap::new(),
            notes: "No skills directory is known for opencode (param skills_via_claude_dir=true renders to \
                    .claude/skills; whether opencode reads it depends on your opencode version). Model tiers are \
                    unmapped by default: set params.model_tiers {fast/standard/deep: provider/model}."
                .into(),
        }
    }

    fn default_config(&self) -> ClientConfig {
        ClientConfig {
            id: "opencode".into(),
            adapter: ID.into(),
            display_name: "opencode".into(),
            description: "opencode: agents + AGENTS.md managed; opencode.json advisory; skills off.".into(),
            roots: [("project".to_string(), "~/path/to/project".to_string())].into_iter().collect(),
            params: PyValue::Dict(vec![
                (PyValue::str("manage"), flags_value(&DEFAULT_MANAGE)),
                (PyValue::str("skills_via_claude_dir"), PyValue::Bool(false)),
                (PyValue::str("temperature"), PyValue::Float(0.1)),
                (PyValue::str("model_tiers"), PyValue::Dict(Vec::new())),
                (PyValue::str("sandbox_wrapper"), PyValue::List(Vec::new())),
            ]),
            strict: false,
            icon: Some("code".into()),
            ..ClientConfig::default()
        }
    }

    fn render(&self, ctx: &RenderContext) -> AdapterResult<RenderResult> {
        let cfg = &ctx.client;
        let mut diags = Vec::new();
        let mut arts = Vec::new();
        let root = "project";
        if !ctx.skills.is_empty() {
            if matches!(cfg.param("skills_via_claude_dir"), Some(PyValue::Bool(true))) {
                diags.push(u::diag(
                    "info",
                    "skills_via_claude_dir",
                    "rendering skills to .claude/skills; whether opencode reads .claude/skills depends on your \
                     opencode version",
                    None,
                ));
                arts.extend(u::skill_artifacts(&ctx.skills, root, ".claude/skills", &mut diags, "SKILL.md")?);
            } else {
                diags.push(u::diag(
                    u::unsupported_severity(cfg),
                    "unsupported_capability",
                    "opencode has no known skills directory (set params.skills_via_claude_dir to try .claude/skills)",
                    None,
                ));
            }
        }
        let dup = u::dedupe(ctx.agents.iter().map(|a| a.name.as_str()), "agent", &mut diags);
        let mut agents: Vec<&AgentItem> = ctx.agents.iter().collect();
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        for ag in agents {
            if dup.contains(&ag.name) {
                continue;
            }
            if let Some(a) = Self::agent(ag, cfg, root, &mut diags) {
                arts.push(a);
            }
        }
        if !ctx.instructions.is_empty()
            && let Some(ins) = u::instruction_artifact(&ctx.instructions, root, "AGENTS.md", &cfg.params, &mut diags)
        {
            arts.push(ins);
        }
        // One artifact per concern on the SAME file: the core merges them but applies each concern's manage flag.
        if !ctx.instructions.is_empty()
            && let Some(PyValue::List(files)) = cfg.param("instruction_files")
            && !files.is_empty()
        {
            let list = u::sorted_set(files.iter().map(PyValue::py_str));
            let ids = u::sorted_instructions(&ctx.instructions)
                .iter()
                .map(|i| i.id.clone())
                .collect();
            arts.push(json_art("instruction", &json!({"instructions": list}), &["instructions"], ids));
        }
        if !ctx.mcp_servers.is_empty() {
            let servers = Self::mcp(ctx, &mut diags);
            let ids = u::sorted_set(servers.keys().cloned());
            arts.push(json_art("mcp", &json!({"mcp": Value::Object(servers)}), &["mcp"], ids));
        }
        if !ctx.permissions.rules.is_empty() {
            let perm = Self::permission(&ctx.permissions, &mut diags);
            let ids = u::sorted_set(ctx.permissions.rules.iter().map(|r| format!("{}:{}", r.kind, r.matches)));
            arts.push(json_art("rule", &json!({"permission": Value::Object(perm)}), &["permission"], ids));
        }
        if !ctx.memory.is_empty() {
            diags.push(u::diag(
                u::unsupported_severity(cfg),
                "unsupported_capability",
                "memory is not rendered for opencode",
                None,
            ));
        }
        arts.sort_by(|a, b| (&a.root, &a.path).cmp(&(&b.root, &b.path)));
        Ok(RenderResult {
            artifacts: arts,
            diagnostics: diags,
        })
    }

    fn discover(&self, cfg: &ClientConfig, fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent> {
        let mut res = DiscoveredContent::default();
        if matches!(cfg.param("skills_via_claude_dir"), Some(PyValue::Bool(true))) {
            res.skills = u::discover_skills(fs, cfg, "project", ".claude/skills", &mut res.notes)?;
        }
        if let Some(base) = u::fs_path(cfg, "project", ".opencode/agent")?
            && fs.exists(&base)
            && fs.is_dir(&base)
        {
            for fname in u::listdir_sorted(fs, &base)? {
                let Some(name) = fname.strip_suffix(".md") else {
                    continue;
                };
                if !u::valid_name(name) {
                    res.notes
                        .push(format!("skipped agent file {}: not a valid hub name", text::repr(&fname)));
                    continue;
                }
                let (fm, body) =
                    u::parse_frontmatter(&u::read_text(fs, &format!("{base}/{fname}"), &mut Vec::new()))?;
                let no_tools = PyValue::Dict(Vec::new());
                let tools = match fm.get("tools") {
                    Some(t @ PyValue::Dict(_)) => t,
                    _ => &no_tools,
                };
                let caps: Vec<String> = GATED
                    .iter()
                    .filter(|c| match tool_of(c).flatten() {
                        None => true,
                        Some(native) => !matches!(tools.get(native), Some(PyValue::Bool(false))),
                    })
                    .map(|c| (*c).to_string())
                    .collect();
                let perm_edit_deny = match fm.get("permission") {
                    Some(p @ PyValue::Dict(_)) => p.get("edit").and_then(PyValue::as_str) == Some("deny"),
                    _ => false,
                };
                let has = |c: &str| caps.iter().any(|x| x == c);
                let read_only = !has("write") || !has("edit") || perm_edit_deny;
                let mode = if fm.get("mode").and_then(PyValue::as_str) == Some("primary") {
                    "primary"
                } else {
                    "subagent"
                };
                res.agents.push(AgentItem {
                    description: u::cap_str(fm.get("description"), u::MAX_DESC_ERR),
                    capabilities: caps,
                    mode: mode.into(),
                    read_only,
                    body,
                    ..agent_item(name)
                });
                if let Some(model) = fm.get("model").filter(|m| u::truthy(m)) {
                    res.notes
                        .push(format!("agent {name}: model {} not mapped to a tier", model.repr()));
                }
            }
        }
        if let Some(p) = u::fs_path(cfg, "project", "AGENTS.md")?
            && fs.exists(&p)
            && u::extract_block(&u::read_text(fs, &p, &mut Vec::new()), "md").is_none()
        {
            res.suggested_params = vec![("instructions_mode".into(), PyValue::str("own"))];
            res.instructions.push(InstructionItem {
                id: "agents-md".into(),
                title: "AGENTS.md".into(),
                order: 0,
                body: u::read_text(fs, &p, &mut Vec::new()),
            });
        }
        if let Some(cj) = u::fs_path(cfg, "project", "opencode.json")?
            && fs.exists(&cj)
        {
            let s = u::read_text(fs, &cj, &mut Vec::new());
            let doc = match u::json_loads(if s.is_empty() { "null" } else { &s }) {
                Ok(d) => d,
                Err(_) => {
                    res.notes.push("opencode.json is not valid JSON (may contain comments)".into());
                    PyValue::Dict(Vec::new())
                }
            };
            if matches!(doc, PyValue::Dict(_)) {
                res.mcp_servers = discover_mcp(doc.get("mcp"), &mut res.notes)?;
                if let Some(PyValue::Dict(gperm)) = doc.get("permission") {
                    res.permissions = Some(discover_permission(gperm)?);
                }
            }
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
        // no offline "list what you loaded" command exists for opencode; files + hashes only.
        u::verify_expected(cfg, expected, fs)
    }
}

/// The registry entry (Python: module attribute `ADAPTER`).
pub fn adapter() -> Option<Result<Arc<dyn ClientAdapter>, String>> {
    Some(Ok(Arc::new(OpenCodeAdapter)))
}
