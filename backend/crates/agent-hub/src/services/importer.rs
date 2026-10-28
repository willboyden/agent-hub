//! Port of services/importer.py: discover what a client has (adapter.discover through a FileSystem port limited to the
//! configured roots), import it into the working tree (conflict modes skip | rename | replace | link; secret-looking
//! files refused unless overridden), import observed skills, all through the evaluation gate (F8). Imported MCP servers
//! are always `unscanned`: `clean` is an operator attestation bound to a digest (services/attest.rs).
//! Security control S-IMPORT. Owner: WS-B1.

use std::collections::BTreeSet;
use std::sync::Arc;

use hub_core::HubResult;
use hub_core::Problem;
use hub_core::common::{now, scrub, sha256};
use hub_core::ids::{ENV_NAME_RE, HOST_RE, slugify, valid_id};
use hub_core::models::{self, CAPABILITIES};
use hub_core::py::text::{repr, take_chars};
use hub_core::value::PyValue;
use hub_core::{pyd, yamlsafe};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::content::{
    ContentStore, Exc, XResult, client_value, decode_utf8, dget, dict_items, dpop, dset, dump_model, model_of,
    split_frontmatter_x, truthy,
};
use super::floor::Floor;
use super::fs::discovery_fs;
use super::observe::ObserveService;
use super::organize::Organizer;
use super::resolver::client_config;
use super::skilleval::content_hash;
use crate::adapters::AdapterRegistry;
use crate::adapters::base::{DiscoveredContent, Files, Rule};

pub const IMPORT_KINDS: [&str; 5] = ["skill", "agent", "instruction", "mcp", "rule"];
pub const CONFLICT_MODES: [&str; 4] = ["skip", "rename", "replace", "link"];

/// content hash -> `SkillEvalService::gate()` result
pub type GateCheck = Arc<dyn Fn(&str) -> Value + Send + Sync>;
/// (skill name, info) once a skill was written under an override
pub type GateAudit = Arc<dyn Fn(&str, &Value) + Send + Sync>;

/// F8: what the evaluation gate says about a skill about to be written into the content tree.
/// mode off: no gate. advisory: every row carries the gate, nothing is refused. require: `missing` and `blocked` are
/// refused (`eval_required` / `eval_blocked`); a blocked skill passes only with an override reason (gate becomes
/// `overridden`).
#[derive(Clone)]
pub struct ImportGate {
    /// off | advisory | require
    pub mode: String,
    pub check: GateCheck,
    pub override_reason: Option<String>,
    pub audit: Option<GateAudit>,
}

impl ImportGate {
    /// `decide(files)`: (gate info for the result row, refusal code `eval_required`/`eval_blocked`).
    pub fn decide(&self, files: &Files) -> (Option<Value>, Option<String>) {
        if self.mode == "off" {
            return (None, None);
        }
        let g = (self.check)(&content_hash(files));
        let status = g.get("status").cloned().unwrap_or(Value::Null);
        let reasons = match g.get("reasons") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        let mut info = Map::new();
        info.insert("gate".into(), status.clone());
        info.insert("run_id".into(), g.get("run_id").cloned().unwrap_or(Value::Null));
        info.insert("verdict".into(), g.get("verdict").cloned().unwrap_or(Value::Null));
        info.insert("reasons".into(), Value::Array(reasons));
        if self.mode == "require" {
            match status.as_str() {
                Some("missing") => return (Some(Value::Object(info)), Some("eval_required".into())),
                Some("blocked") => {
                    if let Some(reason) = &self.override_reason
                        && !reason.is_empty()
                    {
                        info.insert("gate".into(), Value::String("overridden".into()));
                        info.insert("override_reason".into(), Value::String(reason.clone()));
                        info.insert("gate_status".into(), Value::String("blocked".into()));
                        return (Some(Value::Object(info)), None);
                    }
                    return (Some(Value::Object(info)), Some("eval_blocked".into()));
                }
                _ => {}
            }
        }
        (Some(Value::Object(info)), None)
    }
}

fn refused_text(code: &str) -> &'static str {
    match code {
        "eval_required" => "no evaluation of this exact content exists; evaluate it first (eval_gate is `require`)",
        "eval_blocked" => {
            "the evaluation did not clear this skill (security finding, or a security scan that did not finish); fix it \
             or give an override reason"
        }
        _ => "the text contains secret-looking values; remove them or give an override reason",
    }
}

/// What an import works on: one discovered (or observed) item, normalised for listing and importing.
#[derive(Debug, Clone, Default)]
struct Entry {
    name: String,
    description: String,
    /// skills
    files: Files,
    /// everything else: the document fields (+ `body`)
    payload: Vec<(PyValue, PyValue)>,
    rule: Option<Rule>,
    notes: Vec<String>,
    scan_secrets: bool,
    observed_key: Option<String>,
    observed: Option<Vec<(PyValue, PyValue)>>,
    /// `_eval`: the gate details recorded in the sidecar's provenance
    eval: Option<Vec<(PyValue, PyValue)>>,
}

/// Where `import_observed` reads a client's stored observation (ObserveService; fakes in tests).
pub trait ObservedSource {
    fn index(&self, client: &str) -> Option<Value>;
    fn entry(&self, client: &str, key: &str) -> Option<Value>;
    fn files(&self, client: &str, key: &str) -> HubResult<Files>;
}

impl ObservedSource for ObserveService {
    fn index(&self, client: &str) -> Option<Value> {
        ObserveService::index(self, client)
    }
    fn entry(&self, client: &str, key: &str) -> Option<Value> {
        ObserveService::entry(self, client, key)
    }
    fn files(&self, client: &str, key: &str) -> HubResult<Files> {
        ObserveService::files(self, client, key)
    }
}

fn s(v: &str) -> PyValue {
    PyValue::str(v)
}

fn opt(v: Option<&String>) -> PyValue {
    v.map_or(PyValue::None, |x| PyValue::str(x.as_str()))
}

fn strs(v: &[String]) -> PyValue {
    PyValue::List(v.iter().map(|x| PyValue::str(x.as_str())).collect())
}

/// `int(time.time())`.
fn int_now() -> i64 {
    #[allow(clippy::cast_possible_truncation)]
    let t = now().floor() as i64;
    t
}

/// The text of a JSON value as Python's `str()` sees it.
fn jstr(v: &Value) -> String {
    PyValue::from_json(v).py_str()
}

fn jtruthy(v: Option<&Value>) -> Option<&Value> {
    v.filter(|x| truthy(&PyValue::from_json(x)))
}

pub struct Importer {
    pub work: Arc<ContentStore>,
    pub registry: AdapterRegistry,
    pub floor: Arc<Floor>,
    pub organizer: Arc<Organizer>,
    pub extra_roots: Vec<String>,
}

impl Importer {
    pub fn new(
        work: Arc<ContentStore>,
        registry: AdapterRegistry,
        floor: Arc<Floor>,
        organizer: Arc<Organizer>,
        extra_roots: &[String],
    ) -> Self {
        Self {
            work,
            registry,
            floor,
            organizer,
            extra_roots: extra_roots.to_vec(),
        }
    }

    // -- discover ----------------------------------------------------------------------------------------------
    fn discover_raw(&self, client_id: &str) -> XResult<DiscoveredContent> {
        if !self.work.names_of_x("clients")?.iter().any(|c| c == client_id) {
            return Err(Problem::not_found(format!("no such client: {client_id}")).into());
        }
        let doc = self.work.get_client_x(client_id)?;
        let Some(adapter) = self.registry.get(&doc.adapter) else {
            return Err(
                Problem::unavailable(format!("adapter {} is not installed", repr(&doc.adapter)))
                    .code("adapter_missing")
                    .into(),
            );
        };
        let cfg = client_config(&doc, None).map_err(|e| Problem::bad_request(format!("client roots invalid: {e}")))?;
        let fs = discovery_fs(&cfg.roots, &cfg.params, &self.extra_roots);
        adapter.discover(&cfg, &fs).map_err(|e| {
            Problem::unavailable(format!("adapter discover failed ({})", e.py_type))
                .code("discover_failed")
                .into()
        })
    }

    /// `_entries(client, dc)`: kind -> entries normalised for both listing and importing.
    fn entries(client: &str, dc: &DiscoveredContent) -> IndexMap<&'static str, Vec<Entry>> {
        let mut out: IndexMap<&'static str, Vec<Entry>> = IMPORT_KINDS.iter().map(|k| (*k, Vec::new())).collect();
        let mut push = |k: &'static str, e: Entry| {
            if let Some(v) = out.get_mut(k) {
                v.push(e);
            }
        };
        for sk in &dc.skills {
            push(
                "skill",
                Entry {
                    name: sk.name.clone(),
                    description: sk.description.clone(),
                    files: sk.files.clone(),
                    ..Entry::default()
                },
            );
        }
        for a in &dc.agents {
            let caps: Vec<String> = a
                .capabilities
                .iter()
                .filter(|c| CAPABILITIES.contains(&c.as_str()))
                .cloned()
                .collect();
            let desc = if a.description.is_empty() {
                &a.name
            } else {
                &a.description
            };
            let payload = vec![
                (s("name"), s(&a.name)),
                (s("description"), s(desc)),
                (s("capabilities"), strs(&caps)),
                (s("model_tier"), opt(a.model_tier.as_ref())),
                (s("mode"), s(&a.mode)),
                (s("read_only"), PyValue::Bool(a.read_only)),
                (s("body"), s(&a.body)),
            ];
            push(
                "agent",
                Entry {
                    name: a.name.clone(),
                    description: a.description.clone(),
                    payload,
                    ..Entry::default()
                },
            );
        }
        for i in &dc.instructions {
            let payload = vec![
                (s("id"), s(&i.id)),
                (s("title"), s(&i.title)),
                (s("order"), PyValue::Int(i.order)),
                (s("body"), s(&i.body)),
            ];
            push(
                "instruction",
                Entry {
                    name: i.id.clone(),
                    description: i.title.clone(),
                    payload,
                    ..Entry::default()
                },
            );
        }
        for m in &dc.mcp_servers {
            let env: Vec<String> = m
                .env_names
                .iter()
                .filter(|n| ENV_NAME_RE.is_match(n))
                .cloned()
                .collect();
            let hosts: Vec<String> = m.egress_hosts.iter().filter(|h| HOST_RE.is_match(h)).cloned().collect();
            let payload: Vec<(PyValue, PyValue)> = vec![
                (s("name"), s(&m.name)),
                (s("transport"), s(&m.transport)),
                (s("command"), opt(m.command.as_ref())),
                (s("args"), strs(&m.args)),
                (s("url"), opt(m.url.as_ref())),
                (s("env_names"), strs(&env)),
                (s("sandbox_profile"), opt(m.sandbox_profile.as_ref())),
                (s("egress_hosts"), strs(&hosts)),
                (s("scan_status"), s("unscanned")),
                (s("pinned_ref"), opt(m.pinned_ref.as_ref())),
                (s("tags"), strs(&["imported".to_string()])),
            ]
            .into_iter()
            .filter(|(_, v)| !v.is_none())
            .collect();
            let description = [m.command.as_deref(), m.url.as_deref()]
                .into_iter()
                .flatten()
                .find(|x| !x.is_empty())
                .unwrap_or("")
                .to_string();
            push(
                "mcp",
                Entry {
                    name: m.name.clone(),
                    description,
                    payload,
                    ..Entry::default()
                },
            );
        }
        if let Some(perms) = &dc.permissions {
            for r in &perms.rules {
                let slug = slugify(client);
                let digest = sha256(format!("{}|{}|{}", r.kind, r.matches, r.decision).as_bytes());
                let rid = format!("imp-{}-{}", take_chars(&slug, 20), &digest[..8]);
                let mut payload = vec![
                    (s("id"), s(&rid)),
                    (s("title"), s(take_chars(&format!("{} {}", r.kind, r.matches), 120))),
                    (s("kind"), s(&r.kind)),
                    (s("match"), s(&r.matches)),
                    (s("decision"), s(&r.decision)),
                    (s("applies_to"), strs(&[client.to_string()])),
                ];
                if let Some(reason) = r.reason.as_ref().filter(|x| !x.is_empty()) {
                    payload.push((s("reason"), s(take_chars(reason, 300))));
                }
                push(
                    "rule",
                    Entry {
                        name: rid,
                        description: format!("{} {} {}", r.decision, r.kind, r.matches),
                        payload,
                        rule: Some(r.clone()),
                        ..Entry::default()
                    },
                );
            }
        }
        out
    }

    /// `_conflict(kind, entry)`: None when nothing by that name exists, else {existing_source, equal, differences}.
    fn conflict(&self, kind: &str, e: &Entry) -> XResult<Option<Value>> {
        let slug = if valid_id(&e.name) {
            e.name.clone()
        } else {
            slugify(&e.name)
        };
        if slug.is_empty() || !self.work.names_x(kind)?.contains(&slug) {
            return Ok(None);
        }
        let existing = self.work.load_x(kind, &slug)?;
        let mut diffs: Vec<String> = Vec::new();
        if kind == "skill" {
            let mine = Self::prepare_skill(&slug, &e.files)?;
            let paths: BTreeSet<&String> = mine.keys().chain(existing.files.keys()).collect();
            for path in paths {
                match (mine.get(path), existing.files.get(path)) {
                    (Some(_), None) => diffs.push(format!("{path} (only in this client)")),
                    (None, Some(_)) => diffs.push(format!("{path} (only in the hub)")),
                    (Some(a), Some(b)) if a != b => diffs.push(path.clone()),
                    _ => {}
                }
            }
        } else if let (Some(meta), Some(model)) = (existing.meta.as_ref(), model_of(kind)) {
            let mut p = e.payload.clone();
            let body = dpop(&mut p, "body");
            let cur = dict_items(dump_model(model, meta, true));
            for (k, v) in &p {
                let key = k.py_str();
                if key == "tags" || key == "applies_to" {
                    continue;
                }
                if dget(&cur, &key).unwrap_or(&PyValue::None) != v {
                    diffs.push(key);
                }
            }
            if let Some(body) = body.filter(|b| !b.is_none())
                && matches!(kind, "agent" | "instruction")
                && PyValue::Str(existing.body.clone()) != body
            {
                diffs.push("body".into());
            }
        } else {
            diffs.push("existing item is invalid".into());
        }
        Ok(Some(
            json!({"existing_source": existing.source(), "equal": diffs.is_empty(),
                       "differences": diffs}),
        ))
    }

    /// POST /clients/{id}/discover
    pub fn discover(&self, client_id: &str) -> HubResult<Value> {
        self.discover_x(client_id).map_err(Exc::into_problem)
    }

    pub fn discover_x(&self, client_id: &str) -> XResult<Value> {
        let dc = self.discover_raw(client_id)?;
        let entries = Self::entries(client_id, &dc);
        let mut items = Map::new();
        for (kind, lst) in &entries {
            let mut rows = Vec::new();
            for e in lst {
                let mut row = Map::new();
                row.insert("kind".into(), Value::String((*kind).into()));
                row.insert("name".into(), Value::String(e.name.clone()));
                row.insert("description".into(), Value::String(e.description.clone()));
                row.insert("conflict".into(), self.conflict(kind, e)?.unwrap_or(Value::Null));
                if *kind == "skill" {
                    let mut files: Vec<&String> = e.files.keys().collect();
                    files.sort();
                    row.insert("files".into(), json!(files));
                }
                if *kind == "rule"
                    && let Some(r) = &e.rule
                    && let Some(f) = self.floor.check_rule(r, "permissions", Some(&e.name))?
                    && f.severity == "error"
                {
                    row.insert("floor_violation".into(), Value::String(f.message));
                }
                rows.push(Value::Object(row));
            }
            items.insert((*kind).into(), Value::Array(rows));
        }
        let suggested: Map<String, Value> = dc
            .suggested_params
            .iter()
            .map(|(k, v)| (k.clone(), v.to_json()))
            .collect();
        Ok(json!({"client": client_id, "items": items, "notes": dc.notes, "suggested_params": suggested}))
    }

    // -- import ------------------------------------------------------------------------------------------------
    /// `do_import(client, selection, on_conflict, enable, overrides, gate)`. on_conflict: skip | rename | replace |
    /// link, globally and/or per item via `overrides`. Unspecified: an EQUAL existing item is linked (enabled for this
    /// client), a DIFFERING one is skipped with the differences reported.
    pub fn do_import(
        &self,
        client_id: &str,
        selection: Option<&[(String, String)]>,
        on_conflict: Option<&str>,
        enable: bool,
        overrides: &IndexMap<(String, String), String>,
        gate: &ImportGate,
    ) -> HubResult<Value> {
        self.do_import_x(client_id, selection, on_conflict, enable, overrides, Some(gate))
            .map_err(Exc::into_problem)
    }

    pub fn do_import_x(
        &self,
        client_id: &str,
        selection: Option<&[(String, String)]>,
        on_conflict: Option<&str>,
        enable: bool,
        overrides: &IndexMap<(String, String), String>,
        gate: Option<&ImportGate>,
    ) -> XResult<Value> {
        for v in on_conflict.into_iter().chain(overrides.values().map(String::as_str)) {
            if !CONFLICT_MODES.contains(&v) {
                return Err(Problem::bad_request("on_conflict must be skip, rename, replace or link").into());
            }
        }
        let dc = self.discover_raw(client_id)?;
        let entries = Self::entries(client_id, &dc);
        let wanted: Option<BTreeSet<(String, String)>> = selection.map(|s| s.iter().cloned().collect());
        let mut results: Vec<Map<String, Value>> = Vec::new();
        let mut enabled: IndexMap<String, Vec<String>> = IndexMap::new();
        let mut linked: IndexMap<String, Vec<String>> = IndexMap::new();
        for kind in IMPORT_KINDS {
            for e in entries.get(kind).cloned().unwrap_or_default() {
                let mut e = e;
                if let Some(w) = &wanted
                    && !w.contains(&(kind.to_string(), e.name.clone()))
                {
                    continue;
                }
                e.notes = dc.notes.iter().filter(|n| n.contains(&e.name)).cloned().collect();
                let mut res = Map::new();
                res.insert("kind".into(), json!(kind));
                res.insert("name".into(), json!(e.name));
                res.insert("final_name".into(), json!(e.name));
                res.insert("status".into(), json!("error"));
                res.insert("detail".into(), json!(""));
                let oc = overrides
                    .get(&(kind.to_string(), e.name.clone()))
                    .map(String::as_str)
                    .or(on_conflict);
                if let Err(x) = self.import_one(client_id, kind, &mut e, oc, &mut res, gate) {
                    res.insert("status".into(), json!("error"));
                    match x {
                        Exc::Problem(p) => {
                            res.insert("code".into(), json!(p.code));
                            res.insert("detail".into(), json!(p.detail));
                        }
                        other => {
                            res.insert("detail".into(), json!(other.type_name()));
                        }
                    }
                }
                let status = res["status"].as_str().unwrap_or_default().to_string();
                let final_name = jstr(&res["final_name"]);
                if matches!(status.as_str(), "created" | "replaced" | "renamed") {
                    enabled.entry(kind.to_string()).or_default().push(final_name);
                } else if status == "linked" {
                    linked.entry(kind.to_string()).or_default().push(final_name);
                }
                results.push(res);
            }
        }
        if let Some(w) = &wanted {
            let found: BTreeSet<(String, String)> =
                results.iter().map(|r| (jstr(&r["kind"]), jstr(&r["name"]))).collect();
            for (kind, name) in w.difference(&found) {
                let mut r = Map::new();
                r.insert("kind".into(), json!(kind));
                r.insert("name".into(), json!(name));
                r.insert("final_name".into(), json!(name));
                r.insert("status".into(), json!("error"));
                r.insert("detail".into(), json!("not found in the client's discovered content"));
                results.push(r);
            }
        }
        let suggested = PyValue::Dict(
            dc.suggested_params
                .iter()
                .map(|(k, v)| (PyValue::str(k.as_str()), v.clone()))
                .collect(),
        );
        self.merge_params_x(client_id, &suggested)?;
        let kinds: BTreeSet<&String> = enabled.keys().chain(linked.keys()).collect();
        for kind in kinds {
            let mut names: Vec<String> = if enable {
                enabled.get(kind).cloned().unwrap_or_default()
            } else {
                Vec::new()
            };
            names.extend(linked.get(kind).cloned().unwrap_or_default());
            if !names.is_empty() {
                self.organizer.enable_for_client_x(client_id, kind, &names)?;
            }
        }
        Ok(json!({"client": client_id, "results": results}))
    }

    /// `_merge_params` exposure for the hub when needed.
    pub fn merge_params(&self, client_id: &str, suggested: &PyValue) -> HubResult<()> {
        self.merge_params_x(client_id, suggested).map_err(Exc::into_problem)
    }

    /// Adapter-suggested client params (e.g. instructions_mode) fill in keys the operator has not set.
    fn merge_params_x(&self, client_id: &str, suggested: &PyValue) -> XResult<()> {
        if !truthy(suggested) {
            return Ok(());
        }
        let doc = self.work.get_client_x(client_id)?;
        let mut merged = dict_items(suggested.clone());
        for (k, v) in doc.params.as_dict().unwrap_or_default() {
            match merged.iter_mut().find(|(mk, _)| mk == k) {
                Some(slot) => slot.1 = v.clone(),
                None => merged.push((k.clone(), v.clone())),
            }
        }
        let merged = PyValue::Dict(merged);
        if merged != doc.params {
            let mut payload = dict_items(client_value(&doc, true));
            dset(&mut payload, "params", merged);
            self.work.put_client_x(client_id, &PyValue::Dict(payload))?;
        }
        Ok(())
    }

    /// `import_observed(client, items, on_conflict, enable, gate, observe)`. F8: create content skills from a client's
    /// stored observation. Never enabled unless asked: enabling an observed skill for the SAME client would deliver a
    /// second copy next to the original.
    pub fn import_observed(
        &self,
        client_id: &str,
        items: &[Value],
        on_conflict: &str,
        enable: bool,
        gate: &ImportGate,
        observe: &ObserveService,
    ) -> HubResult<Value> {
        self.import_observed_from(client_id, items, on_conflict, enable, Some(gate), observe)
            .map_err(Exc::into_problem)
    }

    /// `import_observed` over any [`ObservedSource`].
    pub fn import_observed_from(
        &self,
        client_id: &str,
        items: &[Value],
        on_conflict: &str,
        enable: bool,
        gate: Option<&ImportGate>,
        observe: &dyn ObservedSource,
    ) -> XResult<Value> {
        if !CONFLICT_MODES.contains(&on_conflict) {
            return Err(Problem::bad_request("on_conflict must be skip, rename, replace or link").into());
        }
        if !self.work.names_of_x("clients")?.iter().any(|c| c == client_id) {
            return Err(Problem::not_found(format!("no such client: {client_id}")).into());
        }
        if observe.index(client_id).is_none() {
            return Err(Problem::not_found("this client has not been observed yet")
                .code("not_observed")
                .into());
        }
        let mut results = Vec::new();
        let mut wanted: Vec<String> = Vec::new();
        for it in items {
            let key = it.get("key").map_or_else(String::new, jstr);
            let mut row = Map::new();
            row.insert("key".into(), json!(key));
            row.insert("final_name".into(), json!(""));
            row.insert("status".into(), json!("error"));
            row.insert("gate".into(), Value::Null);
            row.insert("detail".into(), json!(""));
            let r = (|| -> XResult<()> {
                let Some(ent) = observe.entry(client_id, &key) else {
                    return Err(Problem::not_found("not in the stored observation of this client").into());
                };
                let name = jtruthy(it.get("name"))
                    .or_else(|| jtruthy(ent.get("name")))
                    .map_or_else(|| key.clone(), jstr);
                row.insert("final_name".into(), json!(name));
                let observed_at = observe
                    .index(client_id)
                    .and_then(|i| i.get("observed_at").cloned())
                    .unwrap_or(Value::Null);
                let g = |k: &str| PyValue::from_json(ent.get(k).unwrap_or(&Value::Null));
                let mut e = Entry {
                    scan_secrets: true,
                    observed_key: Some(key.clone()),
                    name: name.clone(),
                    description: ent.get("description").map(jstr).unwrap_or_default(),
                    files: observe.files(client_id, &key)?,
                    observed: Some(vec![
                        (s("client"), s(client_id)),
                        (s("path"), g("path")),
                        (s("origin"), g("origin")),
                        (s("content_hash"), g("content_hash")),
                        (s("observed_at"), PyValue::from_json(&observed_at)),
                    ]),
                    ..Entry::default()
                };
                let mut inner = Map::new();
                inner.insert("final_name".into(), json!(name));
                inner.insert("status".into(), json!("error"));
                inner.insert("detail".into(), json!(""));
                self.import_one(client_id, "skill", &mut e, Some(on_conflict), &mut inner, gate)?;
                for (k, v) in inner {
                    if k != "kind" && k != "name" {
                        row.insert(k, v);
                    }
                }
                if row["status"] == "identical" {
                    row.insert("status".into(), json!("skipped"));
                    row.insert(
                        "detail".into(),
                        json!(format!("identical to the existing skill {}", jstr(&row["final_name"]))),
                    );
                }
                if matches!(row["status"].as_str(), Some("created" | "replaced" | "renamed")) {
                    wanted.push(jstr(&row["final_name"]));
                }
                Ok(())
            })();
            if let Err(x) = r {
                row.insert("status".into(), json!("error"));
                match x {
                    Exc::Problem(p) => {
                        row.insert("code".into(), json!(p.code));
                        row.insert("detail".into(), json!(p.detail));
                    }
                    other => {
                        row.insert("detail".into(), json!(other.type_name()));
                    }
                }
            }
            results.push(Value::Object(row));
        }
        if enable && !wanted.is_empty() {
            self.organizer.enable_for_client_x(client_id, "skill", &wanted)?;
        }
        Ok(json!({"client": client_id, "results": results, "enabled": enable && !wanted.is_empty()}))
    }

    #[allow(clippy::too_many_lines)]
    fn import_one(
        &self,
        client: &str,
        kind: &str,
        e: &mut Entry,
        on_conflict: Option<&str>,
        res: &mut Map<String, Value>,
        gate: Option<&ImportGate>,
    ) -> XResult<()> {
        let name = e.name.clone();
        let (info, mut refusal) = match gate {
            Some(g) if kind == "skill" => g.decide(&e.files),
            _ => (None, None),
        };
        if let Some(info) = &info {
            res.insert("gate".into(), info["gate"].clone());
            let mut ev: Vec<(PyValue, PyValue)> = Vec::new();
            for k in ["run_id", "verdict", "gate", "override_reason"] {
                let v = info.get(k).cloned().unwrap_or(Value::Null);
                if !v.is_null() || matches!(k, "run_id" | "verdict") {
                    ev.push((s(k), PyValue::from_json(&v)));
                }
            }
            e.eval = Some(ev);
        }
        let mut final_name = if valid_id(&name) { name.clone() } else { slugify(&name) };
        if final_name.is_empty() {
            return Err(Problem::bad_request("name cannot be turned into a valid id").into());
        }
        if final_name != name {
            res.insert(
                "detail".into(),
                json!(format!("renamed from {} to satisfy the id rules", repr(&name))),
            );
        }
        if kind == "rule"
            && let Some(r) = &e.rule
            && let Some(f) = self.floor.check_rule(r, "permissions", Some(&name))?
            && f.severity == "error"
        {
            res.insert("status".into(), json!("skipped"));
            res.insert("detail".into(), json!(format!("violates the floor: {}", f.message)));
            return Ok(());
        }
        let conflict = self.conflict(kind, e)?;
        let mut replaced = false;
        if let Some(c) = conflict {
            res.insert("final_name".into(), json!(final_name));
            if c["equal"] == Value::Bool(true) {
                if matches!(on_conflict, None | Some("link")) {
                    res.insert("status".into(), json!("linked"));
                    res.insert(
                        "detail".into(),
                        json!("identical to the existing item: enabled it for this client"),
                    );
                } else {
                    res.insert("status".into(), json!("identical"));
                }
                return Ok(());
            }
            let diffs: Vec<String> = c["differences"]
                .as_array()
                .map(|a| a.iter().map(jstr).collect())
                .unwrap_or_default();
            res.insert("differences".into(), json!(diffs));
            match on_conflict {
                None | Some("skip") => {
                    res.insert("status".into(), json!("skipped"));
                    res.insert("reason".into(), json!("differs"));
                    let shown: Vec<&str> = diffs.iter().take(8).map(String::as_str).collect();
                    res.insert(
                        "detail".into(),
                        json!(format!("exists with different content: {}", shown.join(", "))),
                    );
                    return Ok(());
                }
                Some("link") => {
                    res.insert("status".into(), json!("linked"));
                    res.insert(
                        "detail".into(),
                        json!("enabled the existing canonical item (this client's version was not imported)"),
                    );
                    return Ok(());
                }
                Some("rename") => {
                    let names = self.work.names_x(kind)?;
                    let stem = take_chars(&final_name, 58).to_string();
                    let mut n = 2;
                    while names.contains(&format!("{stem}-{n}")) {
                        n += 1;
                        if n > 99 {
                            return Err(Problem::conflict("could not find a free name").into());
                        }
                    }
                    final_name = format!("{stem}-{n}");
                }
                Some(_) => replaced = true,
            }
        }
        res.insert("final_name".into(), json!(final_name));
        let mut warnings: Vec<String> = Vec::new();
        if kind != "skill"
            && let Some(model) = model_of(kind)
        {
            let allowed = |k: &str| k == "body" || model.fields.iter().any(|f| f.name == k);
            let mut dropped: Vec<String> = e
                .payload
                .iter()
                .map(|(k, _)| k.py_str())
                .filter(|k| !allowed(k))
                .collect();
            dropped.sort();
            if !dropped.is_empty() {
                e.payload.retain(|(k, _)| allowed(&k.py_str()));
                warnings.push(format!("dropped unknown keys: {}", dropped.join(", ")));
            }
        }
        warnings.extend(e.notes.iter().cloned());
        res.insert("warnings".into(), json!(warnings));
        let mut overrides: Vec<String> = Vec::new();
        if info.as_ref().is_some_and(|i| i["gate"] == "overridden") {
            overrides.push("eval_blocked".into());
        }
        if e.scan_secrets && kind == "skill" {
            let hits = Self::secret_files(&e.files);
            let has_reason = gate.is_some_and(|g| g.override_reason.as_ref().is_some_and(|r| !r.is_empty()));
            if !hits.is_empty() && has_reason {
                overrides.push("secret_detected".into());
            } else if !hits.is_empty() && refusal.is_none() {
                refusal = Some("secret_detected".into());
                res.insert("secret_files".into(), json!(hits.iter().take(10).collect::<Vec<_>>()));
            }
        }
        if let Some(code) = refusal {
            let mut detail = refused_text(&code).to_string();
            let reasons: Vec<String> = info
                .as_ref()
                .and_then(|i| i["reasons"].as_array())
                .map(|a| a.iter().take(3).map(jstr).collect())
                .unwrap_or_default();
            if !reasons.is_empty() && code != "secret_detected" {
                detail.push_str(&format!(": {}", reasons.join("; ")));
            }
            if code == "secret_detected" {
                let files: Vec<String> = res["secret_files"]
                    .as_array()
                    .map(|a| a.iter().map(jstr).collect())
                    .unwrap_or_default();
                detail.push_str(&format!(" ({})", files.join(", ")));
            }
            res.insert("status".into(), json!("refused"));
            res.insert("code".into(), json!(code));
            res.insert("detail".into(), json!(detail));
            return Ok(());
        }
        let written = self.write(client, kind, &final_name, e, replaced);
        let outcome = match &written {
            Ok(()) => {
                let status = if replaced {
                    "replaced"
                } else if final_name != name {
                    "renamed"
                } else {
                    "created"
                };
                res.insert("status".into(), json!(status));
                "written".to_string()
            }
            Err(x) => format!("failed: {}", x.type_name()),
        };
        // the override was USED even if the write then failed: always on record
        if !overrides.is_empty()
            && let Some(g) = gate
            && let Some(audit) = &g.audit
        {
            let mut a = match &info {
                Some(Value::Object(m)) => m.clone(),
                _ => Map::new(),
            };
            a.insert("overrides".into(), json!(overrides));
            a.insert("outcome".into(), json!(outcome));
            a.insert("override_reason".into(), json!(g.override_reason));
            a.insert("content_hash".into(), json!(content_hash(&e.files)));
            a.insert("key".into(), json!(e.observed_key));
            audit(&final_name, &Value::Object(a));
        }
        written
    }

    /// `_secret_files(files)`: text files in which the hub's secret scrubber would redact something.
    fn secret_files(files: &Files) -> Vec<String> {
        let mut rels: Vec<&String> = files.keys().collect();
        rels.sort();
        let mut hits = Vec::new();
        for rel in rels {
            let data = &files[rel];
            if data[..data.len().min(4096)].contains(&0) {
                continue;
            }
            let text = String::from_utf8_lossy(data);
            if scrub(&text) != text {
                hits.push(rel.clone());
            }
        }
        hits
    }

    /// `_prepare_skill(name, src)`: files as they will be stored: verbatim, except SKILL.md's `name` is forced to the
    /// (possibly renamed) directory name.
    fn prepare_skill(name: &str, src: &Files) -> XResult<Files> {
        let mut files = src.clone();
        if let Some(raw) = files.get("SKILL.md")
            && let Ok(text) = decode_utf8(raw)
        {
            match split_frontmatter_x(&text) {
                Ok((mut meta, body)) => {
                    if dget(&meta, "name") != Some(&PyValue::str(name)) {
                        dset(&mut meta, "name", PyValue::str(name));
                        files.insert("SKILL.md".into(), yamlsafe::join_frontmatter(&meta, &body).into_bytes());
                    }
                }
                Err(e) if e.is_yaml() => {} // left verbatim; the item shows its issues
                Err(e) => return Err(e),
            }
        }
        Ok(files)
    }

    fn write(&self, client: &str, kind: &str, name: &str, e: &Entry, replace: bool) -> XResult<()> {
        if kind == "skill" {
            let files = Self::prepare_skill(name, &e.files)?;
            if !files.contains_key("SKILL.md") {
                return Err(Problem::bad_request("skill has no SKILL.md").into());
            }
            self.work.validate_skill_files_x(&files)?; // BEFORE anything is removed or written
            let ev = e.eval.clone().filter(|v| !v.is_empty());
            let (source, mut prov) = match &e.observed {
                Some(obs) => {
                    let mut p = obs.clone();
                    dset(&mut p, "imported_at", PyValue::Int(int_now()));
                    (format!("observed:{client}"), p)
                }
                None => (
                    format!("import:{client}"),
                    vec![(s("client"), s(client)), (s("imported_at"), PyValue::Int(int_now()))],
                ),
            };
            if let Some(ev) = ev {
                dset(&mut prov, "evaluation", PyValue::Dict(ev));
            }
            let side = pyd::validate(
                models::sidecar(),
                &PyValue::Dict(vec![(s("source"), s(&source)), (s("provenance"), PyValue::Dict(prov))]),
            )?;
            if replace && self.work.names_x("skill")?.iter().any(|n| n == name) {
                self.work.swap_skill_files_x(name, &files, &side)?;
            } else {
                self.work.write_skill_files_x(name, &files, &side)?;
            }
            return Ok(());
        }
        let mut payload = e.payload.clone();
        let key = if matches!(kind, "instruction" | "rule") {
            "id"
        } else {
            "name"
        };
        dset(&mut payload, key, PyValue::str(name));
        self.work.put_x(kind, name, &PyValue::Dict(payload), None)?;
        Ok(())
    }
}
