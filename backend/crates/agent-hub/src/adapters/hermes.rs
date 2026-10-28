//! Port of backend/src/agent_hub/adapters/hermes.py: the Hermes Agent adapter (id `hermes-agent`). ONE adapter serves
//! every Hermes instance: everything instance-specific (compose project/service, docker context, binary, source dirs,
//! stage root) is a `ClientConfig.params` entry. Owner: WS-A. Security control S-OBSERVE (with services/observe.rs).
//!
//! Hermes runs in a container the hub cannot reach, so `render` produces a STAGE tree (root `stage`):
//!   skills/<name>/...              delivered on request by `deploy/hermes-deliver.sh` (opt-in, not run by the app)
//!   AGENTS.md                      workspace instructions (staged for review)
//!   config.policy.fragment.yaml    yaml_keys: approvals.deny, agent.disabled_toolsets          (advisory by default)
//!   config.mcp.fragment.yaml       yaml_keys: mcp_servers                                      (advisory by default)
//! `verify` asks the running container (`hermes skills list`, PORTING.md H-1/H-2); `observe` runs the constant
//! [`OBSERVE_PROGRAM`] inside it (H-3) and treats every byte that comes back as hostile.
//! Policy floor: [`BASELINE_DISABLED`] is always rendered (the union with params.disabled_toolsets).

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use hub_core::pathsafe::check_relpath;
use hub_core::py::{os as pyos, text};
use hub_core::value::PyValue;
use hub_core::yamlsafe;
use indexmap::IndexMap;
use regex::Regex;

use super::_util::{self as u, JStr, PyJson};
use super::base::{
    AdapterCaps, AdapterError, AdapterResult, Artifact, Check, ClientAdapter, ClientConfig, CommandRunner, Diag,
    DiscoveredContent, Expected, FileSystem, Files, InstructionItem, McpItem, Observation, ObservedSkill,
    PermissionSet, RenderContext, RenderResult, Rule, SkillItem,
};
use super::claude_code::{flags_value, mcp_item};

pub const ID: &str = "hermes-agent";

/// Toolset names as Hermes Agent spells them: every toolset that reaches an external API, drives a desktop, or grants
/// persistence stays disabled.
pub const BASELINE_DISABLED: [&str; 14] = [
    "web",
    "search",
    "x_search",
    "browser",
    "computer_use",
    "image_gen",
    "bfl",
    "video_gen",
    "video",
    "tts",
    "cronjob",
    "homeassistant",
    "desktop_ui",
    "code_execution",
];
/// capability -> the floor toolset that a rule allowing it would contradict
const CAP_TOOLSET: [(&str, &str); 4] = [
    ("web_fetch", "web"),
    ("web_search", "search"),
    ("browser", "browser"),
    ("image", "image_gen"),
];
pub const TOOL_MAP: [(&str, Option<&str>); 12] = [
    ("read", Some("file")),
    ("write", Some("file")),
    ("edit", Some("file")),
    ("shell", Some("terminal")),
    ("web_fetch", None),
    ("web_search", None),
    ("mcp", Some("mcp")),
    ("subagent", None),
    ("browser", None),
    ("notebook", None),
    ("todo", None),
    ("image", None),
];
pub const DEFAULT_MANAGE: [(&str, bool); 6] = [
    ("skills", true),
    ("agents", false),
    ("instructions", false),
    ("mcp", false),
    ("permissions", false),
    ("memory", false),
];
pub const PLACEHOLDER: &str = "CHANGEME";

pub const MAX_OBSERVE_OUT: usize = 48 * 1024 * 1024;
pub const MAX_FILE: usize = 1024 * 1024;
pub const MAX_TOTAL: usize = 32 * 1024 * 1024;
pub const MAX_FILES: usize = 5000;
pub const MAX_SKILL_NAME: usize = 200;
pub const MAX_SKILL_DESC: usize = 2000;

static SERVICE_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$"));
static USER_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^[A-Za-z0-9_][A-Za-z0-9_.:-]{0,63}$"));
static PY_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^[A-Za-z0-9_./-]{1,200}$"));
static ROOT_RE: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^/[A-Za-z0-9_./-]{0,200}$"));
static NAME_TOKEN: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"[A-Za-z0-9._-]+"));

/// A CONSTANT: nothing (params, content) is ever interpolated into it. The root arrives as `sys.argv[1]`.
pub const OBSERVE_PROGRAM: &str = r#"
import base64, json, os, stat, sys
root = sys.argv[1]
MAXF, MAXT, MAXN = 1048576, 33554432, 5000
files, total, trunc, manifest = {}, 0, False, []
mp = os.path.join(root, ".bundled_manifest")
try:
    st = os.lstat(mp)
    if stat.S_ISREG(st.st_mode) and st.st_size <= MAXF:
        with open(mp, "rb") as fh:
            manifest = [l.strip() for l in fh.read().decode("utf-8", "replace").splitlines() if l.strip()][:20000]
except OSError:
    pass
def walk(d, rel):
    global total, trunc
    try:
        names = sorted(os.listdir(d))
    except OSError:
        return
    for n in names:
        if trunc:
            return
        if n == ".curator_backups" or (n.startswith(".") and not (n == ".archive" and rel == "")):
            continue
        p = os.path.join(d, n)
        r = n if not rel else rel + "/" + n
        try:
            st = os.lstat(p)
        except OSError:
            continue
        if stat.S_ISDIR(st.st_mode):
            walk(p, r)
        elif stat.S_ISREG(st.st_mode):
            if st.st_size > MAXF or total + st.st_size > MAXT or len(files) >= MAXN:
                trunc = True
                continue
            try:
                fd = os.open(p, os.O_RDONLY | os.O_NOFOLLOW)
                with os.fdopen(fd, "rb") as fh:
                    data = fh.read(MAXF + 1)
            except OSError:
                continue
            if len(data) > MAXF:
                trunc = True
                continue
            total += len(data)
            files[r] = base64.b64encode(data).decode("ascii")
if not os.path.isdir(root):
    sys.stderr.write("root is not a directory")
    sys.exit(3)
walk(root, "")
sys.stdout.write(json.dumps({"manifest": manifest, "files": files, "truncated": trunc}))
"#;

fn approval_glob(g: &str) -> String {
    let g = text::strip(g);
    format!(
        "{}{g}{}",
        if g.starts_with('*') { "" } else { "*" },
        if g.ends_with('*') { "" } else { "*" }
    )
}

/// `str(params.get(key, default))`: a present key (even a None value) is stringified, an absent one is the default.
fn param_str(cfg: &ClientConfig, key: &str, default: &str) -> String {
    cfg.param(key).map_or_else(|| default.to_string(), PyValue::py_str)
}

/// `listed_names(output)`: whole-name tokens of `hermes skills list` output (`video` must not match
/// `video-improve-loop`).
pub fn listed_names(output: &str) -> HashSet<String> {
    NAME_TOKEN.find_iter(output).map(|m| m.as_str().to_string()).collect()
}

fn stem(path: &str) -> String {
    let base = pyos::basename(path);
    let head = base.rsplit_once('.').map_or(base, |(h, _)| h);
    head.to_lowercase()
}

/// `HermesAdapter`.
#[derive(Debug, Default, Clone, Copy)]
pub struct HermesAdapter;

impl HermesAdapter {
    fn disabled(cfg: &ClientConfig, perms: &PermissionSet, diags: &mut Vec<Diag>) -> Vec<String> {
        let chosen: Vec<String> = match cfg.param("disabled_toolsets") {
            Some(PyValue::List(l)) => l.iter().map(PyValue::py_str).collect(),
            _ => BASELINE_DISABLED.iter().map(|s| (*s).to_string()).collect(),
        };
        let missing: Vec<&str> = BASELINE_DISABLED
            .iter()
            .copied()
            .filter(|t| !chosen.iter().any(|c| c == t))
            .collect();
        if !missing.is_empty() {
            let shown = format!(
                "[{}]",
                missing.iter().map(|m| text::repr(m)).collect::<Vec<_>>().join(", ")
            );
            diags.push(u::diag(
                "error",
                "floor_weakened",
                format!("params.disabled_toolsets omits baseline toolsets {shown}; rendering the floor regardless"),
                None,
            ));
        }
        for r in &perms.rules {
            let ts = if r.kind == "tool" {
                CAP_TOOLSET.iter().find(|(c, _)| *c == r.matches).map(|(_, t)| *t)
            } else {
                None
            };
            if let Some(ts) = ts
                && r.decision == "allow"
            {
                diags.push(u::diag(
                    "error",
                    "floor_conflict",
                    format!(
                        "rule allows {} but toolset {} is in the Hermes baseline-disabled floor",
                        text::repr(&r.matches),
                        text::repr(ts)
                    ),
                    None,
                ));
            }
        }
        u::sorted_set(chosen.into_iter().chain(BASELINE_DISABLED.iter().map(|s| (*s).to_string())))
    }

    fn deny(perms: &PermissionSet) -> Vec<String> {
        u::sorted_set(
            perms
                .rules
                .iter()
                .filter(|r| r.decision == "deny" && matches!(r.kind.as_str(), "command" | "path_read" | "path_write"))
                .map(|r| approval_glob(&r.matches)),
        )
    }

    fn mcp(items: &[McpItem], diags: &mut Vec<Diag>) -> Vec<(PyValue, PyValue)> {
        let mut out: Vec<(PyValue, PyValue)> = Vec::new();
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
                    "env var names not rendered: the Hermes agent never holds real keys (credential broker)",
                    Some(name),
                ));
            }
            if m.sandbox_profile.as_deref().is_some_and(|p| !p.is_empty()) {
                diags.push(u::diag(
                    "info",
                    "sandbox_profile_ignored",
                    "the container is the sandbox; sandbox wrapping is not applied",
                    Some(name),
                ));
            }
            let entry = if m.transport == "stdio" {
                let Some(command) = m.command.as_deref().filter(|c| !c.is_empty()) else {
                    diags.push(u::diag("error", "mcp_no_command", "stdio server without a command", Some(name)));
                    continue;
                };
                PyValue::Dict(vec![
                    (PyValue::str("command"), PyValue::str(command)),
                    (
                        PyValue::str("args"),
                        PyValue::List(m.args.iter().map(|a| PyValue::str(a.clone())).collect()),
                    ),
                ])
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
                PyValue::Dict(vec![(PyValue::str("url"), PyValue::str(url))])
            };
            out.push((PyValue::str(name), entry));
        }
        out
    }

    fn ask_container(cfg: &ClientConfig, skills: &[String], run: &dyn CommandRunner) -> Check {
        let name = "hermes skills list";
        let check = |ok: bool, detail: String| Check {
            name: name.into(),
            ok,
            detail,
        };
        let project = param_str(cfg, "compose_project", PLACEHOLDER);
        let service = param_str(cfg, "compose_service", "hermes");
        let hbin = param_str(cfg, "hermes_bin", "hermes");
        let ctx_name = param_str(cfg, "docker_context", "");
        let user = param_str(cfg, "exec_user", "");
        let dk = docker_prefix(&ctx_name);
        if project.is_empty() || project == PLACEHOLDER || hbin.is_empty() {
            return check(false, "compose_project/hermes_bin not configured: unverified".into());
        }
        // the same plain-name rules as `observe` (audit F13, deviation D-HERMES-NAMES): nothing from the client YAML or
        // from `docker ps` output reaches the docker command line unless it is a plain name or path
        if !SERVICE_RE.is_match(&project) || !SERVICE_RE.is_match(&service) {
            return check(false, "compose project/service name is not a plain name: unverified".into());
        }
        if !PY_RE.is_match(&hbin) || hbin.starts_with('-') {
            return check(false, "hermes_bin is not a plain path: unverified".into());
        }
        if (!ctx_name.is_empty() && !SERVICE_RE.is_match(&ctx_name)) || (!user.is_empty() && !USER_RE.is_match(&user)) {
            return check(false, "docker_context/exec_user is not a plain name: unverified".into());
        }
        let Ok((rc, out, _)) = run.run(&ps_argv(&dk, &project, &service), 15.0) else {
            return check(false, "docker not available: unverified".into());
        };
        let names: Vec<&str> = text::splitlines(&out, false)
            .into_iter()
            .map(text::strip)
            .filter(|l| SERVICE_RE.is_match(l))
            .collect();
        if rc != 0 || names.is_empty() {
            return check(false, "container not running: unverified".into());
        }
        let mut argv = dk;
        argv.push("exec".into());
        if !user.is_empty() {
            argv.extend(["--user".to_string(), user]);
        }
        argv.extend([names[0].to_string(), hbin, "skills".into(), "list".into()]);
        let Ok((rc, out, err)) = run.run(&argv, 60.0) else {
            return check(false, "docker exec failed: unverified".into());
        };
        if rc != 0 {
            return check(
                false,
                format!("`hermes skills list` exited {rc}: {}", text::take_chars(text::strip(&err), 200)),
            );
        }
        let listed = listed_names(&out);
        let missing: Vec<&String> = skills.iter().filter(|s| !listed.contains(*s)).collect();
        if !missing.is_empty() {
            let shown: Vec<&str> = missing.iter().take(10).map(|s| s.as_str()).collect();
            return check(
                false,
                format!(
                    "container does not list {} skill(s): {}",
                    missing.len(),
                    shown.join(", ")
                ),
            );
        }
        check(true, format!("container lists all {} staged skill(s)", skills.len()))
    }
}

fn docker_prefix(ctx_name: &str) -> Vec<String> {
    let mut dk = vec!["docker".to_string()];
    if !ctx_name.is_empty() {
        dk.extend(["--context".to_string(), ctx_name.to_string()]);
    }
    dk
}

/// PORTING.md H-1: `docker (--context c) ps --format {{.Names}} --filter label=...project=p --filter label=...service=s`.
fn ps_argv(dk: &[String], project: &str, service: &str) -> Vec<String> {
    let mut argv = dk.to_vec();
    argv.extend([
        "ps".to_string(),
        "--format".into(),
        "{{.Names}}".into(),
        "--filter".into(),
        format!("label=com.docker.compose.project={project}"),
        "--filter".into(),
        format!("label=com.docker.compose.service={service}"),
    ]);
    argv
}

fn fail(note: &str) -> Observation {
    Observation {
        complete: false,
        notes: vec![note.to_string()],
        ..Observation::default()
    }
}

/// `_scrub(text)`: untrusted stderr -> a short single-line note without control characters.
pub fn scrub(s: &str) -> String {
    let out: String = text::strip(s)
        .chars()
        .map(|c| if text::is_printable(c) { c } else { ' ' })
        .take(160)
        .collect();
    if out.is_empty() { "no detail".into() } else { out }
}

/// Unicode general category Cf (format), Unicode 15.1 (CPython 3.13's tables).
pub fn is_format(c: char) -> bool {
    matches!(
        c as u32,
        0xad | 0x600..=0x605 | 0x61c | 0x6dd | 0x70f | 0x890 | 0x891 | 0x8e2 | 0x180e | 0x200b..=0x200f
            | 0x202a..=0x202e | 0x2060..=0x2064 | 0x2066..=0x206f | 0xfeff | 0xfff9..=0xfffb | 0x110bd | 0x110cd
            | 0x13430..=0x1343f | 0x1bca0..=0x1bca3 | 0x1d173..=0x1d17a | 0xe0001 | 0xe0020..=0xe007f
    )
}

/// `binascii.a2b_base64(data, strict_mode=True)`: `None` where Python raises `binascii.Error`.
pub fn a2b_base64_strict(data: &[u8]) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(u32::from(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    }
    if data.first() == Some(&b'=') {
        return None; // Leading padding not allowed
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let (mut quad_pos, mut pads, mut leftchar, mut padding_started) = (0u32, 0u32, 0u32, false);
    for (i, &ch) in data.iter().enumerate() {
        if ch == b'=' {
            padding_started = true;
            if quad_pos == 0 {
                return None; // Excess padding not allowed
            }
            pads += 1;
            if quad_pos >= 2 && quad_pos + pads >= 4 {
                if i + 1 < data.len() {
                    return None; // Excess data after padding
                }
                return Some(out);
            }
            continue;
        }
        let v = val(ch)?; // Only base64 data is allowed
        if padding_started {
            return None; // Discontinuous padding not allowed
        }
        pads = 0;
        match quad_pos {
            0 => {
                quad_pos = 1;
                leftchar = v;
            }
            1 => {
                quad_pos = 2;
                out.push(u8::try_from(((leftchar << 2) | (v >> 4)) & 0xff).unwrap_or(0));
                leftchar = v & 0x0f;
            }
            2 => {
                quad_pos = 3;
                out.push(u8::try_from(((leftchar << 4) | (v >> 2)) & 0xff).unwrap_or(0));
                leftchar = v & 0x03;
            }
            _ => {
                quad_pos = 0;
                out.push(u8::try_from(((leftchar << 6) | v) & 0xff).unwrap_or(0));
                leftchar = 0;
            }
        }
    }
    if quad_pos != 0 {
        return None; // Incorrect padding / 1 more than a multiple of 4
    }
    Some(out)
}

/// One accepted file of an observation, or `None` when Python counts it as `bad`.
fn accept_file(rel: &JStr, b64: &PyJson) -> Option<Vec<u8>> {
    if rel.lone_surrogate {
        return None; // rel.encode("utf-8") raises UnicodeEncodeError
    }
    check_relpath(&rel.text).ok()?;
    if rel.text.chars().any(|c| c.is_control() || is_format(c)) {
        return None;
    }
    let PyJson::Str(b) = b64 else {
        return None;
    };
    if b.lone_surrogate || !b.text.is_ascii() {
        return None; // b64.encode("ascii") raises
    }
    a2b_base64_strict(b.text.as_bytes())
}

/// `_parse_observation(out, ctr, root, skills_dest)`: the in-container read, parsed as hostile input (size-capped, JSON
/// object only, strict base64, check_relpath on every path).
pub fn parse_observation(out: &str, ctr: &str, root: &str, skills_dest: &str) -> AdapterResult<Observation> {
    let source = format!("container {ctr}: {root}");
    let refuse = |note: &str| Observation {
        skills: Vec::new(),
        source: source.clone(),
        notes: vec![note.to_string()],
        complete: false,
    };
    if out.len() > MAX_OBSERVE_OUT && text::char_len(out) > MAX_OBSERVE_OUT {
        return Ok(refuse("observation output exceeded the size cap: nothing read"));
    }
    let Ok(doc) = u::json_loads_raw(out) else {
        return Ok(refuse("observation output was not valid JSON: nothing read"));
    };
    let Some(PyJson::Dict(files_in)) = doc.get("files").filter(|_| matches!(doc, PyJson::Dict(_))) else {
        return Ok(refuse("observation output has the wrong shape: nothing read"));
    };
    let mut notes: Vec<String> = Vec::new();
    let mut complete = !matches!(doc.get("truncated"), Some(PyJson::Bool(true)));
    if !complete {
        notes.push("the in-container read hit a size or count cap: the inventory is partial".into());
    }
    let mut manifest: HashSet<String> = HashSet::new();
    if let Some(PyJson::List(lines)) = doc.get("manifest") {
        for line in lines.iter().take(20000) {
            if let PyJson::Str(s) = line {
                let head = text::strip(s.text.split(':').next().unwrap_or_default());
                if !head.is_empty() {
                    manifest.insert(head.to_string());
                }
            }
        }
    }
    let mut files: IndexMap<String, Vec<u8>> = IndexMap::new();
    let mut total = 0usize;
    let mut bad = 0usize;
    for (rel, b64) in files_in {
        if files.len() >= MAX_FILES {
            complete = false;
            notes.push("more files than the cap: the rest were ignored".into());
            break;
        }
        let Some(data) = accept_file(rel, b64) else {
            bad += 1;
            continue;
        };
        if data.len() > MAX_FILE || total + data.len() > MAX_TOTAL {
            bad += 1;
            continue;
        }
        total += data.len();
        files.insert(rel.text.clone(), data);
    }
    if bad > 0 {
        complete = false;
        notes.push(format!(
            "{bad} file(s) were rejected (bad path, bad encoding or over a cap)"
        ));
    }
    // a path that is a file in one entry and a directory in another cannot be materialised: drop both sides
    let mut prefixes: HashSet<String> = HashSet::new();
    for r in files.keys() {
        let parts: Vec<&str> = r.split('/').collect();
        for i in 1..parts.len() {
            prefixes.insert(parts[..i].join("/"));
        }
    }
    let clash: Vec<String> = files.keys().filter(|r| prefixes.contains(*r)).cloned().collect();
    if !clash.is_empty() {
        complete = false;
        notes.push(format!(
            "{} file(s) collide with a directory of the same path and were skipped",
            clash.len()
        ));
        for r in &clash {
            files.shift_remove(r);
        }
    }
    let mut skill_dirs: Vec<String> = files
        .keys()
        .filter(|rel| *rel == "SKILL.md" || rel.ends_with("/SKILL.md"))
        .map(|rel| {
            if rel.contains('/') {
                rel[..rel.len() - "/SKILL.md".len()].to_string()
            } else {
                String::new()
            }
        })
        .collect();
    skill_dirs.sort();
    let mut owned: IndexMap<String, Files> = skill_dirs
        .iter()
        .filter(|d| !d.is_empty())
        .map(|d| (d.clone(), Files::new()))
        .collect();
    if skill_dirs.iter().any(String::is_empty) {
        notes.push("a SKILL.md at the skills root itself was ignored".into());
    }
    for (rel, data) in &files {
        let parts: Vec<&str> = rel.split('/').collect();
        for i in (1..parts.len()).rev() {
            let d = parts[..i].join("/");
            if let Some(slot) = owned.get_mut(&d) {
                slot.insert(parts[i..].join("/"), data.clone());
                break;
            }
        }
    }
    let hub_cat = if skills_dest.trim_matches('/').is_empty() {
        ""
    } else {
        let t = skills_dest.trim_end_matches('/');
        t.rsplit_once('/').map_or(t, |(_, last)| last)
    };
    let mut keys: Vec<String> = owned.keys().cloned().collect();
    keys.sort();
    let mut skills = Vec::new();
    for d in keys {
        let sf = owned.shift_remove(&d).unwrap_or_default();
        let leaf = d.rsplit_once('/').map_or(d.as_str(), |(_, l)| l).to_string();
        let md = sf.get("SKILL.md").map(|b| u::decode_replace(b)).unwrap_or_default();
        let fm = match yamlsafe::split_frontmatter_exact(&md) {
            Ok((meta, _)) => PyValue::Dict(meta),
            // `except (yamlsafe.YamlError, ValueError, RecursionError)`
            Err(
                yamlsafe::LoadError::Yaml(_)
                | yamlsafe::LoadError::Python {
                    exc_type: "ValueError" | "UnicodeDecodeError" | "RecursionError",
                    ..
                },
            ) => PyValue::Dict(Vec::new()),
            Err(yamlsafe::LoadError::Python { exc_type, message }) => return Err(AdapterError::new(exc_type, message)),
        };
        let mut name = leaf.clone();
        if let Some(PyValue::Str(n)) = fm.get("name")
            && !text::strip(n).is_empty()
        {
            name = text::take_chars(text::strip(n), MAX_SKILL_NAME).to_string();
        }
        let desc = u::cap_str(fm.get("description"), MAX_SKILL_DESC);
        let parts: Vec<&str> = d.split('/').collect();
        let origin = if parts.contains(&".archive") {
            "archived"
        } else if parts[0] == "lab" {
            "lab_sync"
        } else if !hub_cat.is_empty() && parts[0] == hub_cat {
            "hub"
        } else if manifest.contains(&leaf) {
            "shipped"
        } else {
            "agent"
        };
        skills.push(ObservedSkill {
            path: d,
            name,
            description: desc,
            origin: origin.into(),
            files: sf,
        });
    }
    Ok(Observation {
        skills,
        source,
        notes,
        complete,
    })
}

fn skills_from_dir(fs: &dyn FileSystem, top: &str, notes: &mut Vec<String>) -> AdapterResult<Vec<SkillItem>> {
    let mut out = Vec::new();
    if !fs.is_dir(top) {
        return Ok(out);
    }
    for name in u::listdir_sorted(fs, top)? {
        let d = pyos::join(top, &name);
        if !fs.is_dir(&d) || !u::valid_name(&name) {
            continue;
        }
        let mut files = u::read_tree(fs, &d, u::MAX_DISCOVER_FILES, notes)?;
        files.retain(|k, _| !k.ends_with(".pyc"));
        let Some(skill_md) = files.get("SKILL.md") else {
            notes.push(format!("skipped {}: no SKILL.md", text::repr(&name)));
            continue;
        };
        let (fm, _) = u::parse_frontmatter(&u::decode_replace(skill_md))?;
        out.push(SkillItem {
            name,
            description: u::cap_str(fm.get("description"), u::MAX_DESC_ERR),
            files,
            ..SkillItem::default()
        });
    }
    Ok(out)
}

fn read_config(fs: &dyn FileSystem, path: &str, res: &mut DiscoveredContent) -> AdapterResult<()> {
    let Some(raw) = u::read_capped(fs, path, &mut res.notes, u::MAX_READ_FILE_BYTES) else {
        return Ok(());
    };
    let doc = match yamlsafe::load_exact(&u::decode_replace(&raw), u::MAX_READ_FILE_BYTES) {
        Ok(d) => d,
        Err(yamlsafe::LoadError::Yaml(exc)) => {
            let note = format!("{} rejected by the safe YAML loader: {exc}", pyos::basename(path));
            res.notes.push(text::take_chars(&note, 200).to_string());
            return Ok(());
        }
        Err(yamlsafe::LoadError::Python { exc_type, message }) => return Err(AdapterError::new(exc_type, message)),
    };
    if !matches!(doc, PyValue::Dict(_)) {
        return Ok(());
    }
    let servers = match doc.get("mcp_servers") {
        Some(PyValue::Dict(d)) => u::sorted_items(d)?,
        _ => Vec::new(),
    };
    for (name, e) in servers {
        let key = name.py_str();
        if !matches!(e, PyValue::Dict(_)) || !u::valid_name(&key) {
            res.notes.push(format!("skipped mcp server {}", name.repr()));
            continue;
        }
        let env = match e.get("env") {
            Some(PyValue::Dict(env)) => {
                let mut v: Vec<String> = env.iter().map(|(k, _)| k.py_str()).collect();
                v.sort();
                v
            }
            _ => Vec::new(),
        };
        if e.get("url").is_some_and(u::truthy) {
            res.mcp_servers.push(McpItem {
                url: Some(u::cap_str(e.get("url"), 2000)),
                env_names: env,
                ..mcp_item(&key, "http")
            });
        } else if e.get("command").is_some_and(u::truthy) {
            let args = match e.get("args") {
                None => Vec::new(),
                Some(a) => u::py_iter(a)?
                    .iter()
                    .filter(|a| matches!(a, PyValue::Str(_) | PyValue::Int(_) | PyValue::BigInt(_) | PyValue::Bool(_)))
                    .map(PyValue::py_str)
                    .collect(),
            };
            res.mcp_servers.push(McpItem {
                command: Some(u::cap_str(e.get("command"), 2000)),
                args,
                env_names: env,
                ..mcp_item(&key, "stdio")
            });
        }
    }
    let deny = match doc.get("approvals") {
        Some(approvals @ PyValue::Dict(_)) if u::truthy(approvals) => approvals.get("deny").cloned(),
        _ => None,
    };
    let mut rules: Vec<Rule> = Vec::new();
    if let Some(deny) = deny.filter(u::truthy) {
        for g in u::py_iter(&deny)? {
            if let PyValue::Str(s) = g {
                rules.push(Rule {
                    kind: "command".into(),
                    matches: s,
                    decision: "deny".into(),
                    reason: None,
                });
            }
        }
    }
    if !rules.is_empty() {
        let mut all = res.permissions.take().map(|p| p.rules).unwrap_or_default();
        all.extend(rules);
        res.permissions = Some(PermissionSet {
            rules: all,
            ..PermissionSet::default()
        });
    }
    let dis = match doc.get("agent") {
        Some(agent @ PyValue::Dict(_)) if u::truthy(agent) => agent.get("disabled_toolsets"),
        _ => None,
    };
    if let Some(PyValue::List(l)) = dis {
        res.notes.push(format!(
            "agent.disabled_toolsets floor ({} toolsets) noted, not imported: it is enforced by the adapter, never an \
             allow",
            l.len()
        ));
    }
    Ok(())
}

impl ClientAdapter for HermesAdapter {
    fn id(&self) -> &str {
        ID
    }

    fn display_name(&self) -> &str {
        "Hermes Agent"
    }

    fn docs(&self) -> &str {
        "" // the Python class has no docstring
    }

    fn caps(&self) -> AdapterCaps {
        AdapterCaps {
            skills: true,
            agents: false,
            instructions: true,
            mcp: true,
            permissions: true,
            egress: false,
            memory: false,
            tool_map: TOOL_MAP.iter().map(|(c, t)| ((*c).to_string(), t.map(str::to_string))).collect(),
            model_tiers: IndexMap::new(),
            notes: "Renders to a hub-owned stage dir; delivery into the container volume is the opt-in \
                    deploy/hermes-deliver.sh. Hermes has no per-agent definitions and model selection is owned by \
                    the deployment."
                .into(),
        }
    }

    fn default_config(&self) -> ClientConfig {
        ClientConfig {
            id: "hermes".into(),
            adapter: ID.into(),
            display_name: "Hermes Agent".into(),
            description: "Hermes Agent. Renders a stage tree; skills are delivered by deploy/hermes-deliver.sh.".into(),
            roots: [("stage".to_string(), "~/path/to/hub/out/hermes".to_string())].into_iter().collect(),
            params: PyValue::Dict(vec![
                (PyValue::str("manage"), flags_value(&DEFAULT_MANAGE)),
                (PyValue::str("hermes_bin"), PyValue::str("hermes")),
                (PyValue::str("compose_project"), PyValue::str(PLACEHOLDER)),
                (PyValue::str("compose_service"), PyValue::str("hermes")),
                (PyValue::str("source_root"), PyValue::str("~/path/to/project")),
                (PyValue::str("source_dirs"), PyValue::List(Vec::new())),
            ]),
            strict: true,
            icon: Some("bot".into()),
            ..ClientConfig::default()
        }
    }

    fn render(&self, ctx: &RenderContext) -> AdapterResult<RenderResult> {
        let cfg = &ctx.client;
        let mut diags = Vec::new();
        let mut arts = u::skill_artifacts(&ctx.skills, "stage", "skills", &mut diags, "SKILL.md")?;
        if !ctx.agents.is_empty() {
            diags.push(u::diag(
                u::unsupported_severity(cfg),
                "unsupported_capability",
                "Hermes has no per-agent definitions; agents are not rendered",
                None,
            ));
        }
        if !ctx.memory.is_empty() {
            diags.push(u::diag(
                u::unsupported_severity(cfg),
                "unsupported_capability",
                "memory is not rendered for Hermes",
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
            let servers = Self::mcp(&ctx.mcp_servers, &mut diags);
            let ids = u::sorted_set(servers.iter().map(|(k, _)| k.py_str()));
            let doc = PyValue::Dict(vec![(PyValue::str("mcp_servers"), PyValue::Dict(servers))]);
            let mut a = Artifact::new("stage", "config.mcp.fragment.yaml", u::dump_yaml(&doc), "mcp");
            a.source_ids = ids;
            a.merge = "yaml_keys".into();
            a.managed_keys = vec!["mcp_servers".into()];
            arts.push(a);
        }
        // the policy fragment is ALWAYS rendered: the floor must be visible in the diff even when no rules are enabled
        let deny = Self::deny(&ctx.permissions);
        let disabled = Self::disabled(cfg, &ctx.permissions, &mut diags);
        let strs = |v: Vec<String>| PyValue::List(v.into_iter().map(PyValue::Str).collect());
        let doc = PyValue::Dict(vec![
            (
                PyValue::str("approvals"),
                PyValue::Dict(vec![(PyValue::str("deny"), strs(deny))]),
            ),
            (
                PyValue::str("agent"),
                PyValue::Dict(vec![(PyValue::str("disabled_toolsets"), strs(disabled))]),
            ),
        ]);
        let mut ids = u::sorted_set(ctx.permissions.rules.iter().map(|r| format!("{}:{}", r.kind, r.matches)));
        if ids.is_empty() {
            ids = vec!["floor".into()];
        }
        let mut a = Artifact::new("stage", "config.policy.fragment.yaml", u::dump_yaml(&doc), "rule");
        a.source_ids = ids;
        a.merge = "yaml_keys".into();
        a.managed_keys = vec!["agent.disabled_toolsets".into(), "approvals.deny".into()];
        arts.push(a);
        arts.sort_by(|a, b| (&a.root, &a.path).cmp(&(&b.root, &b.path)));
        Ok(RenderResult {
            artifacts: arts,
            diagnostics: diags,
        })
    }

    /// Reads the client's REAL sources (params.source_dirs, relative to params.source_root) plus the hub stage dir. The
    /// container volume is not visible to the host. Read-only; env VALUES are never read into the result.
    fn discover(&self, cfg: &ClientConfig, fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent> {
        let mut res = DiscoveredContent::default();
        let home = std::env::var("HOME").unwrap_or_default();
        let lab = pyos::expanduser(&param_str(cfg, "source_root", ""), &home);
        let sources: Vec<PyValue> = match cfg.param("source_dirs") {
            Some(PyValue::List(l)) => l.clone(),
            _ => Vec::new(),
        };
        for src in &sources {
            let kind = src.get("kind").and_then(PyValue::as_str);
            let rel = src.get("path").and_then(PyValue::as_str);
            let (Some(kind @ ("skills" | "instruction" | "config")), Some(rel), PyValue::Dict(_)) = (kind, rel, src)
            else {
                res.notes.push(format!("ignored malformed source_dirs entry {}", src.repr()));
                continue;
            };
            let root = src.get("root");
            let base = match root.filter(|r| u::truthy(r)) {
                Some(r) => cfg.roots.get(&r.py_str()).cloned().unwrap_or_default(),
                None => lab.clone(),
            };
            if base.is_empty() {
                let shown = root.map_or_else(|| "None".to_string(), PyValue::repr);
                res.notes.push(format!("source root {shown} is not configured"));
                continue;
            }
            let path = if rel.starts_with('/') {
                rel.to_string()
            } else {
                pyos::join(&base, rel)
            };
            if path.split('/').any(|p| p == "..") {
                res.notes
                    .push(format!("ignored source path with '..': {}", text::repr(rel)));
                continue;
            }
            if !fs.exists(&path) {
                res.notes.push(format!("source not found: {rel}"));
                continue;
            }
            match kind {
                "skills" => {
                    let found = skills_from_dir(fs, &path, &mut res.notes)?;
                    res.skills.extend(found);
                }
                "instruction" => {
                    let Some(raw) = u::read_capped(fs, &path, &mut res.notes, u::MAX_READ_FILE_BYTES) else {
                        continue;
                    };
                    let st = stem(&path);
                    res.instructions.push(InstructionItem {
                        id: if u::valid_name(&st) { st } else { "workspace".into() },
                        title: pyos::basename(&path).to_string(),
                        order: i64::try_from(res.instructions.len()).unwrap_or(i64::MAX),
                        body: u::decode_replace(&raw),
                    });
                }
                _ => read_config(fs, &path, &mut res)?,
            }
        }
        let have: HashSet<String> = res.skills.iter().map(|s| s.name.clone()).collect();
        let staged = u::discover_skills(fs, cfg, "stage", "skills", &mut res.notes)?;
        res.skills.extend(staged.into_iter().filter(|s| !have.contains(&s.name)));
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

    /// Read the skills the RUNNING container really has (read-only; never starts a container). The interpreter inside the
    /// container and the filesystem it reads are controlled by the (untrusted) agent, so the parse-side caps and checks
    /// in [`parse_observation`] are the defence, not the constant program.
    fn observe(&self, cfg: &ClientConfig, _fs: &dyn FileSystem, run: &dyn CommandRunner) -> AdapterResult<Observation> {
        let project = param_str(cfg, "compose_project", PLACEHOLDER);
        let service = param_str(cfg, "compose_service", "hermes");
        let python = param_str(cfg, "observe_python", "python3");
        let root = param_str(cfg, "observe_root", "/opt/data/skills");
        let ctx_name = param_str(cfg, "docker_context", "");
        let user = param_str(cfg, "exec_user", "");
        if project.is_empty() || project == PLACEHOLDER {
            return Ok(fail("compose_project is not configured: nothing observed"));
        }
        let mut services = vec![service];
        if let Some(PyValue::List(extra)) = cfg.param("observe_services") {
            services.extend(extra.iter().filter_map(PyValue::as_str).map(str::to_string));
        }
        if !services.iter().all(|s| SERVICE_RE.is_match(s)) || !SERVICE_RE.is_match(&project) {
            return Ok(fail("compose project/service name is not a plain name: nothing observed"));
        }
        if !PY_RE.is_match(&python) {
            return Ok(fail("observe_python is not a plain path: nothing observed"));
        }
        if !ROOT_RE.is_match(&root) || root.split('/').any(|p| p == "..") {
            return Ok(fail("observe_root is not a plain absolute path: nothing observed"));
        }
        if (!ctx_name.is_empty() && !SERVICE_RE.is_match(&ctx_name)) || (!user.is_empty() && !USER_RE.is_match(&user)) {
            return Ok(fail("docker_context/exec_user is not a plain name: nothing observed"));
        }
        let dk = docker_prefix(&ctx_name);
        let mut ctr = String::new();
        for svc in &services {
            let Ok((rc, out, _)) = run.run(&ps_argv(&dk, &project, svc), 15.0) else {
                return Ok(fail("docker is not available: nothing observed"));
            };
            let names: Vec<&str> = text::splitlines(&out, false)
                .into_iter()
                .map(text::strip)
                .filter(|l| SERVICE_RE.is_match(l))
                .collect();
            if rc == 0 && !names.is_empty() {
                ctr = names[0].to_string();
                break;
            }
        }
        if ctr.is_empty() {
            return Ok(fail(
                "no running container for the configured compose project/service: nothing observed (none is started)",
            ));
        }
        let mut argv = dk;
        argv.push("exec".into());
        if !user.is_empty() {
            argv.extend(["--user".to_string(), user]);
        }
        argv.extend([ctr.clone(), python, "-c".into(), OBSERVE_PROGRAM.into(), root.clone()]);
        let Ok((rc, out, err)) = run.run(&argv, 120.0) else {
            return Ok(fail("docker exec failed to start: nothing observed"));
        };
        if rc != 0 {
            return Ok(fail(&format!("in-container read exited {rc}: {}", scrub(&err))));
        }
        parse_observation(&out, &ctr, &root, &param_str(cfg, "skills_dest", ""))
    }
}

/// The registry entry (Python: module attribute `ADAPTER`).
pub fn adapter() -> Option<Result<Arc<dyn ClientAdapter>, String>> {
    Some(Ok(Arc::new(HermesAdapter)))
}
