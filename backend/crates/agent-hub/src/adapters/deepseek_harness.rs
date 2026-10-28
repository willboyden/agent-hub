//! Port of the DeepSeek Harness adapter (id `deepseek-harness`), for dsh, the DeepSeek agent harness. The Python
//! reference is `adapters/deepseek_harness.py` (written after c419f1e, never committed to the Python tree; its goldens
//! are recorded by `backend/parity/gen/gen_dsh_golden.py`).
//!
//! dsh reads user-global content from its agents home (`$DSH_AGENTS_HOME`): skills as `skills/<name>/SKILL.md`, an
//! `AGENTS.md` loaded for every session, and MCP servers as rows of a dsh patch YAML. The hub assumes dsh runs in a
//! container and never writes into it. `render` produces a STAGE tree (root `stage`, a host directory the hub owns):
//!
//!   skills/<name>/...        the skills tree (SKILL.md + supporting files)
//!   AGENTS.md                user-global instructions
//!   hub.cordis.patch.yml     MCP servers only, as a dsh patch: a top-level list holding one `insert` list of rows named
//!                            `@deepseek-ai/dsh-mcp-client` (stdio or streamable-http); absent when no server renders
//!
//! Delivery is the opt-in `deploy/dsh-deliver.sh` (never run by the app). `verify` checks the stage files and then asks
//! the running container, through dsh's own skill discovery, whether every staged skill is there with source
//! `user-agents`: `docker [--context <ctx>] exec <container> node /opt/lab/list-skills.mjs` (a helper the image must
//! provide). The program and its arguments are fixed; params choose only the container and the docker context, each a
//! plain name (anything else is a failed check and nothing runs).
//!
//! Security rules (dsh behaviour these follow): a SKILL.md or a file path holding a `<skill_...>` tag could fake dsh's
//! prompt framing (refused); SKILL.md over 64 KiB, a skill folder over 4 MiB, a missing / over-long / control-character
//! description and a frontmatter name that is missing or differs from the folder are errors (not staged); a patch row
//! whose text would hold `!!`, `!<` or a `%` directive line is refused (a YAML tag in a dsh patch can execute
//! JavaScript); environment variable names are never rendered. Owner: WS-A (0.3.0 port).

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use hub_core::py::text;
use hub_core::value::PyValue;
use hub_core::yamlsafe;
use indexmap::IndexMap;
use regex::Regex;

use super::_util::{self as u, PyJson};
use super::base::{
    AdapterCaps, AdapterResult, Artifact, Check, ClientAdapter, ClientConfig, CommandRunner, Diag, DiscoveredContent,
    Expected, FileSystem, McpItem, RenderContext, RenderResult, SkillItem,
};
use super::claude_code::flags_value;
use super::hermes::{is_format, scrub};

pub const ID: &str = "deepseek-harness";
pub const DEFAULT_MANAGE: [(&str, bool); 6] = [
    ("skills", true),
    ("agents", false),
    ("instructions", true),
    ("mcp", false),
    ("permissions", false),
    ("memory", false),
];
pub const PLACEHOLDER: &str = "CHANGEME";
pub const PATCH_FILE: &str = "hub.cordis.patch.yml";
pub const MCP_CLIENT: &str = "@deepseek-ai/dsh-mcp-client";
/// What dsh's discovery reports for skills found in `$DSH_AGENTS_HOME`.
pub const HUB_SOURCE: &str = "user-agents";
/// The fixed in-container program verify runs (the image provides it).
pub const LIST_SKILLS: [&str; 2] = ["node", "/opt/lab/list-skills.mjs"];
/// Keep these equal to the copies in deploy/dsh-deliver.sh (a test compares them).
pub const MAX_SKILL_MD: usize = 64 * 1024;
pub const MAX_SKILL_DIR: usize = 4 * 1024 * 1024;
/// Characters of the frontmatter description.
pub const MAX_DESCRIPTION: usize = 1024;
pub const MAX_LIST_OUTPUT: usize = 4 * 1024 * 1024;

/// Python's `DSH_NAME_RE.pattern` (used with fullmatch); deploy/dsh-deliver.sh's NAME_RE is the same rule (a test compares).
pub const DSH_NAME_PATTERN: &str = "[a-z0-9]+(?:-[a-z0-9]+)*";
static DSH_NAME_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(&format!("^(?:{DSH_NAME_PATTERN})$")));
/// `<\s*/?\s*skill_` with re.IGNORECASE, spelled out the way Python's sre matches it: `\s` is `str.isspace()` (incl.
/// \x1c-\x1f) and case-insensitive `s`, `k`, `i` also match U+017F, U+212A, U+0130 and U+0131 (measured over every code
/// point), which Rust's `(?i)` does not all do.
static FRAMING_RE: LazyLock<Regex> = LazyLock::new(|| {
    hub_core::re(
        r"<[\s\x1c-\x1f]*/?[\s\x1c-\x1f]*[sS\x{17F}][kK\x{212A}][iI\x{130}\x{131}][lL][lL]_",
    )
});
/// `[A-Za-z0-9][A-Za-z0-9_.-]{0,127}`, fullmatch.
static PLAIN_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$"));
/// `!!|!<|^\s*%` with re.MULTILINE (`^` after a `\n` only, as in Python).
static TAG_RISK_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"(?m)!!|!<|^[\s\x1c-\x1f]*%"));

/// `DELIVER_SCRIPT`: `<app root>/deploy/dsh-deliver.sh` (the opt-in delivery the hub never runs).
pub fn default_deliver_script() -> PathBuf {
    crate::config::app_root().join("deploy").join("dsh-deliver.sh")
}

/// `yaml_tag_risk(text)`: true when YAML text holds anything a tag check would flag: `!!` (a secondary-handle tag such
/// as `!!js/function`), `!<` (a verbose tag) or a `%` directive line (`%TAG` can rebind a handle). deploy/dsh-deliver.sh
/// applies the same rule.
pub fn yaml_tag_risk(s: &str) -> bool {
    TAG_RISK_RE.is_match(s)
}

/// Unicode categories Cc, Cf, Zl, Zp (CPython 3.13's tables).
fn bad_chars(s: &str) -> bool {
    s.chars()
        .any(|c| c.is_control() || is_format(c) || c == '\u{2028}' || c == '\u{2029}')
}

/// dsh error code -> the shared (`skill_artifacts`) warning about the same thing, dropped so it is not reported twice.
fn supersedes(code: &str) -> Option<&'static str> {
    match code {
        "skill_name_mismatch" => Some("skill_name_mismatch"),
        "description_too_long" => Some("description_long"),
        _ => None,
    }
}

fn dict(pairs: Vec<(&str, PyValue)>) -> PyValue {
    PyValue::Dict(pairs.into_iter().map(|(k, v)| (PyValue::str(k), v)).collect())
}

/// `DeepSeekHarnessAdapter`.
#[derive(Debug, Default, Clone, Copy)]
pub struct DeepSeekHarnessAdapter;

impl DeepSeekHarnessAdapter {
    /// dsh's own rules for one skill the shared checks let through: a Diag per problem; returns the codes of the ERRORS
    /// (a skill with any is not staged).
    fn skill_problems(s: &SkillItem, diags: &mut Vec<Diag>) -> AdapterResult<Vec<&'static str>> {
        let mut errors: Vec<&'static str> = Vec::new();
        let name = s.name.as_str();
        let mut err = |code: &'static str, msg: String, diags: &mut Vec<Diag>| {
            errors.push(code);
            diags.push(u::diag("error", code, msg, Some(name)));
        };
        if !DSH_NAME_RE.is_match(name) {
            err(
                "invalid_name",
                "dsh skill names are lowercase letters and digits in hyphen-separated words ([a-z0-9]+(-[a-z0-9]+)*)"
                    .into(),
                diags,
            );
        }
        let raw: &[u8] = s.files.get("SKILL.md").map_or(&[], Vec::as_slice);
        if raw.len() > MAX_SKILL_MD {
            err(
                "skill_too_large",
                format!(
                    "SKILL.md is {} bytes; dsh inserts it into the prompt whole, at most {MAX_SKILL_MD}",
                    raw.len()
                ),
                diags,
            );
        }
        let total: usize = s.files.values().map(Vec::len).sum();
        if total > MAX_SKILL_DIR {
            err(
                "skill_folder_too_large",
                format!("the skill folder is {total} bytes; at most {MAX_SKILL_DIR}"),
                diags,
            );
        }
        let Ok(body) = std::str::from_utf8(raw) else {
            err("skill_md_not_utf8", "SKILL.md is not UTF-8 text".into(), diags);
            return Ok(errors);
        };
        if FRAMING_RE.is_match(body) {
            err(
                "prompt_framing_tag",
                "SKILL.md contains a <skill_...> or </skill_...> tag; dsh inserts the body verbatim between its own \
                 <skill_...> tags, so it could fake the prompt framing"
                    .into(),
                diags,
            );
        }
        if s.files.keys().any(|p| FRAMING_RE.is_match(p)) {
            err(
                "prompt_framing_tag",
                "a file path in the skill contains a <skill_...> tag".into(),
                diags,
            );
        }
        let (fm, _) = u::parse_frontmatter(body)?;
        match fm.get("name") {
            Some(PyValue::Str(n)) if !n.is_empty() => {
                if n != name {
                    err(
                        "skill_name_mismatch",
                        format!(
                            "SKILL.md frontmatter name {} differs from the folder name; dsh lists the skill by its \
                             frontmatter name",
                            text::repr(&u::cap_str(fm.get("name"), 80))
                        ),
                        diags,
                    );
                }
            }
            _ => err(
                "skill_name_mismatch",
                "SKILL.md frontmatter has no name (dsh requires name and description)".into(),
                diags,
            ),
        }
        match fm.get("description") {
            Some(PyValue::Str(d)) if !text::strip(d).is_empty() => {
                let n = text::char_len(d);
                if n > MAX_DESCRIPTION {
                    err(
                        "description_too_long",
                        format!("description is {n} characters; at most {MAX_DESCRIPTION} for dsh"),
                        diags,
                    );
                }
                if bad_chars(text::strip(d)) {
                    err(
                        "description_control_chars",
                        "description holds control, format or line-separator characters (a multi-line description \
                         must be one line)"
                            .into(),
                        diags,
                    );
                }
            }
            _ => err(
                "missing_description",
                "SKILL.md frontmatter has no description (dsh requires name and description)".into(),
                diags,
            ),
        }
        if fm.contains_key("allowed-tools") || fm.contains_key("allowed_tools") {
            diags.push(u::diag(
                "warn",
                "allowed_tools_ignored",
                "dsh does not read allowed-tools; the skill's tool list is not enforced",
                Some(name),
            ));
        }
        Ok(errors)
    }

    fn mcp_rows(cfg: &ClientConfig, items: &[McpItem], diags: &mut Vec<Diag>) -> Vec<(String, PyValue)> {
        let mut rows: Vec<(String, PyValue)> = Vec::new();
        let mut sorted: Vec<&McpItem> = items.iter().collect();
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
            if !m.env_names.is_empty() {
                diags.push(u::diag(
                    "info",
                    "env_names_omitted",
                    "env var names not rendered (env stays empty): set credentials in the container, never in the hub",
                    Some(name),
                ));
            }
            if m.sandbox_profile.as_deref().is_some_and(|p| !p.is_empty()) {
                diags.push(u::diag(
                    "info",
                    "sandbox_profile_ignored",
                    "the dsh container is the sandbox; sandbox wrapping is not applied",
                    Some(name),
                ));
            }
            let config = match m.transport.as_str() {
                "stdio" => {
                    let Some(command) = m.command.as_deref().filter(|c| !c.is_empty()) else {
                        diags.push(u::diag("error", "mcp_no_command", "stdio server without a command", Some(name)));
                        continue;
                    };
                    diags.push(u::diag(
                        "warn",
                        "mcp_stdio_in_container",
                        "a stdio server runs inside the dsh container, which has no network: anything it fetches at \
                         start (npx, uvx) fails, and it can read what dsh can. Prefer an http server run in its own \
                         sandbox",
                        Some(name),
                    ));
                    dict(vec![
                        ("serverName", PyValue::str(name)),
                        ("transport", PyValue::str("stdio")),
                        ("command", PyValue::str(command)),
                        (
                            "args",
                            PyValue::List(m.args.iter().map(|a| PyValue::str(a.clone())).collect()),
                        ),
                        ("env", PyValue::Dict(Vec::new())),
                    ])
                }
                "http" => {
                    let Some(url) = m.url.as_deref().filter(|x| !x.is_empty()) else {
                        diags.push(u::diag("error", "mcp_no_url", "http server without a url", Some(name)));
                        continue;
                    };
                    dict(vec![
                        ("serverName", PyValue::str(name)),
                        ("transport", PyValue::str("streamable-http")),
                        ("url", PyValue::str(url)),
                    ])
                }
                other => {
                    diags.push(u::diag(
                        u::unsupported_severity(cfg),
                        "mcp_transport_unsupported",
                        format!(
                            "dsh's MCP client takes stdio or streamable-http; a {other} server is not rendered"
                        ),
                        Some(name),
                    ));
                    continue;
                }
            };
            let row = dict(vec![
                ("id", PyValue::Str(format!("hub-mcp-{name}"))),
                ("name", PyValue::str(MCP_CLIENT)),
                ("config", config),
            ]);
            if yaml_tag_risk(&yamlsafe::dump(&PyValue::List(vec![row.clone()]))) {
                diags.push(u::diag(
                    "error",
                    "yaml_tag_in_mcp",
                    format!(
                        "the server's command, args or url would put '!!', '!<' or a '%' directive line into \
                         {PATCH_FILE}; a YAML tag in a dsh patch can execute JavaScript, so it is not rendered"
                    ),
                    Some(name),
                ));
                continue;
            }
            rows.push((name.to_string(), row));
        }
        rows
    }

    fn ask_container(cfg: &ClientConfig, skills: &[String], run: &dyn CommandRunner) -> Check {
        let check = |ok: bool, detail: String| Check {
            name: "dsh skills (list-skills in the container)".into(),
            ok,
            detail,
        };
        let container = cfg.param("container");
        // `cfg.params.get("docker_context") or ""`
        let ctx_name = cfg.param("docker_context").filter(|v| u::truthy(v));
        match container {
            None | Some(PyValue::None) => {
                return check(false, "params.container is not configured: not checked".into());
            }
            Some(PyValue::Str(c)) if c.is_empty() || c == PLACEHOLDER => {
                return check(false, "params.container is not configured: not checked".into());
            }
            _ => {}
        }
        let Some(container) = container.and_then(PyValue::as_str).filter(|c| PLAIN_RE.is_match(c)) else {
            return check(false, "params.container is not a plain container name: not checked".into());
        };
        let ctx_name = match ctx_name {
            None => "",
            Some(PyValue::Str(c)) if PLAIN_RE.is_match(c) => c.as_str(),
            Some(_) => {
                return check(false, "params.docker_context is not a plain context name: not checked".into());
            }
        };
        let mut argv: Vec<String> = vec!["docker".into()];
        if !ctx_name.is_empty() {
            argv.extend(["--context".to_string(), ctx_name.to_string()]);
        }
        argv.extend(["exec".to_string(), container.to_string()]);
        argv.extend(LIST_SKILLS.iter().map(|s| (*s).to_string()));
        let Ok((rc, out, err)) = run.run(&argv, 60.0) else {
            return check(false, "docker could not be run: not checked".into());
        };
        if rc != 0 {
            return check(false, format!("list-skills exited {rc}: {}", scrub(&err)));
        }
        let Some((sources, complete)) = parse_listing(&out) else {
            return check(false, "list-skills printed no usable JSON: not checked".into());
        };
        let missing: Vec<&String> = skills
            .iter()
            .filter(|s| !sources.get(s.as_str()).is_some_and(|set| set.contains(HUB_SOURCE)))
            .collect();
        let partial = if complete {
            ""
        } else {
            " (dsh reported its discovery as incomplete)"
        };
        if !missing.is_empty() {
            let elsewhere: Vec<&str> = missing
                .iter()
                .filter(|s| sources.contains_key(s.as_str()))
                .take(10)
                .map(|s| s.as_str())
                .collect();
            let more = if elsewhere.is_empty() {
                String::new()
            } else {
                format!("; listed only from another source: {}", elsewhere.join(", "))
            };
            let shown: Vec<&str> = missing.iter().take(10).map(|s| s.as_str()).collect();
            return check(
                false,
                format!(
                    "dsh does not list {} staged skill(s) from its agents home (source {HUB_SOURCE}): {}{more}{partial} \
                     (run deploy/dsh-deliver.sh)",
                    missing.len(),
                    shown.join(", ")
                ),
            );
        }
        check(
            true,
            format!(
                "dsh lists all {} staged skill(s) from its agents home (source {HUB_SOURCE}){partial}",
                skills.len()
            ),
        )
    }
}

/// `_parse_listing(out)`: list-skills output -> ({skill name: {sources}}, complete), or `None` when it is not the expected
/// JSON. Untrusted (it comes from inside the agent's container): size-capped, and only names and sources are read.
pub fn parse_listing(out: &str) -> Option<(BTreeMap<String, HashSet<String>>, bool)> {
    if out.len() > MAX_LIST_OUTPUT && text::char_len(out) > MAX_LIST_OUTPUT {
        return None;
    }
    let lines: Vec<&str> = text::splitlines(out, false)
        .into_iter()
        .filter(|l| !text::strip(l).is_empty())
        .collect();
    let last = lines.last().copied().unwrap_or("");
    // the whole output, else its last line (after a banner); the first that parses wins, whatever it is
    let doc = u::json_loads_raw(out).or_else(|_| u::json_loads_raw(last)).ok()?;
    let PyJson::List(entries) = doc.get("skills")? else {
        return None;
    };
    let mut sources: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    for e in entries {
        if let (Some(PyJson::Str(n)), Some(PyJson::Str(s))) = (e.get("name"), e.get("source")) {
            sources.entry(n.text.clone()).or_default().insert(s.text.clone());
        }
    }
    Some((sources, matches!(doc.get("complete"), Some(PyJson::Bool(true)))))
}

impl ClientAdapter for DeepSeekHarnessAdapter {
    fn id(&self) -> &str {
        ID
    }

    fn display_name(&self) -> &str {
        "DeepSeek Harness"
    }

    fn docs(&self) -> &str {
        "" // the Python class has no docstring (`type(a).__doc__` is None)
    }

    fn caps(&self) -> AdapterCaps {
        AdapterCaps {
            skills: true,
            agents: false,
            instructions: true,
            mcp: true,
            permissions: false,
            egress: false,
            memory: false,
            tool_map: IndexMap::new(),
            model_tiers: IndexMap::new(),
            notes: "Renders to a hub-owned stage dir; delivery into the docker volume dsh reads as DSH_AGENTS_HOME is the \
                    opt-in deploy/dsh-deliver.sh. dsh has no per-command permission rules, file-based subagents or \
                    memory files; model selection stays in dsh."
                .into(),
        }
    }

    fn default_config(&self) -> ClientConfig {
        ClientConfig {
            id: ID.into(),
            adapter: ID.into(),
            display_name: "DeepSeek Harness".into(),
            description: "DeepSeek Harness (dsh). Renders a stage tree; deploy/dsh-deliver.sh copies it into the volume \
                          dsh reads as its agents home."
                .into(),
            roots: [("stage".to_string(), "~/path/to/hub/out/deepseek-harness".to_string())]
                .into_iter()
                .collect(),
            params: dict(vec![
                ("manage", flags_value(&DEFAULT_MANAGE)),
                ("container", PyValue::str(PLACEHOLDER)),
                ("docker_context", PyValue::str("")),
            ]),
            strict: true,
            icon: Some("terminal".into()),
            ..ClientConfig::default()
        }
    }

    fn render(&self, ctx: &RenderContext) -> AdapterResult<RenderResult> {
        let cfg = &ctx.client;
        let mut diags: Vec<Diag> = Vec::new();
        let mut arts = u::skill_artifacts(&ctx.skills, "stage", "skills", &mut diags, "SKILL.md")?;
        let staged: HashSet<String> = arts
            .iter()
            .filter(|a| a.kind == "skill")
            .filter_map(|a| a.source_ids.first().cloned())
            .collect();
        let mut refused: HashSet<String> = HashSet::new();
        // (shared WARNING code, item) that a dsh error replaces
        let mut superseded: HashSet<(&'static str, String)> = HashSet::new();
        let mut sorted: Vec<&SkillItem> = ctx.skills.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for s in sorted {
            if !staged.contains(&s.name) {
                continue; // already refused by the shared checks (reported there)
            }
            let problems = Self::skill_problems(s, &mut diags)?;
            if !problems.is_empty() {
                refused.insert(s.name.clone());
                superseded.extend(problems.iter().filter_map(|c| supersedes(c)).map(|w| (w, s.name.clone())));
            }
        }
        if !refused.is_empty() {
            arts.retain(|a| !(a.kind == "skill" && a.source_ids.first().is_some_and(|n| refused.contains(n))));
        }
        diags.retain(|d| {
            !(d.severity == "warn"
                && superseded
                    .iter()
                    .any(|(code, item)| d.code == *code && d.item.as_deref().unwrap_or("") == item))
        });
        if !ctx.agents.is_empty() {
            diags.push(u::diag(
                u::unsupported_severity(cfg),
                "unsupported_capability",
                "dsh has no file-based subagent definitions; agents are not rendered",
                None,
            ));
        }
        if !ctx.memory.is_empty() {
            diags.push(u::diag(
                u::unsupported_severity(cfg),
                "unsupported_capability",
                "dsh has no memory files; memory is not rendered",
                None,
            ));
        }
        if !ctx.permissions.rules.is_empty() {
            // the floor's own rules are in every client's context, so this cannot be an error without blocking every plan
            diags.push(u::diag(
                "info",
                "permissions_not_rendered",
                "dsh has no per-command permission rules; hub rules are not rendered (contain dsh with its container \
                 instead)",
                None,
            ));
        }
        if !ctx.instructions.is_empty() {
            let content = format!("{}\n", u::render_instructions(&ctx.instructions, true));
            let mut a = Artifact::new("stage", "AGENTS.md", u::to_bytes(&content), "instruction");
            a.source_ids = u::sorted_instructions(&ctx.instructions)
                .iter()
                .map(|i| i.id.clone())
                .collect();
            arts.push(a);
        }
        if !ctx.mcp_servers.is_empty() {
            let rows = Self::mcp_rows(cfg, &ctx.mcp_servers, &mut diags);
            if !rows.is_empty() {
                // `sorted(serverName for each row)`: a list, duplicates kept
                let mut ids: Vec<String> = rows.iter().map(|(n, _)| n.clone()).collect();
                ids.sort();
                let doc = PyValue::List(vec![dict(vec![(
                    "insert",
                    PyValue::List(rows.into_iter().map(|(_, r)| r).collect()),
                )])]);
                let text = yamlsafe::dump(&doc);
                if yaml_tag_risk(&text) {
                    // unreachable: every row was checked; never render it anyway
                    diags.push(u::diag(
                        "error",
                        "yaml_tag_in_patch",
                        format!("{PATCH_FILE} would contain a YAML tag; not rendered"),
                        None,
                    ));
                } else {
                    let mut a = Artifact::new("stage", PATCH_FILE, u::to_bytes(&text), "mcp");
                    a.source_ids = ids;
                    arts.push(a);
                }
            }
        }
        arts.sort_by(|a, b| (&a.root, &a.path).cmp(&(&b.root, &b.path)));
        Ok(RenderResult {
            artifacts: arts,
            diagnostics: diags,
        })
    }

    /// dsh's real content lives in a docker volume the host cannot read; import reads only the hub's own stage.
    fn discover(&self, cfg: &ClientConfig, fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent> {
        let mut res = DiscoveredContent {
            notes: vec!["dsh reads skills from a docker volume; import reads only the hub stage dir".into()],
            ..DiscoveredContent::default()
        };
        res.skills = u::discover_skills(fs, cfg, "stage", "skills", &mut res.notes)?;
        Ok(res)
    }

    fn verify(
        &self,
        cfg: &ClientConfig,
        expected: &[Expected],
        fs: &dyn FileSystem,
        run: &dyn CommandRunner,
    ) -> AdapterResult<Vec<Check>> {
        let mut checks = u::verify_expected(cfg, expected, fs)?;
        let names = u::sorted_set(
            expected
                .iter()
                .filter(|e| e.kind == "skill")
                .filter_map(|e| e.source_ids.first().cloned()),
        );
        checks.push(Self::ask_container(cfg, &names, run));
        Ok(checks)
    }
}

/// The registry entry (Python: module attribute `ADAPTER`).
pub fn adapter() -> Option<Result<Arc<dyn ClientAdapter>, String>> {
    Some(Ok(Arc::new(DeepSeekHarnessAdapter)))
}
