//! Port of backend/tests/adapters/fakes.py (offline fakes + sample content shared by the adapter tests: no real file,
//! process or network access), plus the decoding of the Python-generated golden file tests/golden/adapters.json
//! (backend/parity/gen/gen_adapter_golden.py). Owner: WS-A.
#![allow(dead_code, unreachable_pub, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::sync::{LazyLock, Mutex};

use agent_hub::adapters::base::{
    AgentItem, Artifact, ClientConfig, CommandRunner, DiscoveredContent, Expected, FileSystem, Files, InstructionItem,
    McpItem, MemoryItem, PermissionSet, RenderContext, RenderResult, Rule, SkillItem,
};
use base64::Engine as _;
use hub_core::value::PyValue;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

// ---- FakeFS / FakeRunner ---------------------------------------------------------------------------------------------
/// Dict-backed FileSystem: keys are absolute posix paths. Like Python's FakeFS it offers no `size()`.
#[derive(Default)]
pub struct FakeFs {
    pub files: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl FakeFs {
    pub fn new<K: Into<String>, V: Into<Vec<u8>>>(files: impl IntoIterator<Item = (K, V)>) -> Self {
        Self {
            files: Mutex::new(files.into_iter().map(|(k, v)| (k.into(), v.into())).collect()),
        }
    }
    pub fn set(&self, path: &str, data: &[u8]) {
        self.files.lock().unwrap().insert(path.into(), data.to_vec());
    }
    pub fn remove(&self, path: &str) {
        self.files.lock().unwrap().remove(path);
    }
}

impl FileSystem for FakeFs {
    fn read_bytes(&self, path: &str) -> io::Result<Vec<u8>> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.to_string()))
    }
    fn exists(&self, path: &str) -> bool {
        self.files.lock().unwrap().contains_key(path) || self.is_dir(path)
    }
    fn is_dir(&self, path: &str) -> bool {
        let p = format!("{}/", path.trim_end_matches('/'));
        self.files.lock().unwrap().keys().any(|k| k.starts_with(&p))
    }
    fn listdir(&self, path: &str) -> io::Result<Vec<String>> {
        let p = format!("{}/", path.trim_end_matches('/'));
        let names: BTreeSet<String> = self
            .files
            .lock()
            .unwrap()
            .keys()
            .filter_map(|k| k.strip_prefix(&p))
            .map(|rest| rest.split('/').next().unwrap_or_default().to_string())
            .collect();
        Ok(names.into_iter().collect())
    }
    fn size(&self, _path: &str) -> Option<io::Result<u64>> {
        None
    }
}

/// FileSystem that offers size(): oversize files must NOT be read at all.
pub struct SizedFs {
    pub inner: FakeFs,
    pub sizes: BTreeMap<String, u64>,
    pub read: Mutex<Vec<String>>,
}

impl FileSystem for SizedFs {
    fn read_bytes(&self, path: &str) -> io::Result<Vec<u8>> {
        self.read.lock().unwrap().push(path.into());
        self.inner.read_bytes(path)
    }
    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }
    fn is_dir(&self, path: &str) -> bool {
        self.inner.is_dir(path)
    }
    fn listdir(&self, path: &str) -> io::Result<Vec<String>> {
        self.inner.listdir(path)
    }
    fn size(&self, path: &str) -> Option<io::Result<u64>> {
        Some(match self.sizes.get(path) {
            Some(n) => Ok(*n),
            None => self.inner.read_bytes(path).map(|b| b.len() as u64),
        })
    }
}

/// Scripted CommandRunner: records every argv; answers from `responses` (then `(0, "", "")`), or raises OSError.
#[derive(Default)]
pub struct FakeRunner {
    pub calls: Mutex<Vec<Vec<String>>>,
    pub timeouts: Mutex<Vec<f64>>,
    pub responses: Mutex<VecDeque<(i32, String, String)>>,
    pub raises: bool,
}

impl FakeRunner {
    pub fn new(responses: &[(i32, &str, &str)]) -> Self {
        Self {
            responses: Mutex::new(
                responses
                    .iter()
                    .map(|(rc, o, e)| (*rc, (*o).to_string(), (*e).to_string()))
                    .collect(),
            ),
            ..Self::default()
        }
    }
    pub fn raising() -> Self {
        Self {
            raises: true,
            ..Self::default()
        }
    }
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, argv: &[String], timeout: f64) -> io::Result<(i32, String, String)> {
        self.calls.lock().unwrap().push(argv.to_vec());
        self.timeouts.lock().unwrap().push(timeout);
        if self.raises {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no docker"));
        }
        Ok(self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or((0, String::new(), String::new())))
    }
}

// ---- sample content ----------------------------------------------------------------------------------------------------
pub const DESC: &str = "Does a thing. Use when asked.";

pub fn skill(name: &str) -> SkillItem {
    skill_with(name, DESC, &[])
}

pub fn skill_with(name: &str, desc: &str, extra: &[(&str, &[u8])]) -> SkillItem {
    let mut files = Files::new();
    files.insert(
        "SKILL.md".into(),
        format!("---\nname: {name}\ndescription: {desc}\n---\n\n# {name}\n\nSteps.\n").into_bytes(),
    );
    for (k, v) in extra {
        files.insert((*k).to_string(), v.to_vec());
    }
    SkillItem {
        name: name.into(),
        description: desc.into(),
        files,
        ..SkillItem::default()
    }
}

pub fn agent(name: &str, desc: &str, caps: &[&str], body: &str) -> AgentItem {
    AgentItem {
        name: name.into(),
        description: desc.into(),
        capabilities: caps.iter().map(|c| (*c).to_string()).collect(),
        model_tier: None,
        mode: "subagent".into(),
        read_only: false,
        body: body.into(),
    }
}

pub fn instruction(id: &str, title: &str, order: i64, body: &str) -> InstructionItem {
    InstructionItem {
        id: id.into(),
        title: title.into(),
        order,
        body: body.into(),
    }
}

pub fn mcp(name: &str, transport: &str) -> McpItem {
    McpItem {
        name: name.into(),
        transport: transport.into(),
        scan_status: "unscanned".into(),
        ..McpItem::default()
    }
}

pub fn rule(kind: &str, m: &str, decision: &str) -> Rule {
    Rule {
        kind: kind.into(),
        matches: m.into(),
        decision: decision.into(),
        reason: None,
    }
}

pub fn rules(rs: &[(&str, &str, &str)]) -> PermissionSet {
    PermissionSet {
        rules: rs.iter().map(|(k, m, d)| rule(k, m, d)).collect(),
        ..PermissionSet::default()
    }
}

/// `sample_ctx(cfg)`.
pub fn sample_ctx(cfg: &ClientConfig) -> RenderContext {
    let mut reviewer = agent(
        "reviewer",
        "Reviews changes; read-only.",
        &["read", "shell"],
        "You review.\n",
    );
    reviewer.model_tier = Some("standard".into());
    reviewer.read_only = true;
    let mut builder = agent(
        "builder",
        "Builds things.",
        &["read", "write", "edit", "shell", "web_fetch"],
        "You build.\n",
    );
    builder.model_tier = Some("fast".into());
    let serena = McpItem {
        command: Some("serena".into()),
        args: vec!["start-mcp-server".into()],
        env_names: vec!["SERENA_HOME".into()],
        sandbox_profile: Some("sandbox".into()),
        scan_status: "clean".into(),
        ..mcp("serena", "stdio")
    };
    let docs = McpItem {
        url: Some("http://mcpo:8000/docs".into()),
        ..mcp("docs", "http")
    };
    RenderContext {
        client: cfg.clone(),
        skills: vec![
            skill_with("example-skill", DESC, &[("scripts/run.sh", b"#!/bin/sh\necho hi\n")]),
            skill("example-other-skill"),
        ],
        agents: vec![reviewer, builder],
        instructions: vec![
            instruction("b-second", "Second", 2, "Two."),
            instruction("a-first", "First", 1, "One."),
        ],
        mcp_servers: vec![serena, docs],
        memory: vec![MemoryItem {
            id: "m1".into(),
            kind: "project".into(),
            title: "Fact".into(),
            body: "Body.".into(),
        }],
        permissions: rules(&[
            ("path_read", "**/private-keys/**", "deny"),
            ("command", "docker compose *", "allow"),
            ("command", "sudo *", "ask"),
            ("tool", "web_fetch", "ask"),
            ("egress_host", "pypi.org", "allow"),
        ]),
        endpoints: [("router".to_string(), "http://127.0.0.1:4000/v1".to_string())]
            .into_iter()
            .collect(),
    }
}

/// `sample_ctx(cfg, skills=[], agents=[], instructions=[], mcp_servers=[], memory=[], permissions=PermissionSet())`.
pub fn empty_ctx(cfg: &ClientConfig) -> RenderContext {
    RenderContext {
        skills: vec![],
        agents: vec![],
        instructions: vec![],
        mcp_servers: vec![],
        memory: vec![],
        permissions: PermissionSet::default(),
        ..sample_ctx(cfg)
    }
}

/// `shuffled(ctx)`: every list reversed (permissions kept).
pub fn shuffled(ctx: &RenderContext) -> RenderContext {
    let mut c = ctx.clone();
    c.skills.reverse();
    c.agents.reverse();
    c.instructions.reverse();
    c.mcp_servers.reverse();
    c.memory.reverse();
    c
}

/// `tree(res, base)`: render result -> fake filesystem entries under `base` (own artifacts only).
pub fn tree(res: &RenderResult, base: &str) -> Vec<(String, Vec<u8>)> {
    res.artifacts
        .iter()
        .filter(|a| a.merge == "own")
        .map(|a| (format!("{base}/{}", a.path), a.content.clone()))
        .collect()
}

/// Every artifact (whatever its merge mode) as files under `base`.
pub fn all_files(res: &RenderResult, base: &str) -> Vec<(String, Vec<u8>)> {
    res.artifacts
        .iter()
        .map(|a| (format!("{base}/{}", a.path), a.content.clone()))
        .collect()
}

pub fn expected(a: &Artifact) -> Expected {
    Expected {
        root: a.root.clone(),
        path: a.path.clone(),
        sha256: agent_hub::adapters::_util::sha256_hex(&a.content),
        kind: a.kind.clone(),
        source_ids: a.source_ids.clone(),
        merge: "own".into(),
        managed_keys: vec![],
    }
}

pub fn text(a: &Artifact) -> String {
    String::from_utf8_lossy(&a.content).into_owned()
}

pub fn art<'a>(res: &'a RenderResult, path: &str) -> &'a Artifact {
    res.artifacts
        .iter()
        .find(|a| a.path == path)
        .unwrap_or_else(|| panic!("no artifact {path}"))
}

pub fn codes(res: &RenderResult) -> BTreeSet<String> {
    res.diagnostics.iter().map(|d| d.code.clone()).collect()
}

pub fn has_diag(res: &RenderResult, code: &str, sev: Option<&str>, item: Option<&str>) -> bool {
    res.diagnostics.iter().any(|d| {
        d.code == code && sev.is_none_or(|s| d.severity == s) && item.is_none_or(|i| d.item.as_deref() == Some(i))
    })
}

/// A Python dict value from `(key, value)` pairs.
pub fn dict(pairs: Vec<(&str, PyValue)>) -> PyValue {
    PyValue::Dict(pairs.into_iter().map(|(k, v)| (PyValue::str(k), v)).collect())
}

pub fn s(x: &str) -> PyValue {
    PyValue::str(x)
}

pub fn list(items: &[&str]) -> PyValue {
    PyValue::List(items.iter().map(|x| PyValue::str(*x)).collect())
}

/// `cfg.params[key] = value`.
pub fn set_param(cfg: &mut ClientConfig, key: &str, value: PyValue) {
    let PyValue::Dict(d) = &mut cfg.params else {
        cfg.params = dict(vec![(key, value)]);
        return;
    };
    match d.iter_mut().find(|(k, _)| k.as_str() == Some(key)) {
        Some(slot) => slot.1 = value,
        None => d.push((PyValue::str(key), value)),
    }
}

pub fn del_param(cfg: &mut ClientConfig, key: &str) {
    if let PyValue::Dict(d) = &mut cfg.params {
        d.retain(|(k, _)| k.as_str() != Some(key));
    }
}

pub fn roots(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

/// YAML text -> value (`yaml.safe_load`).
pub fn yaml_load(b: &[u8]) -> PyValue {
    hub_core::yaml::load(&String::from_utf8_lossy(b), hub_core::yaml::Loader::Safe).unwrap()
}

pub fn json_load(b: &[u8]) -> Value {
    serde_json::from_slice(b).unwrap()
}

// ---- golden decoding ---------------------------------------------------------------------------------------------------
pub static GOLDEN: LazyLock<Value> = LazyLock::new(|| {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/adapters.json");
    serde_json::from_slice(&std::fs::read(path).expect("golden file")).expect("golden json")
});

/// The DeepSeek Harness goldens (backend/parity/gen/gen_dsh_golden.py; same encoding as [`GOLDEN`]).
pub static DSH_GOLDEN: LazyLock<Value> = LazyLock::new(|| {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/deepseek_harness.json");
    serde_json::from_slice(&std::fs::read(path).expect("golden file")).expect("golden json")
});

fn b64d(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s).expect("base64")
}

fn b64e(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// The generator's `enc(value)` decoding.
pub fn dec(v: &Value) -> PyValue {
    match v {
        Value::Object(m) if m.len() == 1 && m.keys().next().is_some_and(|k| k.starts_with('$')) => {
            let (k, x) = m.iter().next().unwrap();
            match k.as_str() {
                "$bytes" => PyValue::Bytes(b64d(x.as_str().unwrap())),
                "$int" => PyValue::BigInt(x.as_str().unwrap().into()),
                "$float" => PyValue::Float(match x.as_str().unwrap() {
                    "nan" => f64::NAN,
                    "inf" => f64::INFINITY,
                    _ => f64::NEG_INFINITY,
                }),
                "$date" => {
                    let s = x.as_str().unwrap();
                    PyValue::Date {
                        year: s[..4].parse().unwrap(),
                        month: s[5..7].parse().unwrap(),
                        day: s[8..10].parse().unwrap(),
                    }
                }
                "$set" => PyValue::Set(x.as_array().unwrap().iter().map(dec).collect()),
                "$dict" => PyValue::Dict(x.as_array().unwrap().iter().map(|p| (dec(&p[0]), dec(&p[1]))).collect()),
                other => panic!("unknown tag {other}"),
            }
        }
        Value::Object(m) => PyValue::Dict(m.iter().map(|(k, x)| (PyValue::str(k.clone()), dec(x))).collect()),
        Value::Array(a) => PyValue::List(a.iter().map(dec).collect()),
        other => PyValue::from_json(other),
    }
}

/// The generator's `enc(value)`.
pub fn enc(v: &PyValue) -> Value {
    match v {
        PyValue::None => Value::Null,
        PyValue::Bool(b) => Value::Bool(*b),
        PyValue::Int(i) => json!(i),
        PyValue::BigInt(s) => json!({"$int": s}),
        PyValue::Float(f) if f.is_nan() => json!({"$float": "nan"}),
        PyValue::Float(f) if f.is_infinite() => json!({"$float": if *f > 0.0 { "inf" } else { "-inf" }}),
        PyValue::Float(f) => json!(f),
        PyValue::Str(s) => json!(s),
        PyValue::Bytes(b) => json!({"$bytes": b64e(b)}),
        PyValue::Date { .. } => json!({"$date": v.isoformat()}),
        PyValue::DateTime(_) => panic!("datetime"),
        PyValue::List(l) => Value::Array(l.iter().map(enc).collect()),
        PyValue::Set(l) => json!({"$set": l.iter().map(enc).collect::<Vec<_>>()}),
        PyValue::Dict(d) => {
            if d.iter().all(|(k, _)| k.as_str().is_some_and(|s| !s.starts_with('$'))) {
                Value::Object(
                    d.iter()
                        .map(|(k, x)| (k.as_str().unwrap().to_string(), enc(x)))
                        .collect(),
                )
            } else {
                json!({"$dict": d.iter().map(|(k, x)| json!([enc(k), enc(x)])).collect::<Vec<_>>()})
            }
        }
    }
}

/// The generator's `blob(bytes)`: text when UTF-8, else `{"$bytes": ...}`.
pub fn blob(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({"$bytes": b64e(b)}),
    }
}

pub fn unblob(v: &Value) -> Vec<u8> {
    match v {
        Value::String(s) => s.as_bytes().to_vec(),
        other => b64d(other["$bytes"].as_str().unwrap()),
    }
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

pub fn cfg_from(v: &Value) -> ClientConfig {
    ClientConfig {
        id: v["id"].as_str().unwrap().into(),
        adapter: v["adapter"].as_str().unwrap().into(),
        display_name: v["display_name"].as_str().unwrap().into(),
        description: v["description"].as_str().unwrap().into(),
        roots: v["roots"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, x)| (k.clone(), x.as_str().unwrap().to_string()))
            .collect(),
        params: dec(&v["params"]),
        spec: if v["spec"].is_null() {
            None
        } else {
            Some(dec(&v["spec"]))
        },
        strict: v["strict"].as_bool().unwrap(),
        manage: v["manage"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, x)| (k.clone(), x.as_bool().unwrap()))
            .collect(),
        icon: v["icon"].as_str().map(str::to_string),
        color: v["color"].as_str().map(str::to_string),
    }
}

pub fn skill_from(v: &Value) -> SkillItem {
    SkillItem {
        name: v["name"].as_str().unwrap().into(),
        description: v["description"].as_str().unwrap().into(),
        files: v["files"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, x)| (k.clone(), unblob(x)))
            .collect(),
        groups: strs(&v["groups"]),
        tags: strs(&v["tags"]),
        source: v["source"].as_str().unwrap().into(),
    }
}

pub fn ctx_from(v: &Value) -> RenderContext {
    RenderContext {
        client: cfg_from(&v["client"]),
        skills: v["skills"].as_array().unwrap().iter().map(skill_from).collect(),
        agents: serde_json::from_value(v["agents"].clone()).unwrap(),
        instructions: serde_json::from_value(v["instructions"].clone()).unwrap(),
        mcp_servers: serde_json::from_value(v["mcp_servers"].clone()).unwrap(),
        memory: serde_json::from_value(v["memory"].clone()).unwrap(),
        permissions: serde_json::from_value(v["permissions"].clone()).unwrap(),
        endpoints: v["endpoints"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, x)| (k.clone(), x.as_str().unwrap().to_string()))
            .collect(),
    }
}

pub fn enc_skill(s: &SkillItem) -> Value {
    json!({"name": s.name, "description": s.description,
           "files": s.files.iter().map(|(k, v)| (k.clone(), blob(v))).collect::<Map<String, Value>>(),
           "groups": s.groups, "tags": s.tags, "source": s.source})
}

/// The generator's `enc_discovered(d)`.
pub fn enc_discovered(d: &DiscoveredContent) -> Value {
    json!({
        "skills": d.skills.iter().map(enc_skill).collect::<Vec<_>>(),
        "agents": serde_json::to_value(&d.agents).unwrap(),
        "instructions": serde_json::to_value(&d.instructions).unwrap(),
        "mcp_servers": serde_json::to_value(&d.mcp_servers).unwrap(),
        "permissions": d.permissions.as_ref().map(|p| serde_json::to_value(p).unwrap()),
        "notes": d.notes,
        "suggested_params": enc(&PyValue::Dict(d.suggested_params.iter().map(|(k, v)| (PyValue::str(k.clone()), v.clone())).collect())),
    })
}

/// One rendered artifact in the generator's shape (without its "yaml" flag).
pub fn enc_artifact(a: &Artifact) -> Value {
    json!({"root": a.root, "path": a.path, "content": blob(&a.content), "mode": a.mode, "kind": a.kind,
           "source_ids": a.source_ids, "merge": a.merge, "managed_keys": a.managed_keys})
}

pub fn enc_diags(res: &RenderResult) -> Value {
    serde_json::to_value(&res.diagnostics).unwrap()
}

// ---- golden case runners -----------------------------------------------------------------------------------------------
pub fn adapter_for(id: &str) -> std::sync::Arc<dyn agent_hub::adapters::base::ClientAdapter> {
    agent_hub::adapters::load_registry()
        .get(id)
        .unwrap_or_else(|| panic!("adapter {id} not registered"))
}

fn first_diff(a: &Value, b: &Value) -> String {
    let (sa, sb) = (
        serde_json::to_string_pretty(a).unwrap(),
        serde_json::to_string_pretty(b).unwrap(),
    );
    for (i, (x, y)) in sa.lines().zip(sb.lines()).enumerate() {
        if x != y {
            return format!("line {i}: rust {x:?} != python {y:?}");
        }
    }
    format!(
        "lengths differ: rust {} lines, python {} lines",
        sa.lines().count(),
        sb.lines().count()
    )
}

/// Every golden render case whose name starts with `prefix`: artifacts (bytes, mode, kind, merge, keys, ids) and
/// diagnostics in order, or the exception. Returns the names of the cases that differ, with the first difference.
pub fn render_golden(prefix: &str) -> (usize, Vec<String>) {
    render_golden_in(&GOLDEN, prefix)
}

/// [`render_golden`] over another golden document.
pub fn render_golden_in(golden: &Value, prefix: &str) -> (usize, Vec<String>) {
    let mut n = 0;
    let mut bad = Vec::new();
    for case in golden["render"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        if !name.starts_with(prefix) {
            continue;
        }
        n += 1;
        let adapter = adapter_for(case["adapter"].as_str().unwrap());
        let ctx = ctx_from(&case["ctx"]);
        let want = &case["result"];
        let got = match adapter.render(&ctx) {
            Err(e) => json!({"error": format!("{}: {}", e.py_type, hub_core::py::text::take_chars(&e.message, 200))}),
            Ok(res) => json!({"artifacts": res.artifacts.iter().map(enc_artifact).collect::<Vec<_>>(),
                              "diagnostics": enc_diags(&res)}),
        };
        let mut want = want.clone();
        if let Some(arts) = want.get_mut("artifacts").and_then(Value::as_array_mut) {
            for a in arts {
                a.as_object_mut().unwrap().remove("yaml");
            }
        }
        if got != want {
            bad.push(format!("{name}: {}", first_diff(&got, &want)));
        }
    }
    (n, bad)
}

/// Every golden discover case whose name starts with `prefix`.
pub fn discover_golden(prefix: &str) -> (usize, Vec<String>) {
    discover_golden_in(&GOLDEN, prefix)
}

/// [`discover_golden`] over another golden document.
pub fn discover_golden_in(golden: &Value, prefix: &str) -> (usize, Vec<String>) {
    let mut n = 0;
    let mut bad = Vec::new();
    for case in golden["discover"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        if !name.starts_with(prefix) {
            continue;
        }
        n += 1;
        let adapter = adapter_for(case["adapter"].as_str().unwrap());
        let cfg = cfg_from(&case["cfg"]);
        let fs = FakeFs::new(
            case["fs"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), unblob(v))),
        );
        let got = match adapter.discover(&cfg, &fs) {
            Err(e) => json!({"error": format!("{}: {}", e.py_type, hub_core::py::text::take_chars(&e.message, 200))}),
            Ok(d) => enc_discovered(&d),
        };
        if got != case["result"] {
            bad.push(format!("{name}: {}", first_diff(&got, &case["result"])));
        }
    }
    (n, bad)
}

/// id, display_name, docs, caps and default_config of one adapter (GET /adapters serves them).
pub fn meta_golden(id: &str) -> Vec<String> {
    let a = adapter_for(id);
    let want = GOLDEN["meta"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap();
    let got = json!({"id": a.id(), "module": want["module"], "display_name": a.display_name(), "docs": a.docs(),
                     "caps": serde_json::to_value(a.caps()).unwrap(),
                     "default_config": serde_json::to_value(a.default_config()).unwrap(), "registry_id": id});
    if &got == want {
        vec![]
    } else {
        vec![first_diff(&got, want)]
    }
}

pub fn assert_none(what: &str, n: usize, bad: &[String]) {
    assert!(n > 0, "{what}: no golden cases");
    assert!(
        bad.is_empty(),
        "{what}: {} of {n} golden cases differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
