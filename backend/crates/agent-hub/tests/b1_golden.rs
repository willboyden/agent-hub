//! WS-B1 golden scenarios: every step of tests/golden/b1_content.json (recorded from the PYTHON ContentStore, Organizer,
//! Inbox and suggest by backend/parity/gen/gen_content_golden.py) is replayed on the Rust services; results, errors
//! (exception class, status, code, detail, extra) and the content tree must match. Normalisation is the generator's:
//! mtimes, inbox `created`, saved random ids.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_hub::adapters::AdapterRegistry;
use agent_hub::adapters::base::{
    AdapterCaps, AdapterResult, AgentItem, Check, ClientAdapter, ClientConfig, CommandRunner, DiscoveredContent,
    Expected, FileSystem, Files, InstructionItem, McpItem, PermissionSet, RenderContext, RenderResult, Rule, SkillItem,
};
use agent_hub::services::attest::Attestor;
use agent_hub::services::content::{ContentStore, Exc, Item, XResult, py_class, sidecar_json, validate_endpoints};
use agent_hub::services::floor::{Floor, load_floor};
use agent_hub::services::importer::{ImportGate, Importer, ObservedSource};
use agent_hub::services::inbox::Inbox;
use agent_hub::services::organize::Organizer;
use agent_hub::services::resolver::Resolver;
use agent_hub::services::suggest::suggest;
use base64::Engine as _;
use hub_core::models::Member;
use hub_core::value::PyValue;
use hub_core::{pyd, yaml, yamlsafe};
use serde_json::{Map, Value, json};

const GOLDEN: &str = include_str!("golden/b1_content.json");

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn unb64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s).unwrap_or_default()
}

fn item_out(it: &Item) -> Value {
    json!({
        "kind": it.kind, "name": it.name, "valid": it.valid(), "meta": it.meta_json(), "body": it.body,
        "files": it.files.iter().map(|(k, v)| (k.clone(), Value::String(b64(v)))).collect::<Map<_, _>>(),
        "file_order": it.files.keys().collect::<Vec<_>>(),
        "sidecar": it.sidecar.as_ref().map(sidecar_json),
        "issues": it.issues.iter().map(|i| serde_json::to_value(i).unwrap_or(Value::Null)).collect::<Vec<_>>(),
        "title": it.title(), "description": it.description(), "tags": it.tags(), "groups": it.groups(),
        "source": it.source(), "applies_to": it.applies_to(),
        "updated": if it.updated == 0.0 { json!(0.0) } else { json!("<mtime>") },
    })
}

fn tv<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// A client whose discover() returns what the scenario set (no filesystem).
#[derive(Default)]
struct Canned {
    dc: Mutex<DiscoveredContent>,
}

impl ClientAdapter for Canned {
    fn id(&self) -> &str {
        "canned"
    }
    fn display_name(&self) -> &str {
        "Canned"
    }
    fn docs(&self) -> &str {
        ""
    }
    fn caps(&self) -> AdapterCaps {
        AdapterCaps {
            skills: true,
            agents: true,
            instructions: true,
            mcp: true,
            permissions: true,
            ..AdapterCaps::default()
        }
    }
    fn default_config(&self) -> ClientConfig {
        ClientConfig {
            id: "canned".into(),
            adapter: "canned".into(),
            display_name: "Canned".into(),
            ..ClientConfig::default()
        }
    }
    fn render(&self, _ctx: &RenderContext) -> AdapterResult<RenderResult> {
        Ok(RenderResult::default())
    }
    fn discover(&self, _cfg: &ClientConfig, _fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent> {
        Ok(self.dc.lock().map(|d| d.clone()).unwrap_or_default())
    }
    fn verify(
        &self,
        _: &ClientConfig,
        _: &[Expected],
        _: &dyn FileSystem,
        _: &dyn CommandRunner,
    ) -> AdapterResult<Vec<Check>> {
        Ok(Vec::new())
    }
}

/// `DiscoveredContent.model_validate(d)` of the scenario's JSON (pydantic defaults filled in).
fn discovered(d: &Value) -> DiscoveredContent {
    let s = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    let os = |v: &Value, k: &str| v[k].as_str().map(str::to_string);
    let l = |v: &Value, k: &str| -> Vec<String> { serde_json::from_value(v[k].clone()).unwrap_or_default() };
    let arr = |k: &str| d[k].as_array().cloned().unwrap_or_default();
    DiscoveredContent {
        skills: arr("skills")
            .iter()
            .map(|x| SkillItem {
                name: s(x, "name"),
                description: s(x, "description"),
                files: Run::files(&x["files"]),
                ..SkillItem::default()
            })
            .collect(),
        agents: arr("agents")
            .iter()
            .map(|x| AgentItem {
                name: s(x, "name"),
                description: s(x, "description"),
                capabilities: l(x, "capabilities"),
                model_tier: os(x, "model_tier"),
                mode: os(x, "mode").unwrap_or_else(|| "subagent".into()),
                read_only: x["read_only"].as_bool().unwrap_or(false),
                body: s(x, "body"),
            })
            .collect(),
        instructions: arr("instructions")
            .iter()
            .map(|x| InstructionItem {
                id: s(x, "id"),
                title: s(x, "title"),
                order: x["order"].as_i64().unwrap_or(0),
                body: s(x, "body"),
            })
            .collect(),
        mcp_servers: arr("mcp_servers")
            .iter()
            .map(|x| McpItem {
                name: s(x, "name"),
                transport: s(x, "transport"),
                command: os(x, "command"),
                args: l(x, "args"),
                url: os(x, "url"),
                env_names: l(x, "env_names"),
                sandbox_profile: os(x, "sandbox_profile"),
                egress_hosts: l(x, "egress_hosts"),
                scan_status: os(x, "scan_status").unwrap_or_else(|| "unscanned".into()),
                pinned_ref: os(x, "pinned_ref"),
            })
            .collect(),
        permissions: d.get("permissions").filter(|p| !p.is_null()).map(|p| PermissionSet {
            rules: p["rules"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| Rule {
                    kind: s(r, "kind"),
                    matches: s(r, "match"),
                    decision: s(r, "decision"),
                    reason: os(r, "reason"),
                })
                .collect(),
            ..PermissionSet::default()
        }),
        notes: l(d, "notes"),
        suggested_params: d["suggested_params"]
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), PyValue::from_json(v))).collect())
            .unwrap_or_default(),
    }
}

/// The stored observation of a client, as the scenario set it.
#[derive(Default)]
struct FakeObserve {
    idx: Option<Value>,
    entries: Map<String, Value>,
    blobs: BTreeMap<String, Files>,
}

#[derive(Default)]
struct Observe(Mutex<FakeObserve>);

impl ObservedSource for Observe {
    fn index(&self, _client: &str) -> Option<Value> {
        self.0.lock().ok().and_then(|o| o.idx.clone())
    }
    fn entry(&self, _client: &str, key: &str) -> Option<Value> {
        self.0.lock().ok().and_then(|o| o.entries.get(key).cloned())
    }
    fn files(&self, _client: &str, key: &str) -> hub_core::HubResult<Files> {
        Ok(self
            .0
            .lock()
            .ok()
            .and_then(|o| o.blobs.get(key).cloned())
            .unwrap_or_default())
    }
}

struct Run {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    store: Arc<ContentStore>,
    org: Arc<Organizer>,
    inbox: Inbox,
    canned: Arc<Canned>,
    importer: Importer,
    observe: Observe,
    audits: Arc<Mutex<Vec<Value>>>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

impl Run {
    fn new(key: &str, max_file: u64, max_skill: u64) -> Result<Run, Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let base = tmp.path().to_path_buf();
        let root = base.join("content");
        std::fs::create_dir(&root)?;
        let kf = base.join("attest.key");
        std::fs::write(&kf, key)?;
        let mut perms = std::fs::metadata(&kf)?.permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o600);
        std::fs::set_permissions(&kf, perms)?;
        let store = Arc::new(ContentStore::new(
            &root,
            max_file,
            max_skill,
            Some(Arc::new(Attestor::new(&kf))),
        ));
        let floor = Arc::new(Floor::new(
            load_floor(&repo_root().join("policy/floor.yaml")).map_err(|p| p.detail)?,
        ));
        let resolver = Arc::new(Resolver::new(floor.clone(), AdapterRegistry::new(), None, None));
        let org = Arc::new(Organizer::new(store.clone(), resolver, AdapterRegistry::new()));
        let inbox = Inbox::new(store.clone(), 100, 200);
        let canned = Arc::new(Canned::default());
        let mut reg = AdapterRegistry::new();
        reg.register(canned.clone())?;
        let importer = Importer::new(store.clone(), reg, floor, org.clone(), &[]);
        Ok(Run {
            _tmp: tmp,
            base,
            root,
            store,
            org,
            inbox,
            canned,
            importer,
            observe: Observe::default(),
            audits: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn files(a: &Value) -> agent_hub::adapters::base::Files {
        a.as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), unb64(v.as_str().unwrap_or_default())))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn side(a: &Value) -> XResult<PyValue> {
        Ok(pyd::validate(hub_core::models::sidecar(), &PyValue::from_json(a))?)
    }

    fn gate(&self, g: &Value) -> Option<ImportGate> {
        if g.is_null() {
            return None;
        }
        let res = g.get("result").cloned().unwrap_or_else(|| json!({"status": "missing"}));
        let audits = self.audits.clone();
        Some(ImportGate {
            mode: g["mode"].as_str().unwrap_or_default().to_string(),
            check: Arc::new(move |_h: &str| res.clone()),
            override_reason: g["override_reason"].as_str().map(str::to_string),
            audit: Some(Arc::new(move |name: &str, info: &Value| {
                if let Ok(mut a) = audits.lock() {
                    a.push(json!({"name": name, "info": info}));
                }
            })),
        })
    }

    fn members(a: &Value) -> Vec<Member> {
        serde_json::from_value(a.clone()).unwrap_or_default()
    }

    #[allow(clippy::too_many_lines)]
    fn op(&self, op: &str, a: &Value) -> XResult<Value> {
        let s = |k: &str| a[k].as_str().unwrap_or_default().to_string();
        let pv = |k: &str| PyValue::from_json(&a[k]);
        let st = &self.store;
        Ok(match op {
            "seed" => json!(st.seed_x()?),
            "put" => item_out(&st.put_x(&s("kind"), &s("name"), &pv("payload"), a["must_exist"].as_bool())?),
            "load" => item_out(&st.load_x(&s("kind"), &s("name"))?),
            "delete" => {
                st.delete_x(&s("kind"), &s("name"))?;
                Value::Null
            }
            "dup" => item_out(&st.duplicate_skill_x(&s("name"), &s("new_name"))?),
            "names" => json!(st.names_x(&s("kind"))?),
            "names_of" => json!(st.names_of_x(&s("sub"))?),
            "list_collections" => json!(st.list_collections_x()?.iter().map(tv).collect::<Vec<_>>()),
            "get_collection" => tv(&st.get_collection_x(&s("cid"))?),
            "put_collection" => tv(&st.put_collection_x(&s("cid"), &pv("payload"))?),
            "delete_collection" => {
                st.delete_collection_x(&s("cid"))?;
                Value::Null
            }
            "get_client" => st.get_client_x(&s("cid"))?.dump(true),
            "put_client" => st.put_client_x(&s("cid"), &pv("payload"))?.dump(true),
            "delete_client" => {
                st.delete_client_x(&s("cid"))?;
                Value::Null
            }
            "get_profile" => tv(&st.get_profile_x(&s("cid"))?),
            "put_profile" => tv(&st.put_profile_x(&s("cid"), &pv("payload"))?),
            "endpoints" => tv(&st.endpoints_x()?),
            "put_endpoints" => tv(&st.put_endpoints(&pv("data")).map_err(Exc::Problem)?),
            "catalog" => {
                let cat = st.catalog_x()?;
                json!({
                    "items": cat.items.iter().map(|((k, n), i)| json!([k, n, item_out(i)])).collect::<Vec<_>>(),
                    "collections": cat.collections.iter().map(|(k, v)| (k.clone(), tv(v))).collect::<Map<_, _>>(),
                    "clients": cat.clients.iter().map(|(k, v)| (k.clone(), v.dump(true))).collect::<Map<_, _>>(),
                    "profiles": cat.profiles.iter().map(|(k, v)| (k.clone(), tv(v))).collect::<Map<_, _>>(),
                    "endpoints": tv(&cat.endpoints),
                    "problems": cat.problems.iter().map(tv).collect::<Vec<_>>(),
                })
            }
            "validate_skill_files" => {
                st.validate_skill_files_x(&Self::files(&a["files"]))?;
                Value::Null
            }
            "write_skill_files" => {
                st.write_skill_files_x(&s("name"), &Self::files(&a["files"]), &Self::side(&a["sidecar"])?)?;
                Value::Null
            }
            "swap_skill_files" => {
                st.swap_skill_files_x(&s("name"), &Self::files(&a["files"]), &Self::side(&a["sidecar"])?)?;
                Value::Null
            }
            "set_sidecar" => {
                st.set_sidecar_x(&s("name"), &Self::side(&a["sidecar"])?)?;
                Value::Null
            }
            "attest_mcp" => item_out(&st.attest_mcp_x(&s("name"))?),
            "raw_write" => {
                let p = self.root.join(s("rel"));
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let data = match a.get("b64") {
                    Some(v) => unb64(v.as_str().unwrap_or_default()),
                    None => s("text").into_bytes(),
                };
                std::fs::write(p, data)?;
                Value::Null
            }
            "raw_symlink" => {
                std::os::unix::fs::symlink(self.base.join(s("target")), self.root.join(s("rel")))?;
                Value::Null
            }
            "raw_outside" => {
                std::fs::write(self.base.join(s("rel")), s("text"))?;
                Value::Null
            }
            "raw_mkdir" => {
                std::fs::create_dir_all(self.root.join(s("rel")))?;
                Value::Null
            }
            "create_collection" => tv(&self.org.create_collection_x(&pv("payload"))?),
            "update_collection" => tv(&self.org.update_collection_x(&s("cid"), &pv("payload"))?),
            "add_members" => tv(&self.org.add_members_x(&s("cid"), &Self::members(&a["members"]))?),
            "remove_member" => tv(&self.org.remove_member_x(&s("cid"), &s("kind"), &s("name"))?),
            "reorder" => {
                let ids: Vec<String> = serde_json::from_value(a["ids"].clone()).unwrap_or_default();
                json!(self.org.reorder_x(&ids)?.iter().map(tv).collect::<Vec<_>>())
            }
            "accept_suggestions" => {
                let ids: Vec<String> = serde_json::from_value(a["ids"].clone()).unwrap_or_default();
                let ov = a
                    .get("overrides")
                    .and_then(Value::as_object)
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), PyValue::from_json(v))).collect())
                    .unwrap_or_default();
                json!(self.org.accept_suggestions_x(&ids, &ov)?)
            }
            "bulk" => {
                let l = |k: &str| -> Vec<String> { serde_json::from_value(a[k].clone()).unwrap_or_default() };
                json!(
                    self.org
                        .bulk(
                            &Self::members(&a["items"]),
                            a["add_to"].as_str(),
                            a["remove_from"].as_str(),
                            &l("add_tags"),
                            &l("remove_tags"),
                        )
                        .map_err(Exc::Problem)?
                )
            }
            "enable_for_client" => {
                let names: Vec<String> = serde_json::from_value(a["names"].clone()).unwrap_or_default();
                self.org.enable_for_client_x(&s("client"), &s("kind"), &names)?;
                Value::Null
            }
            "suggest" => json!(suggest(&st.catalog_x()?).map_err(Exc::Problem)?),
            "validate_endpoints" => match validate_endpoints(&pv("data")) {
                Ok(m) => tv(&m),
                Err(e) => json!({"value_error": e}),
            },
            "inbox_add" => self.inbox.add_x(&s("client"), &s("title"), &s("body"), &s("mtype"))?,
            "inbox_list" => json!(self.inbox.list_x(a["client"].as_str())?),
            "inbox_promote" => {
                let edits = a.get("edits").filter(|e| !e.is_null()).map(PyValue::from_json);
                self.inbox.promote_x(&s("iid"), edits.as_ref())?
            }
            "inbox_reject" => self.inbox.reject_x(&s("iid"))?,
            "set_discovered" => {
                if let Ok(mut d) = self.canned.dc.lock() {
                    *d = discovered(&a["dc"]);
                }
                Value::Null
            }
            "discover" => self.importer.discover_x(&s("client"))?,
            "import" => {
                let sel: Option<Vec<(String, String)>> = a
                    .get("selection")
                    .filter(|v| !v.is_null())
                    .map(|v| serde_json::from_value(v.clone()).unwrap_or_default());
                let ov: indexmap::IndexMap<(String, String), String> = a["overrides"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|t| {
                        let x = |i: usize| t[i].as_str().unwrap_or_default().to_string();
                        ((x(0), x(1)), x(2))
                    })
                    .collect();
                self.importer.do_import_x(
                    &s("client"),
                    sel.as_deref(),
                    a["on_conflict"].as_str(),
                    a["enable"].as_bool().unwrap_or(true),
                    &ov,
                    self.gate(&a["gate"]).as_ref(),
                )?
            }
            "set_observation" => {
                if let Ok(mut o) = self.observe.0.lock() {
                    o.idx = Some(a["index"].clone());
                    o.entries = a["entries"].as_object().cloned().unwrap_or_default();
                    o.blobs = a["files"]
                        .as_object()
                        .map(|m| m.iter().map(|(k, f)| (k.clone(), Self::files(f))).collect())
                        .unwrap_or_default();
                }
                Value::Null
            }
            "import_observed" => {
                let items = a["items"].as_array().cloned().unwrap_or_default();
                self.importer.import_observed_from(
                    &s("client"),
                    &items,
                    &s("on_conflict"),
                    a["enable"].as_bool().unwrap_or(false),
                    self.gate(&a["gate"]).as_ref(),
                    &self.observe,
                )?
            }
            "audits" => json!(self.audits.lock().map(|a| a.clone()).unwrap_or_default()),
            "inbox_set" => {
                self.inbox.set_rate(a["rate"].as_i64().unwrap_or(0));
                self.inbox.set_max_per_client(a["max"].as_i64().unwrap_or(0));
                Value::Null
            }
            other => return Err(Exc::other("UnknownOp", other)),
        })
    }
}

fn err_out(e: &Exc) -> Value {
    match e {
        Exc::Problem(p) => json!({"error": py_class(p), "status": p.status, "code": p.code, "detail": p.detail,
                                  "extra": Value::Object((*p.extra).clone())}),
        other => json!({"error": other.type_name()}),
    }
}

fn fill(v: &Value, vars: &BTreeMap<String, String>) -> Value {
    match v {
        Value::String(s) => {
            let mut out = s.clone();
            for (k, val) in vars {
                out = out.replace(&format!("${{{k}}}"), val);
            }
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| fill(x, vars)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), fill(x, vars))).collect()),
        other => other.clone(),
    }
}

fn norm_str(s: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = s.to_string();
    for (k, val) in vars {
        out = out.replace(val.as_str(), &format!("${{{k}}}"));
    }
    out
}

fn norm(v: &Value, vars: &BTreeMap<String, String>) -> Value {
    match v {
        Value::String(s) => Value::String(norm_str(s, vars)),
        Value::Array(a) => Value::Array(a.iter().map(|x| norm(x, vars)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| {
                    let x = if (k == "created" && x.is_f64()) || (k == "imported_at" && x.is_i64()) {
                        json!("<time>")
                    } else if k == "content_hash" && x.as_str().is_some_and(|h| h.len() == 64 || h.is_empty()) {
                        // pending WS-C: skilleval::content_hash is a stub ("") until WS-C is merged
                        json!("<hash>")
                    } else {
                        x.clone()
                    };
                    (norm_str(k, vars), norm(&x, vars))
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn tree(root: &Path) -> BTreeMap<String, Value> {
    fn walk(d: &Path, root: &Path, out: &mut BTreeMap<String, Value>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p
                .strip_prefix(root)
                .map(|r| r.to_string_lossy().into_owned())
                .unwrap_or_default();
            let m = std::fs::symlink_metadata(&p);
            if m.as_ref().is_ok_and(|m| m.file_type().is_symlink()) {
                let t = std::fs::read_link(&p).unwrap_or_default();
                out.insert(
                    rel,
                    json!({"link": t.file_name().map(|n| n.to_string_lossy().into_owned())}),
                );
            } else if m.as_ref().is_ok_and(std::fs::Metadata::is_dir) {
                out.insert(rel, json!({"dir": true}));
                walk(&p, root, out);
            } else {
                out.insert(rel, json!({"b64": b64(&std::fs::read(&p).unwrap_or_default())}));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn norm_tree(t: BTreeMap<String, Value>, vars: &BTreeMap<String, String>) -> BTreeMap<String, Value> {
    let created = regex::bytes::Regex::new(r"(?m)^created: .*$").ok();
    let imported = regex::bytes::Regex::new(r"imported_at: \d+").ok();
    t.into_iter()
        .map(|(rel, ent)| {
            let ent = match ent.get("b64").and_then(Value::as_str) {
                Some(b) => {
                    let mut raw = unb64(b);
                    if rel.starts_with("inbox/")
                        && let Some(re) = &created
                    {
                        raw = re.replace_all(&raw, &b"created: <time>"[..]).into_owned();
                    }
                    if let Some(re) = &imported {
                        raw = re.replace_all(&raw, &b"imported_at: <time>"[..]).into_owned();
                    }
                    for (k, val) in vars {
                        raw = replace_bytes(&raw, val.as_bytes(), format!("${{{k}}}").as_bytes());
                    }
                    json!({"b64": b64(&raw)})
                }
                None => ent,
            };
            (norm_str(&rel, vars), ent)
        })
        .collect()
}

fn replace_bytes(hay: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if hay[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

/// The value of a content file for a format-insensitive comparison: YAML documents parsed, frontmatter files split.
fn semantic(rel: &str, raw: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(raw).ok()?;
    if rel.ends_with(".yaml") {
        return yaml::load(text, yaml::Loader::Safe).ok().map(|v| v.to_json());
    }
    if rel.ends_with(".md") {
        let (meta, body) = yamlsafe::split_frontmatter(text).ok()?;
        return Some(json!([PyValue::Dict(meta).to_json(), body]));
    }
    None
}

/// The path and values of the first difference.
fn first_diff(a: &Value, b: &Value, path: &str) -> Option<String> {
    if a == b {
        return None;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys()) {
                let (u, v) = (x.get(k).unwrap_or(&Value::Null), y.get(k).unwrap_or(&Value::Null));
                if let Some(d) = first_diff(u, v, &format!("{path}.{k}")) {
                    return Some(d);
                }
            }
            Some(format!("{path}: key sets differ"))
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x
            .iter()
            .zip(y)
            .enumerate()
            .find_map(|(i, (u, v))| first_diff(u, v, &format!("{path}[{i}]"))),
        _ => Some(format!("{path}: {a} != {b}")),
    }
}

/// YAML parser error texts are WS-Y's (the phase-1 engine words them differently): `invalid YAML: <anything>`.
fn mask_yaml_errors(v: &Value) -> Value {
    match v {
        Value::String(s) => match s.find("invalid YAML: ") {
            Some(i) => Value::String(format!("{}invalid YAML: <masked>", &s[..i])),
            None => v.clone(),
        },
        Value::Array(a) => Value::Array(a.iter().map(mask_yaml_errors).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), mask_yaml_errors(x))).collect()),
        other => other.clone(),
    }
}

struct Outcome {
    yaml_texts: Vec<String>,
    steps: Vec<String>,
    semantic: Vec<String>,
    bytes: Vec<String>,
}

fn replay() -> Result<Outcome, Box<dyn std::error::Error>> {
    replay_where(|needs| needs.is_none())
}

fn replay_where(want: impl Fn(Option<&str>) -> bool) -> Result<Outcome, Box<dyn std::error::Error>> {
    let doc: Value = serde_json::from_str(GOLDEN)?;
    let key = doc["key"].as_str().unwrap_or_default();
    let (mf, ms) = (
        doc["max_file"].as_u64().unwrap_or(0),
        doc["max_skill"].as_u64().unwrap_or(0),
    );
    let mut out = Outcome {
        yaml_texts: Vec::new(),
        steps: Vec::new(),
        semantic: Vec::new(),
        bytes: Vec::new(),
    };
    for sc in doc["scenarios"].as_array().into_iter().flatten() {
        let name = sc["name"].as_str().unwrap_or_default();
        if !want(sc["needs"].as_str()) {
            continue;
        }
        let run = Run::new(key, mf, ms)?;
        let mut vars: BTreeMap<String, String> = BTreeMap::new();
        for (i, st) in sc["steps"].as_array().into_iter().flatten().enumerate() {
            let args = fill(&st["args"], &vars);
            let op = st["op"].as_str().unwrap_or_default();
            let got = match run.op(op, &args) {
                Ok(v) => {
                    if let Some(var) = st["save"].as_str()
                        && let Some(id) = v["id"].as_str()
                    {
                        vars.insert(var.to_string(), id.to_string());
                    }
                    json!({"ok": v})
                }
                Err(e) => err_out(&e),
            };
            let got = norm(&got, &vars);
            if let Some(d) = first_diff(&st["expect"], &got, "") {
                let msg = format!(
                    "{name}#{i} {op} {}\n   {}",
                    hub_core::py::text::take_chars(&args.to_string(), 160),
                    hub_core::py::text::take_chars(&d, 600)
                );
                if mask_yaml_errors(&st["expect"]) == mask_yaml_errors(&got) {
                    out.yaml_texts.push(msg);
                } else {
                    out.steps.push(msg);
                }
            }
        }
        let want: BTreeMap<String, Value> = sc["tree"]
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        let got = norm_tree(tree(&run.root), &vars);
        for k in want.keys().chain(got.keys()).collect::<std::collections::BTreeSet<_>>() {
            let (w, g) = (want.get(k), got.get(k));
            if w == g {
                continue;
            }
            let msg = format!(
                "{name}: {k}\n   want {}\n   got  {}",
                w.map(|v| String::from_utf8_lossy(&unb64(v["b64"].as_str().unwrap_or_default())).into_owned())
                    .unwrap_or_else(|| format!("{w:?}")),
                g.map(|v| String::from_utf8_lossy(&unb64(v["b64"].as_str().unwrap_or_default())).into_owned())
                    .unwrap_or_else(|| format!("{g:?}"))
            );
            out.bytes.push(msg.clone());
            let same_value = match (w.and_then(|v| v["b64"].as_str()), g.and_then(|v| v["b64"].as_str())) {
                (Some(a), Some(b)) => {
                    let (sa, sb) = (semantic(k, &unb64(a)), semantic(k, &unb64(b)));
                    sa.is_some() && sa == sb
                }
                _ => false,
            };
            if !same_value {
                out.semantic.push(msg);
            }
        }
    }
    Ok(out)
}

/// Results, errors and the content tree up to YAML formatting (values of YAML documents and frontmatter).
#[test]
fn b1_golden_results_and_tree_values() -> Result<(), Box<dyn std::error::Error>> {
    let o = replay()?;
    let mut all = o.steps.clone();
    all.extend(o.semantic.iter().cloned());
    assert!(all.is_empty(), "{} differences:\n{}", all.len(), all.join("\n"));
    Ok(())
}

/// Scenarios that also need WS-F's floor (`check_rule` for imported permission rules).
#[test]
fn b1_golden_needs_floor() -> Result<(), Box<dyn std::error::Error>> {
    let o = replay_where(|needs| needs == Some("WS-F"))?;
    let mut all = o.steps.clone();
    all.extend(o.semantic.iter().cloned());
    all.extend(o.bytes.iter().cloned());
    assert!(all.is_empty(), "{} differences:\n{}", all.len(), all.join("\n"));
    Ok(())
}

/// Scenarios where PyYAML lets a Python exception escape (`2024-02-30` is a ValueError, `!!bool maybe` a KeyError):
/// callers that catch only `YamlError` must let them through (-> 500), like Python.
#[test]
fn b1_golden_needs_yaml_engine() -> Result<(), Box<dyn std::error::Error>> {
    let o = replay_where(|needs| needs == Some("WS-Y"))?;
    let mut all = o.steps.clone();
    all.extend(o.yaml_texts.iter().cloned());
    all.extend(o.semantic.iter().cloned());
    all.extend(o.bytes.iter().cloned());
    assert!(all.is_empty(), "{} differences:\n{}", all.len(), all.join("\n"));
    Ok(())
}

/// PyYAML's parser error texts inside results (`invalid YAML: ...`); the other two tests mask them.
#[test]
fn b1_golden_yaml_error_texts() -> Result<(), Box<dyn std::error::Error>> {
    let o = replay()?;
    assert!(
        o.yaml_texts.is_empty(),
        "{} differences:\n{}",
        o.yaml_texts.len(),
        o.yaml_texts.join("\n")
    );
    Ok(())
}

/// The content tree byte for byte (YAML emitter fidelity is WS-Y's; see the values test for everything else).
#[test]
fn b1_golden_tree_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let o = replay()?;
    assert!(
        o.bytes.is_empty(),
        "{} files differ:\n{}",
        o.bytes.len(),
        o.bytes.join("\n")
    );
    Ok(())
}
