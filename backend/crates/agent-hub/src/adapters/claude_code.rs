//! Port of backend/src/agent_hub/adapters/claude_code.py: the Claude Code adapter (id `claude-code`). Owner: WS-A.
//!
//! Where things go (all under the configured `project` root, never under the user config dir):
//!   skills        .claude/skills/<name>/...      (dir-per-skill; SKILL.md standard passed through)
//!   agents        .claude/agents/<name>.md       (frontmatter name/description/tools/model)
//!   instructions  CLAUDE.md                      (params.instructions_mode: block = hub markers, own = whole file)
//!   mcp           .mcp.json                      (merge=json_keys on `mcpServers`)                ADVISORY by default
//!   permissions   .claude/settings.json          (merge=json_keys on permissions.allow/ask/deny)  ADVISORY by default
//! `params.sandbox_wrapper` (argv list) prefixes every MCP server that declares a `sandbox_profile`; without it such a
//! server is an ERROR (fail closed).

use std::collections::BTreeSet;
use std::sync::{Arc, LazyLock};

use hub_core::py::text;
use hub_core::value::PyValue;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value, json};

use super::_util as u;
use super::base::{
    AdapterCaps, AdapterError, AdapterResult, AgentItem, Artifact, Check, ClientAdapter, ClientConfig, CommandRunner,
    Diag, DiscoveredContent, Expected, FileSystem, InstructionItem, McpItem, PermissionSet, RenderContext,
    RenderResult, Rule,
};

pub const ID: &str = "claude-code";

/// canonical capability -> Claude Code tool (`None` = unsupported).
pub const TOOL_MAP: [(&str, Option<&str>); 12] = [
    ("read", Some("Read")),
    ("write", Some("Write")),
    ("edit", Some("Edit")),
    ("shell", Some("Bash")),
    ("web_fetch", Some("WebFetch")),
    ("web_search", Some("WebSearch")),
    ("mcp", Some("mcp__*")),
    ("subagent", Some("Task")),
    ("browser", None),
    ("notebook", Some("NotebookEdit")),
    ("todo", Some("TodoWrite")),
    ("image", None),
];
pub const MODEL_TIERS: [(&str, &str); 3] = [("fast", "haiku"), ("standard", "sonnet"), ("deep", "opus")];
const REV_TOOLS: [(&str, &str); 12] = [
    ("Read", "read"),
    ("Grep", "read"),
    ("Glob", "read"),
    ("Write", "write"),
    ("Edit", "edit"),
    ("MultiEdit", "edit"),
    ("Bash", "shell"),
    ("WebFetch", "web_fetch"),
    ("WebSearch", "web_search"),
    ("Task", "subagent"),
    ("NotebookEdit", "notebook"),
    ("TodoWrite", "todo"),
];
pub const DEFAULT_MANAGE: [(&str, bool); 6] = [
    ("skills", true),
    ("agents", true),
    ("instructions", true),
    ("mcp", false),
    ("permissions", false),
    ("memory", false),
];

// `(\w+)(?:\((.*)\))?` with Python's `\w` (str.isalnum() or "_"), fullmatch.
static RULE_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^([\p{L}\p{N}_]+)(?:\((.*)\))?$"));
// `([^*?\[\]]+?)\s*\*` (Python `\s` = str.isspace()), fullmatch.
static CMD_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^([^*?\[\]]+?)[\s\x1c-\x1f]*\*$"));

/// `TOOL_MAP.get(cap)`: `None` = not a key; `Some(None)` = unsupported.
pub fn tool_of(cap: &str) -> Option<Option<&'static str>> {
    TOOL_MAP.iter().find(|(c, _)| *c == cap).map(|(_, t)| *t)
}

fn rev_tool(t: &str) -> Option<&'static str> {
    REV_TOOLS.iter().find(|(n, _)| *n == t).map(|(_, c)| *c)
}

fn model_of(tier: &str) -> Option<&'static str> {
    MODEL_TIERS.iter().find(|(t, _)| *t == tier).map(|(_, m)| *m)
}

fn rev_model(m: &str) -> Option<&'static str> {
    MODEL_TIERS.iter().find(|(_, x)| *x == m).map(|(t, _)| *t)
}

/// `_native_tools(caps)`: "read" also grants Grep/Glob.
fn native_tools(caps: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in caps {
        let expansion: Vec<&str> = if c == "read" {
            vec!["Read", "Grep", "Glob"]
        } else {
            tool_of(c).flatten().into_iter().collect()
        };
        for t in expansion {
            if !out.iter().any(|x| x == t) {
                out.push(t.to_string());
            }
        }
    }
    out
}

/// `_cmd_pattern(glob)`: hub command glob -> Claude `Bash(...)` matcher (`docker compose *` -> `Bash(docker compose:*)`).
pub fn cmd_pattern(glob: &str) -> String {
    let g = text::strip(glob);
    match CMD_RE.captures(g) {
        Some(m) => format!("Bash({}:*)", text::strip(m.get(1).map_or("", |x| x.as_str()))),
        None => format!("Bash({g})"),
    }
}

fn rule_strings(rule: &Rule) -> Option<Vec<String>> {
    match rule.kind.as_str() {
        "tool" => tool_of(&rule.matches).flatten().map(|t| vec![t.to_string()]),
        "command" => Some(vec![cmd_pattern(&rule.matches)]),
        "path_read" => Some(vec![format!("Read({})", rule.matches)]),
        "path_write" => Some(vec![format!("Edit({})", rule.matches), format!("Write({})", rule.matches)]),
        "mcp_server" => Some(vec![format!("mcp__{}", rule.matches)]),
        "egress_host" => Some(vec![format!("WebFetch(domain:{})", rule.matches)]),
        _ => None,
    }
}

/// `render_permissions(perms)`: `{"allow": [...], "ask": [...], "deny": [...]}` (sorted) and the diagnostics.
pub fn render_permissions(perms: &PermissionSet) -> (Map<String, Value>, Vec<Diag>) {
    let mut lists: IndexMap<&str, BTreeSet<String>> =
        ["allow", "ask", "deny"].into_iter().map(|d| (d, BTreeSet::new())).collect();
    let mut diags = Vec::new();
    for r in &perms.rules {
        let Some(strs) = rule_strings(r) else {
            diags.push(u::diag(
                "warn",
                "unsupported_rule",
                format!("rule {}:{} has no Claude Code equivalent", r.kind, r.matches),
                None,
            ));
            continue;
        };
        if let Some(set) = lists.get_mut(r.decision.as_str()) {
            set.extend(strs);
        }
    }
    if perms.rules.iter().any(|r| r.kind == "egress_host") {
        diags.push(u::diag(
            "info",
            "egress_advisory",
            "egress_host rules render as WebFetch(domain:...) only; Bash network egress is governed by the sandbox \
             settings, which the hub never writes",
            None,
        ));
    }
    let map = lists
        .into_iter()
        .map(|(k, v)| (k.to_string(), Value::from(v.into_iter().collect::<Vec<_>>())))
        .collect();
    (map, diags)
}

fn mcp_entry(m: &McpItem, wrapper: &[String]) -> Value {
    if m.transport == "stdio" {
        let mut argv: Vec<String> = Vec::new();
        if m.sandbox_profile.as_deref().is_some_and(|p| !p.is_empty()) {
            argv.extend(wrapper.iter().cloned());
        }
        argv.push(m.command.clone().unwrap_or_default());
        argv.extend(m.args.iter().cloned());
        let mut entry = Map::new();
        entry.insert("command".into(), Value::from(argv[0].clone()));
        entry.insert("args".into(), Value::from(argv[1..].to_vec()));
        if !m.env_names.is_empty() {
            // names only: Claude Code expands ${VAR} from the launching environment; the value never enters the hub
            let mut names = m.env_names.clone();
            names.sort();
            let env: Map<String, Value> = names
                .into_iter()
                .map(|n| {
                    let v = format!("${{{n}}}");
                    (n, Value::from(v))
                })
                .collect();
            entry.insert("env".into(), Value::Object(env));
        }
        return Value::Object(entry);
    }
    json!({"type": m.transport, "url": m.url.clone().unwrap_or_default()})
}

/// `{"skills": True, ...}` as a Python dict value.
pub fn flags_value(pairs: &[(&str, bool)]) -> PyValue {
    PyValue::Dict(pairs.iter().map(|(k, v)| (PyValue::str(*k), PyValue::Bool(*v))).collect())
}

/// A pydantic-default `AgentItem` (mode "subagent").
pub fn agent_item(name: &str) -> AgentItem {
    AgentItem {
        name: name.into(),
        mode: "subagent".into(),
        ..AgentItem::default()
    }
}

/// A pydantic-default `McpItem` (scan_status "unscanned").
pub fn mcp_item(name: &str, transport: &str) -> McpItem {
    McpItem {
        name: name.into(),
        transport: transport.into(),
        scan_status: "unscanned".into(),
        ..McpItem::default()
    }
}

/// `"'<type>' object has no attribute 'items'"` (`(x or {}).items()` on a truthy non-dict).
pub fn no_items(v: &PyValue) -> AdapterError {
    AdapterError::new(
        "AttributeError",
        format!("'{}' object has no attribute 'items'", v.type_name()),
    )
}

/// `ClaudeCodeAdapter`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeCodeAdapter;

impl ClaudeCodeAdapter {
    fn agents(ctx: &RenderContext, root: &str, diags: &mut Vec<Diag>) -> Vec<Artifact> {
        let mut out = Vec::new();
        let dup = u::dedupe(ctx.agents.iter().map(|a| a.name.as_str()), "agent", diags);
        let mut sorted: Vec<&AgentItem> = ctx.agents.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for ag in sorted {
            if dup.contains(&ag.name) {
                continue;
            }
            if let Some(art) = Self::agent(ag, &ctx.client, root, diags) {
                out.push(art);
            }
        }
        out
    }

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
        let body_too_large = text::char_len(&ag.body) > u::MAX_BODY_CHARS;
        if !u::check_description(&ag.description, name, diags) || body_too_large {
            if body_too_large {
                diags.push(u::diag("error", "body_too_large", "agent body exceeds the size cap", Some(name)));
            }
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
            if tool_of(c) == Some(None) {
                diags.push(u::diag(
                    u::unsupported_severity(cfg),
                    "unsupported_capability",
                    format!("capability {} has no Claude Code tool", text::repr(c)),
                    Some(name),
                ));
            }
        }
        if ag.read_only && ag.capabilities.iter().any(|c| u::READ_ONLY_DROPS.contains(&c.as_str())) {
            diags.push(u::diag("info", "read_only_applied", "read_only: write/edit/notebook removed", Some(name)));
        }
        let tools = native_tools(&caps);
        if tools.is_empty() {
            // omitting `tools:` would make Claude Code inherit EVERY tool: fail closed instead
            diags.push(u::diag(
                "error",
                "empty_toolset",
                "agent would have no tools (Claude Code would inherit all)",
                Some(name),
            ));
            return None;
        }
        let mut fm: Vec<(PyValue, PyValue)> = vec![
            (PyValue::str("name"), PyValue::str(name)),
            (PyValue::str("description"), PyValue::str(u::oneline(&ag.description, name, diags))),
            (PyValue::str("tools"), PyValue::str(tools.join(", "))),
        ];
        if let Some(tier) = &ag.model_tier {
            fm.push((PyValue::str("model"), PyValue::str(model_of(tier).unwrap_or_default())));
        }
        if ag.mode == "primary" {
            diags.push(u::diag(
                "info",
                "mode_ignored",
                "Claude Code agents are subagents; mode=primary has no effect",
                Some(name),
            ));
        }
        let content = u::render_frontmatter(&PyValue::Dict(fm), &ag.body);
        let path = u::safe_relpath(&[".claude/agents", &format!("{name}.md")]).ok()?;
        let mut a = Artifact::new(root, &path, u::to_bytes(&content), "agent");
        a.source_ids = vec![name.to_string()];
        Some(a)
    }

    fn mcp(ctx: &RenderContext, root: &str, diags: &mut Vec<Diag>) -> Artifact {
        let wrapper = u::sandbox_prefix(&ctx.client.params);
        let mut servers = Map::new();
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
            if m.transport == "stdio" && m.command.as_deref().is_none_or(str::is_empty) {
                diags.push(u::diag("error", "mcp_no_command", "stdio server without a command", Some(name)));
                continue;
            }
            if m.transport != "stdio" && m.url.as_deref().is_none_or(str::is_empty) {
                diags.push(u::diag(
                    "error",
                    "mcp_no_url",
                    format!("{} server without a url", m.transport),
                    Some(name),
                ));
                continue;
            }
            if m.sandbox_profile.as_deref().is_some_and(|p| !p.is_empty()) && wrapper.is_empty() {
                u::sandbox_missing(name, diags);
                continue;
            }
            servers.insert(name.to_string(), mcp_entry(m, &wrapper));
        }
        let ids: Vec<String> = u::sorted_set(servers.keys().cloned());
        let mut a = Artifact::new(
            root,
            ".mcp.json",
            u::dump_json(&json!({"mcpServers": Value::Object(servers)})),
            "mcp",
        );
        a.source_ids = ids;
        a.merge = "json_keys".into();
        a.managed_keys = vec!["mcpServers".into()];
        a
    }

    fn discover_agents(cfg: &ClientConfig, fs: &dyn FileSystem, notes: &mut Vec<String>) -> AdapterResult<Vec<AgentItem>> {
        let mut out = Vec::new();
        let Some(base) = u::fs_path(cfg, "project", ".claude/agents")? else {
            return Ok(out);
        };
        if !fs.exists(&base) || !fs.is_dir(&base) {
            return Ok(out);
        }
        for fname in u::listdir_sorted(fs, &base)? {
            let Some(name) = fname.strip_suffix(".md") else {
                continue;
            };
            if !u::valid_name(name) {
                notes.push(format!("skipped agent file {}: not a valid hub name", text::repr(&fname)));
                continue;
            }
            let (fm, body) = u::parse_frontmatter(&u::read_text(fs, &format!("{base}/{fname}"), &mut Vec::new()))?;
            let raw_tools = u::cap_str(fm.get("tools"), 2000);
            let names: Vec<&str> = raw_tools.split(',').map(text::strip).filter(|t| !t.is_empty()).collect();
            let caps = u::ordered_caps(names.iter().filter_map(|t| rev_tool(t)));
            for t in &names {
                if rev_tool(t).is_none() {
                    notes.push(format!(
                        "agent {name}: tool {} has no canonical capability (dropped)",
                        text::repr(t)
                    ));
                }
            }
            if names.is_empty() {
                notes.push(format!(
                    "agent {name}: no tools: line, Claude inherits all tools; imported with no capabilities"
                ));
            }
            let tier = rev_model(&u::cap_str(fm.get("model"), 100)).map(str::to_string);
            let read_only = !caps.iter().any(|c| c == "write" || c == "edit");
            out.push(AgentItem {
                description: u::cap_str(fm.get("description"), u::MAX_DESC_ERR),
                capabilities: caps,
                model_tier: tier,
                read_only,
                body,
                ..agent_item(name)
            });
        }
        Ok(out)
    }

    fn discover_instructions(cfg: &ClientConfig, fs: &dyn FileSystem) -> AdapterResult<Vec<InstructionItem>> {
        let Some(p) = u::fs_path(cfg, "project", "CLAUDE.md")? else {
            return Ok(Vec::new());
        };
        if !fs.exists(&p) {
            return Ok(Vec::new());
        }
        let s = u::read_text(fs, &p, &mut Vec::new());
        if u::extract_block(&s, "md").is_some() {
            return Ok(Vec::new()); // the hub's own block: nothing new to import
        }
        Ok(vec![InstructionItem {
            id: "claude-md".into(),
            title: "CLAUDE.md".into(),
            order: 0,
            body: s,
        }])
    }

    fn discover_permissions(
        cfg: &ClientConfig,
        fs: &dyn FileSystem,
        notes: &mut Vec<String>,
    ) -> AdapterResult<Option<PermissionSet>> {
        let Some(p) = u::fs_path(cfg, "project", ".claude/settings.json")? else {
            return Ok(None);
        };
        if !fs.exists(&p) {
            return Ok(None);
        }
        let s = u::read_text(fs, &p, &mut Vec::new());
        let Ok(doc) = u::json_loads(if s.is_empty() { "null" } else { &s }) else {
            notes.push(".claude/settings.json is not valid JSON".into());
            return Ok(None);
        };
        let Some(perms @ PyValue::Dict(_)) = doc.get("permissions") else {
            return Ok(None);
        };
        let mut rules = Vec::new();
        for dec in ["deny", "ask", "allow"] {
            let Some(PyValue::List(items)) = perms.get(dec) else {
                continue;
            };
            for s in items {
                match parse_rule(&s.py_str(), dec) {
                    Some(r) => rules.push(r),
                    None => notes.push(format!("permission {} not representable as an abstract rule", s.repr())),
                }
            }
        }
        Ok(Some(PermissionSet {
            rules,
            ..PermissionSet::default()
        }))
    }

    fn discover_mcp(cfg: &ClientConfig, fs: &dyn FileSystem, notes: &mut Vec<String>) -> AdapterResult<Vec<McpItem>> {
        let Some(p) = u::fs_path(cfg, "project", ".mcp.json")? else {
            return Ok(Vec::new());
        };
        if !fs.exists(&p) {
            return Ok(Vec::new());
        }
        let s = u::read_text(fs, &p, &mut Vec::new());
        let Ok(doc) = u::json_loads(if s.is_empty() { "null" } else { &s }) else {
            notes.push(".mcp.json is not valid JSON".into());
            return Ok(Vec::new());
        };
        let items: Vec<(PyValue, PyValue)> = match doc.get("mcpServers").filter(|v| u::truthy(v)) {
            None => Vec::new(),
            Some(PyValue::Dict(d)) => u::sorted_items(d)?,
            Some(other) => return Err(no_items(other)),
        };
        let mut out = Vec::new();
        for (name, e) in items {
            let key = name.py_str();
            if !matches!(e, PyValue::Dict(_)) || !u::valid_name(&key) {
                notes.push(format!("skipped mcp server {}", name.repr()));
                continue;
            }
            let env_names = match e.get("env") {
                Some(PyValue::Dict(env)) => u::sorted_set(env.iter().map(|(k, _)| k.py_str())),
                _ => Vec::new(),
            };
            if e.get("url").is_some_and(u::truthy) {
                let t = if e.get("type").and_then(PyValue::as_str) == Some("sse") { "sse" } else { "http" };
                out.push(McpItem {
                    url: Some(u::cap_str(e.get("url"), 2000)),
                    env_names,
                    ..mcp_item(&key, t)
                });
            } else {
                let args = match e.get("args") {
                    None => Vec::new(),
                    Some(a) => u::py_iter(a)?.iter().map(PyValue::py_str).collect(),
                };
                out.push(McpItem {
                    command: Some(e.get("command").map_or_else(String::new, PyValue::py_str)),
                    args,
                    env_names,
                    ..mcp_item(&key, "stdio")
                });
            }
        }
        Ok(out)
    }
}

/// `_parse_rule(s, decision)`: a Claude Code permission string -> an abstract rule (or `None`).
pub fn parse_rule(s: &str, decision: &str) -> Option<Rule> {
    let m = RULE_RE.captures(text::strip(s))?;
    let tool = m.get(1).map_or("", |x| x.as_str());
    let arg = m.get(2).map(|x| x.as_str());
    let rule = |kind: &str, matches: &str| Rule {
        kind: kind.into(),
        matches: matches.into(),
        decision: decision.into(),
        reason: None,
    };
    match (tool, arg) {
        ("Bash", Some(a)) => Some(rule(
            "command",
            &match a.strip_suffix(":*") {
                Some(head) => format!("{head} *"),
                None => a.to_string(),
            },
        )),
        ("Read", Some(a)) => Some(rule("path_read", a)),
        ("Edit" | "Write", Some(a)) => Some(rule("path_write", a)),
        ("WebFetch", Some(a)) if a.starts_with("domain:") => Some(rule("egress_host", &a[7..])),
        (t, None) if t.starts_with("mcp__") => Some(rule("mcp_server", &t[5..])),
        (t, None) => rev_tool(t).map(|c| rule("tool", c)),
        _ => None,
    }
}

impl ClientAdapter for ClaudeCodeAdapter {
    fn id(&self) -> &str {
        ID
    }

    fn display_name(&self) -> &str {
        "Claude Code"
    }

    fn docs(&self) -> &str {
        "" // the Python class has no docstring (`type(a).__doc__` is None)
    }

    fn caps(&self) -> AdapterCaps {
        AdapterCaps {
            skills: true,
            agents: true,
            instructions: true,
            mcp: true,
            permissions: true,
            egress: false,
            memory: false,
            tool_map: TOOL_MAP.iter().map(|(c, t)| ((*c).to_string(), t.map(str::to_string))).collect(),
            model_tiers: MODEL_TIERS.iter().map(|(t, m)| ((*t).to_string(), (*m).to_string())).collect(),
            notes: "mcp and permissions are advisory by default (a user-level settings file may be owned by another \
                    process; the hub only writes project files). Memory is not rendered."
                .into(),
        }
    }

    fn default_config(&self) -> ClientConfig {
        ClientConfig {
            id: "claude-code".into(),
            adapter: ID.into(),
            display_name: "Claude Code".into(),
            description: "Claude Code. Skills/agents/CLAUDE.md managed; mcp/permissions advisory.".into(),
            roots: [("project".to_string(), "~/path/to/project".to_string())].into_iter().collect(),
            params: PyValue::Dict(vec![
                (PyValue::str("manage"), flags_value(&DEFAULT_MANAGE)),
                (PyValue::str("sandbox_wrapper"), PyValue::List(Vec::new())),
            ]),
            strict: true,
            icon: Some("terminal".into()),
            ..ClientConfig::default()
        }
    }

    fn render(&self, ctx: &RenderContext) -> AdapterResult<RenderResult> {
        let cfg = &ctx.client;
        let mut diags = Vec::new();
        let root = "project";
        let mut arts = u::skill_artifacts(&ctx.skills, root, ".claude/skills", &mut diags, "SKILL.md")?;
        arts.extend(Self::agents(ctx, root, &mut diags));
        if !ctx.instructions.is_empty()
            && let Some(ins) = u::instruction_artifact(&ctx.instructions, root, "CLAUDE.md", &cfg.params, &mut diags)
        {
            arts.push(ins);
        }
        if !ctx.mcp_servers.is_empty() {
            arts.push(Self::mcp(ctx, root, &mut diags));
        }
        if !ctx.permissions.rules.is_empty() {
            let (lists, pd) = render_permissions(&ctx.permissions);
            diags.extend(pd);
            let mut a = Artifact::new(
                root,
                ".claude/settings.json",
                u::dump_json(&json!({"permissions": Value::Object(lists)})),
                "rule",
            );
            a.source_ids = u::sorted_set(ctx.permissions.rules.iter().map(|r| format!("{}:{}", r.kind, r.matches)));
            a.merge = "json_keys".into();
            a.managed_keys = vec![
                "permissions.allow".into(),
                "permissions.ask".into(),
                "permissions.deny".into(),
            ];
            arts.push(a);
        }
        if !ctx.memory.is_empty() {
            diags.push(u::diag(
                u::unsupported_severity(cfg),
                "unsupported_capability",
                "memory is not rendered for Claude Code (use the curated CLAUDE.md instructions instead)",
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
        res.skills = u::discover_skills(fs, cfg, "project", ".claude/skills", &mut res.notes)?;
        res.agents = Self::discover_agents(cfg, fs, &mut res.notes)?;
        res.instructions = Self::discover_instructions(cfg, fs)?;
        if !res.instructions.is_empty() {
            res.suggested_params = vec![("instructions_mode".into(), PyValue::str("own"))];
        }
        res.permissions = Self::discover_permissions(cfg, fs, &mut res.notes)?;
        res.mcp_servers = Self::discover_mcp(cfg, fs, &mut res.notes)?;
        Ok(res)
    }

    fn verify(
        &self,
        cfg: &ClientConfig,
        expected: &[Expected],
        fs: &dyn FileSystem,
        _run: &dyn CommandRunner,
    ) -> AdapterResult<Vec<Check>> {
        // Claude Code has no non-interactive `skills list`; files + hashes is the strongest offline proof available.
        u::verify_expected(cfg, expected, fs)
    }
}

/// The registry entry (Python: module attribute `ADAPTER`).
pub fn adapter() -> Option<Result<Arc<dyn ClientAdapter>, String>> {
    Some(Ok(Arc::new(ClaudeCodeAdapter)))
}
