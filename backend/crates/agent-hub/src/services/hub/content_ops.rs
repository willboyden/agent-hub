//! services/hub.py methods for content, clients, import and dry runs, plus the content/organize/inbox endpoints of
//! api/routers/{content,organize}.py that run under `hub.mutating()`. Owner: WS-B1.

use std::path::Path;
use std::sync::Arc;

use hub_core::common::{now, redact, scrub, sha256};
use hub_core::ids::{KINDS, plural};
use hub_core::models::{self, ClientDoc, Member};
use hub_core::py::text::{repr, strip, take_chars};
use hub_core::value::PyValue;
use hub_core::{HubResult, Problem, pyd};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::super::audit::AuditService;
use super::super::content::{Catalog, Item, dump_model, invalid, sidecar_json, truthy, yaml_load_x};
use super::super::importer::{GateAudit, GateCheck, ImportGate};
use super::super::lock::{Lock, LockEntry};
use super::super::resolver::{Resolved, client_caps, client_config, enabled_keys, manage_flags};
use super::super::store::page_list;
use super::super::suggest::suggest;
use super::Hub;
use crate::adapters::base::{
    AgentItem, ClientAdapter, ClientConfig, Diag, InstructionItem, McpItem, RenderContext, SkillItem,
};

/// `list_items` filters (GET /<plural> query parameters, already validated by the API layer).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListFilter {
    pub q: Option<String>,
    pub group: Option<String>,
    pub tag: Option<String>,
    pub client: Option<String>,
    pub enabled: Option<bool>,
    pub source: Option<String>,
    pub issues: Option<bool>,
    pub sort: String,
    pub limit: i64,
    pub cursor: Option<String>,
}

/// `_NO_DRIFT`.
fn no_drift() -> Value {
    json!({"edited_outside": [], "pending_changes": 0})
}

/// `isinstance(exc, Unavailable)`: every 503 but the two plain `ProblemError(status=503)`.
fn is_unavailable(p: &Problem) -> bool {
    super::super::content::py_class(p) == "Unavailable"
}

fn to_value<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn jstr(v: &Value) -> String {
    PyValue::from_json(v).py_str()
}

impl Hub {
    // -- listing with filters ----------------------------------------------------------------------------------
    /// `_enabled_map(cat)`: (kind, name) -> the clients that enable it.
    fn enabled_clients(&self, cat: &Catalog) -> IndexMap<(String, String), Vec<String>> {
        let mut out: IndexMap<(String, String), Vec<String>> = IndexMap::new();
        for cid in cat.clients.keys() {
            let profile = cat.profiles.get(cid).cloned().unwrap_or_default();
            let (via, _) = enabled_keys(&profile, cat);
            for k in via.keys() {
                out.entry(k.clone()).or_default().push(cid.clone());
            }
        }
        out
    }

    /// `item_summary(item, cat, enabled)`.
    pub fn item_summary(&self, item: &Item, cat: &Catalog, enabled: &IndexMap<(String, String), Vec<String>>) -> Value {
        let mut colls: Vec<String> = cat
            .collections
            .values()
            .filter(|c| c.members.iter().any(|m| m.kind == item.kind && m.name == item.name))
            .map(|c| c.id.clone())
            .collect();
        colls.sort();
        let mut enabled_for = enabled
            .get(&(item.kind.clone(), item.name.clone()))
            .cloned()
            .unwrap_or_default();
        enabled_for.sort();
        json!({
            "kind": item.kind, "name": item.name, "title": item.title(),
            "description": take_chars(&item.description(), 300), "tags": item.tags(), "groups": item.groups(),
            "collections": colls, "source": item.source(), "valid": item.valid(),
            "issues": item.issues.iter().map(to_value).collect::<Vec<_>>(), "updated": item.updated,
            "enabled_for": enabled_for,
        })
    }

    /// GET /content/validate: `{"ok", "problems"}`.
    pub fn validate_content(&self) -> HubResult<Value> {
        let cat = self.work.catalog()?;
        Ok(json!({"ok": cat.problems.is_empty(),
                  "problems": cat.problems.iter().map(to_value).collect::<Vec<_>>()}))
    }

    /// GET /<plural>.
    pub fn list_items(&self, kind: &str, f: &ListFilter) -> HubResult<Value> {
        let cat = self.work.catalog()?;
        let emap = self.enabled_clients(&cat);
        let mut rows: Vec<Value> = cat
            .of_kind(kind)
            .iter()
            .map(|i| self.item_summary(i, &cat, &emap))
            .collect();
        let s = |r: &Value, k: &str| r[k].as_str().unwrap_or_default().to_string();
        let list = |r: &Value, k: &str| -> Vec<String> {
            r[k].as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        if let Some(q) = f.q.as_deref().filter(|q| !q.is_empty()) {
            let ql = q.to_lowercase();
            rows.retain(|r| {
                s(r, "name").to_lowercase().contains(&ql)
                    || s(r, "title").to_lowercase().contains(&ql)
                    || s(r, "description").to_lowercase().contains(&ql)
                    || list(r, "tags").iter().any(|t| t.contains(&ql))
            });
        }
        if let Some(g) = f.group.as_deref().filter(|g| !g.is_empty()) {
            rows.retain(|r| list(r, "collections").iter().any(|c| c == g) || list(r, "groups").iter().any(|c| c == g));
        }
        if let Some(t) = f.tag.as_deref().filter(|t| !t.is_empty()) {
            rows.retain(|r| list(r, "tags").iter().any(|x| x == t));
        }
        if let Some(src) = f.source.as_deref().filter(|x| !x.is_empty()) {
            rows.retain(|r| s(r, "source").starts_with(src));
        }
        if let Some(want) = f.issues {
            rows.retain(|r| r["issues"].as_array().is_some_and(|a| !a.is_empty()) == want);
        }
        if let Some(client) = f.client.as_deref().filter(|c| !c.is_empty()) {
            if !cat.clients.contains_key(client) {
                return Err(Problem::not_found(format!("no such client: {client}")));
            }
            let on = |r: &Value| list(r, "enabled_for").iter().any(|c| c == client);
            match f.enabled {
                Some(true) => rows.retain(on),
                Some(false) => rows.retain(|r| !on(r)),
                None => {}
            }
        } else if let Some(want) = f.enabled {
            rows.retain(|r| r["enabled_for"].as_array().is_some_and(|a| !a.is_empty()) == want);
        }
        match f.sort.as_str() {
            "updated" => rows.sort_by(|a, b| {
                let (ua, ub) = (
                    a["updated"].as_f64().unwrap_or(0.0),
                    b["updated"].as_f64().unwrap_or(0.0),
                );
                ub.partial_cmp(&ua)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| s(a, "name").cmp(&s(b, "name")))
            }),
            "title" => rows.sort_by(|a, b| {
                (s(a, "title").to_lowercase(), s(a, "name")).cmp(&(s(b, "title").to_lowercase(), s(b, "name")))
            }),
            "name" => rows.sort_by_key(|r| s(r, "name")),
            _ => return Err(Problem::bad_request("sort must be name, title or updated")),
        }
        Ok(page_list(rows, f.limit, f.cursor.as_deref()))
    }

    /// GET /<plural>/{name} (and the response of PUT / duplicate).
    pub fn item_detail(&self, kind: &str, name: &str) -> HubResult<Value> {
        let item = self.work.load(kind, name)?;
        let cat = self.work.catalog()?;
        let Value::Object(mut out) = self.item_summary(&item, &cat, &self.enabled_clients(&cat)) else {
            return Err(Problem::internal());
        };
        if let Some(meta) = item.meta_json() {
            out.insert("meta".into(), meta);
        }
        out.insert("body".into(), Value::String(item.body.clone()));
        if kind == "skill" {
            let mut rels: Vec<&String> = item.files.keys().collect();
            rels.sort();
            let mut files = Vec::new();
            let mut texts = Map::new();
            for rel in rels {
                let blob = &item.files[rel];
                let binary = blob[..blob.len().min(4096)].contains(&0);
                files.push(json!({"path": rel, "size": blob.len(), "binary": binary}));
                texts.insert(
                    rel.clone(),
                    if binary {
                        Value::Null
                    } else {
                        Value::String(String::from_utf8_lossy(blob).into_owned())
                    },
                );
            }
            out.insert("file_list".into(), Value::Array(files));
            out.insert("files".into(), Value::Object(texts));
            if let Some(side) = &item.sidecar {
                out.insert("sidecar".into(), sidecar_json(side));
            }
        }
        Ok(Value::Object(out))
    }

    /// PUT /<plural>/{name}: `work.put(kind, name, body)` under `mutating()`, then the detail.
    pub fn put_item(&self, kind: &str, name: &str, body: &PyValue) -> HubResult<Value> {
        let _g = self.mutating()?;
        self.work.put(kind, name, body, None)?;
        self.item_detail(kind, name)
    }

    /// DELETE /<plural>/{name} under `mutating()`.
    pub fn delete_item(&self, kind: &str, name: &str) -> HubResult<()> {
        let _g = self.mutating()?;
        self.work.delete(kind, name)
    }

    /// POST /skills/{name}/duplicate under `mutating()`: the new skill's detail.
    pub fn duplicate_skill(&self, name: &str, new_name: &str) -> HubResult<Value> {
        let _g = self.mutating()?;
        self.work.duplicate_skill(name, new_name)?;
        self.item_detail("skill", new_name)
    }

    // -- clients -----------------------------------------------------------------------------------------------
    /// `_adapter_for(doc)`: the installed adapter (422 `unknown_adapter`).
    fn adapter_for(&self, adapter: &str) -> HubResult<Arc<dyn ClientAdapter>> {
        self.registry.get(adapter).ok_or_else(|| {
            Problem::unprocessable(format!("adapter {} is not installed", repr(adapter))).code("unknown_adapter")
        })
    }

    /// POST /clients (create) and PUT /clients/{cid}; payload without `id`, `spec_yaml` accepted.
    pub fn put_client(&self, cid: &str, payload: &IndexMap<String, PyValue>, create: bool) -> HubResult<Value> {
        if cid == "hub" || cid.contains(':') {
            return Err(Problem::unprocessable(
                "the client id `hub` is reserved (it names the hub's own skills in evaluation subjects), and an id \
                 cannot contain `:`",
            )
            .code("reserved_client_id"));
        }
        let payload = self.spec_body(payload)?;
        let _g = self.lock.lock();
        let exists = self.work.names_of("clients")?.iter().any(|c| c == cid);
        if create && exists {
            return Err(Problem::conflict("client already exists").code("already_exists"));
        }
        if !create && !exists {
            return Err(Problem::not_found(format!("no such client: {cid}")));
        }
        let mut data: Vec<(PyValue, PyValue)> = payload
            .iter()
            .filter(|(k, _)| k.as_str() != "id")
            .map(|(k, v)| (PyValue::str(k.as_str()), v.clone()))
            .collect();
        data.push((PyValue::str("id"), PyValue::str(cid)));
        let v = pyd::validate(models::client_doc(), &PyValue::Dict(data))
            .map_err(|e| invalid("client failed validation", &e))?;
        let doc = ClientDoc::from_validated(&v);
        self.adapter_for(&doc.adapter)?;
        match client_config(&doc, None) {
            Ok(cfg) => {
                for rp in cfg.roots.values() {
                    if let Some(prob) = self.guard.root_problem(Path::new(rp)) {
                        return Err(Problem::unprocessable(format!("invalid roots: {prob}")).code("root_not_allowed"));
                    }
                }
            }
            Err(e) => return Err(Problem::unprocessable(format!("invalid roots: {e}"))),
        }
        self.work.put_client(cid, &dump_model(models::client_doc(), &v, true))?;
        self.client_view(cid)
    }

    /// GET /clients/{cid}.
    pub fn client_view(&self, cid: &str) -> HubResult<Value> {
        let doc = self.work.get_client(cid)?;
        let adapter = self.registry.get(&doc.adapter);
        let status = self.client_status_x(cid)?;
        let Value::Object(mut out) = doc.dump(true) else {
            return Err(Problem::internal());
        };
        out.insert(
            "manage_effective".into(),
            to_value(&manage_flags(&doc, adapter.as_deref())),
        );
        out.insert("adapter_installed".into(), Value::Bool(adapter.is_some()));
        out.insert(
            "caps".into(),
            match &adapter {
                Some(a) => to_value(&client_caps(a.as_ref(), &doc)?),
                None => Value::Null,
            },
        );
        out.insert("status".into(), status["status"].clone());
        out.insert("drift".into(), status["drift"].clone());
        out.insert("committed".into(), status["committed"].clone());
        out.insert(
            "observed_summary".into(),
            self.observe_svc.summary(cid).unwrap_or(Value::Null),
        );
        out.extend(self.lock_info(cid));
        Ok(Value::Object(out))
    }

    /// `_lock_info(cid)`.
    fn lock_info(&self, cid: &str) -> Map<String, Value> {
        let lk = self.locks.load(cid);
        let nz = |f: f64| if f == 0.0 { Value::Null } else { json!(f) };
        let mut m = Map::new();
        m.insert(
            "adopted_at".into(),
            lk.as_ref().map_or(Value::Null, |l| nz(l.adopted_at)),
        );
        m.insert(
            "last_applied_at".into(),
            lk.as_ref().map_or(Value::Null, |l| nz(l.applied_at)),
        );
        m.insert(
            "last_applied_plan".into(),
            lk.as_ref()
                .filter(|l| !l.plan_id.is_empty())
                .map_or(Value::Null, |l| json!(l.plan_id)),
        );
        m.insert(
            "last_verified_at".into(),
            json!(lk.as_ref().and_then(|l| l.verified_at)),
        );
        m.insert(
            "last_verified_ok".into(),
            json!(lk.as_ref().and_then(|l| l.verified_ok)),
        );
        m
    }

    /// GET /clients (paged by the API layer).
    pub fn list_clients(&self) -> HubResult<Vec<Value>> {
        self.work
            .names_of("clients")?
            .iter()
            .map(|c| self.client_view(c))
            .collect()
    }

    /// `client_status(cid)` as Python raises it: only `Unavailable` (no repo yet, git missing) becomes status `error`;
    /// other problems of the committed snapshot (an unsafe content dir) propagate.
    pub fn client_status_x(&self, cid: &str) -> HubResult<Value> {
        let cat = match self.approved().and_then(|(_, store)| store.catalog()) {
            Ok(c) => c,
            Err(p) if is_unavailable(&p) => {
                return Ok(json!({"status": "error", "drift": no_drift(), "committed": false}));
            }
            Err(p) => return Err(p),
        };
        if !cat.clients.contains_key(cid) {
            return Ok(json!({"status": "never_applied", "drift": no_drift(), "committed": false}));
        }
        let probe = self
            .planner
            .plan_client(&cat, cid, &[])
            .and_then(|ip| self.planner.drift(&ip));
        Ok(match probe {
            Ok(d) => json!({"status": d["status"], "drift": d["drift"], "committed": true}),
            Err(_) => json!({"status": "error", "drift": no_drift(), "committed": true}),
        })
    }

    /// `client_status(cid)`: `{"status", "drift", "committed"}` (never fails: errors become status "error").
    pub fn client_status(&self, cid: &str) -> Value {
        self.client_status_x(cid)
            .unwrap_or_else(|_| json!({"status": "error", "drift": no_drift(), "committed": false}))
    }

    pub fn delete_client(&self, cid: &str) -> HubResult<()> {
        {
            let _g = self.lock.lock();
            self.work.delete_client(cid)?;
        }
        self.observe_svc.forget(cid);
        Ok(())
    }

    fn catalog_for(&self, cid: &str, source: &str) -> HubResult<Catalog> {
        match source {
            "working" => {
                let cat = self.work.catalog()?;
                if !cat.clients.contains_key(cid) {
                    return Err(Problem::not_found(format!("no such client: {cid}")));
                }
                Ok(cat)
            }
            "committed" => Ok(self.approved_client(cid)?.1),
            _ => Err(Problem::bad_request("source must be committed or working")),
        }
    }

    /// GET /clients/{cid}/effective?source=committed|working.
    pub fn effective(&self, cid: &str, source: &str) -> HubResult<Value> {
        let cat = self.catalog_for(cid, source)?;
        let r = self.resolver.resolve(&cat, cid)?;
        let caps = match &r.adapter {
            Some(a) => to_value(&client_caps(a.as_ref(), &r.client)?),
            None => Value::Null,
        };
        Ok(json!({
            "client": cid, "source": source, "adapter": r.client.adapter, "adapter_installed": r.adapter.is_some(),
            "caps": caps, "manage": r.manage, "items": r.entries,
            "rules": Self::rules_with_source(&r),
            "default_tool_decision": r.permissions.default_tool_decision,
            "network_default": r.permissions.network_default,
            "mcp": r.mcp.iter().map(|m| json!({"name": m.name, "transport": m.transport,
                                               "egress_hosts": m.egress_hosts, "scan_status": m.scan_status}))
                       .collect::<Vec<_>>(),
            "egress": {"default": r.permissions.network_default, "allow_hosts": r.allow_hosts},
            "findings": r.findings.iter().map(to_value).collect::<Vec<_>>(),
            "diagnostics": r.diagnostics.iter().map(to_value).collect::<Vec<_>>(),
            "warnings": r.warnings,
        }))
    }

    /// `_rules_with_source(r)`: every effective rule with `floor` | `profile` | `content`.
    fn rules_with_source(r: &Resolved) -> Vec<Value> {
        let rules = &r.permissions.rules;
        let n_floor = rules.len().saturating_sub(r.kept_rules.len());
        rules
            .iter()
            .enumerate()
            .map(|(i, x)| {
                let mut v = to_value(x);
                let src = if i < n_floor {
                    "floor".to_string()
                } else {
                    r.kept_sources.get(i - n_floor).cloned().unwrap_or_default()
                };
                if let Value::Object(m) = &mut v {
                    m.insert("source".into(), Value::String(src));
                }
                v
            })
            .collect()
    }

    /// GET /clients/{cid}/rendered?kind&name&source: the artifacts one item contributes for one client, from the same
    /// render the planner uses.
    pub fn rendered(&self, cid: &str, kind: &str, name: &str, source: &str) -> HubResult<Value> {
        if !KINDS.contains(&kind) {
            return Err(Problem::bad_request("unknown kind"));
        }
        let cat = self.catalog_for(cid, source)?;
        let res = self.resolver.resolve(&cat, cid)?;
        let (Some(adapter), Some(ctx)) = (&res.adapter, &res.ctx) else {
            return Err(Problem::unavailable("adapter not available for this client").code("adapter_missing"));
        };
        let pl = plural(kind).unwrap_or_default();
        let listed = res
            .entries
            .get(pl)
            .is_some_and(|l| l.iter().any(|e| e["name"].as_str() == Some(name)));
        if !listed {
            let why = self
                .resolver
                .block_reason_x(&cat, &res, kind, name)
                .map_err(super::super::content::Exc::into_problem)?
                .unwrap_or_else(|| "it is not enabled for this client (profile or collection)".into());
            return Err(
                Problem::not_found(format!("{kind} {} does not render for {cid}: {why}", repr(name)))
                    .code("not_rendered"),
            );
        }
        let rr = adapter.render(ctx).map_err(|e| {
            Problem::unavailable(format!("adapter render failed ({})", e.py_type)).code("render_failed")
        })?;
        let mut arts = Vec::new();
        for a in &rr.artifacts {
            if !a.source_ids.iter().any(|s| s == name) {
                continue;
            }
            let text = String::from_utf8_lossy(&a.content);
            let n = text.chars().count();
            arts.push(json!({"root": a.root, "path": a.path, "merge": a.merge, "kind": a.kind,
                             "source_ids": a.source_ids, "managed_keys": a.managed_keys, "size": a.content.len(),
                             "content": scrub(take_chars(&text, 100_000)), "truncated": n > 100_000}));
        }
        let diags: Vec<Value> = rr
            .diagnostics
            .iter()
            .filter(|d| d.item.as_deref().is_none_or(|i| i == name))
            .map(to_value)
            .collect();
        Ok(
            json!({"client": cid, "kind": kind, "name": name, "source": source, "artifacts": arts,
                  "diagnostics": diags}),
        )
    }

    /// `_spec_body(body)`: accept `spec_yaml` (safe YAML, 64 KB) in place of a `spec` object.
    fn spec_body(&self, body: &IndexMap<String, PyValue>) -> HubResult<IndexMap<String, PyValue>> {
        let Some(text) = body.get("spec_yaml") else {
            return Ok(body.clone());
        };
        if body.get("spec").is_some_and(|s| !s.is_none()) {
            return Err(Problem::unprocessable("send either `spec` or `spec_yaml`, not both"));
        }
        let Some(text) = text.as_str() else {
            return Err(Problem::unprocessable("`spec_yaml` must be a string"));
        };
        let spec = match yaml_load_x(text, 64 * 1024) {
            Ok(v) => v,
            Err(e) if e.is_yaml() => {
                return Err(Problem::unprocessable(format!("spec_yaml: {}", e.message())).code("invalid_yaml"));
            }
            Err(e) => return Err(e.into_problem()),
        };
        if spec.as_dict().is_none() {
            return Err(Problem::unprocessable("spec_yaml must be a mapping"));
        }
        let mut out: IndexMap<String, PyValue> = body
            .iter()
            .filter(|(k, _)| k.as_str() != "spec_yaml")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.insert("spec".into(), spec);
        Ok(out)
    }

    /// POST /clients/validate-spec: dry-run a generic spec against sample content. Pure: renders, never touches a
    /// filesystem. Python validates the spec (generic.validate_spec) and then dry-renders it; the generic adapter's own
    /// render does the same parse first and returns exactly those diagnostics with no artifacts for an invalid spec,
    /// so rendering the dry-run client with it is equivalent.
    pub fn validate_spec(&self, body: &IndexMap<String, PyValue>) -> HubResult<Value> {
        let body = self.spec_body(body)?;
        let Some(adapter) = self.registry.get("generic") else {
            return Err(Problem::unavailable("the generic adapter is not installed").code("adapter_missing"));
        };
        let spec = body.get("spec").cloned().unwrap_or_default();
        if spec.as_dict().is_none() {
            return Err(Problem::unprocessable("`spec` must be a mapping"));
        }
        let roots_v = body
            .get("roots")
            .filter(|v| truthy(v))
            .cloned()
            .unwrap_or_else(|| PyValue::Dict(vec![(PyValue::str("home"), PyValue::str("/tmp/agent-hub-dry-run"))]));
        let Some(roots) = roots_v.as_dict() else {
            return Err(Problem::internal()); // AttributeError: .items() of a non-dict
        };
        let roots = PyValue::Dict(
            roots
                .iter()
                .map(|(k, v)| (k.clone(), PyValue::Str(v.py_str())))
                .collect(),
        );
        let strict = body.get("strict").is_some_and(truthy);
        let raw = PyValue::Dict(vec![
            (PyValue::str("id"), PyValue::str("dry-run")),
            (PyValue::str("adapter"), PyValue::str("generic")),
            (PyValue::str("display_name"), PyValue::str("Dry run")),
            (PyValue::str("roots"), roots),
            (PyValue::str("spec"), spec),
            (PyValue::str("strict"), PyValue::Bool(strict)),
        ]);
        let v = pyd::validate(models::client_doc(), &raw).map_err(|e| invalid("spec failed validation", &e))?;
        let doc = ClientDoc::from_validated(&v);
        let cfg = client_config(&doc, None)
            .map_err(|e| Problem::unprocessable("spec failed validation").with("errors", json!([{"message": e.0}])))?;
        let ctx = self.sample_context(cfg)?;
        let rr = match adapter.render(&ctx) {
            Ok(rr) => rr,
            Err(e) => {
                let d = Diag {
                    severity: "error".into(),
                    code: "render_failed".into(),
                    message: format!("{}: {}", e.py_type, take_chars(&e.message, 200)),
                    item: None,
                };
                return Ok(json!({"ok": false, "artifacts": [], "diagnostics": [to_value(&d)]}));
            }
        };
        let arts: Vec<Value> = rr
            .artifacts
            .iter()
            .map(|a| {
                let text = String::from_utf8_lossy(&a.content);
                json!({"root": a.root, "path": a.path, "kind": a.kind, "merge": a.merge, "size": a.content.len(),
                       "managed_keys": a.managed_keys, "preview": scrub(take_chars(&text, 4000))})
            })
            .collect();
        Ok(
            json!({"ok": !rr.diagnostics.iter().any(|d| d.severity == "error"), "artifacts": arts,
                  "diagnostics": rr.diagnostics.iter().map(to_value).collect::<Vec<_>>()}),
        )
    }

    /// `_sample_context(cfg)`.
    fn sample_context(&self, cfg: ClientConfig) -> HubResult<RenderContext> {
        let mut files = crate::adapters::base::Files::new();
        files.insert(
            "SKILL.md".into(),
            b"---\nname: sample-skill\ndescription: A sample skill\n---\nDo the sample thing.\n".to_vec(),
        );
        Ok(RenderContext {
            client: cfg,
            skills: vec![SkillItem {
                name: "sample-skill".into(),
                description: "A sample skill".into(),
                files,
                ..SkillItem::default()
            }],
            agents: vec![AgentItem {
                name: "sample-agent".into(),
                description: "A sample agent".into(),
                capabilities: vec!["read".into(), "shell".into()],
                model_tier: Some("standard".into()),
                mode: "subagent".into(),
                read_only: false,
                body: "You are a sample agent.".into(),
            }],
            instructions: vec![InstructionItem {
                id: "sample-rules".into(),
                title: "Sample rules".into(),
                order: 10,
                body: "Be careful.".into(),
            }],
            mcp_servers: vec![McpItem {
                name: "sample-mcp".into(),
                transport: "stdio".into(),
                command: Some("sample-mcp".into()),
                scan_status: "clean".into(),
                ..McpItem::default()
            }],
            memory: Vec::new(),
            permissions: self.floor.enforce(&[], None)?.permissions,
            endpoints: self.work_endpoints(),
        })
    }

    /// `work_endpoints()`: the working tree's endpoints, `{}` on any error.
    pub fn work_endpoints(&self) -> IndexMap<String, String> {
        self.work.endpoints().unwrap_or_default()
    }

    // -- import ------------------------------------------------------------------------------------------------
    /// `_import_gate(override_reason, actor, client, setting)`: the gate for one import request. An override reason is
    /// 10..500 characters (stripped) and every write it lets through is recorded as `skill.import.override`.
    fn import_gate(
        &self,
        override_reason: Option<&str>,
        actor: &str,
        client: &str,
        setting: &str,
    ) -> HubResult<ImportGate> {
        let reason = override_reason.map(|r| strip(r).to_string());
        if override_reason.is_some() && !(10..=500).contains(&reason.as_deref().unwrap_or_default().chars().count()) {
            return Err(
                Problem::unprocessable("override_reason must be 10 to 500 characters").code("bad_override_reason")
            );
        }
        let weak = self.skilleval.hub.clone();
        let check: GateCheck = Arc::new(move |h: &str| match weak.upgrade() {
            Some(hub) => hub.skilleval.gate(h),
            None => json!({"status": "missing"}),
        });
        let audit_svc = AuditService::new(self.store.clone());
        let (actor, client) = (actor.to_string(), client.to_string());
        let audit: GateAudit = Arc::new(move |name: &str, info: &Value| {
            let reasons: Vec<String> = info
                .get("reasons")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|x| take_chars(&jstr(x), 200).to_string())
                        .take(5)
                        .collect()
                })
                .unwrap_or_default();
            let ch = info
                .get("content_hash")
                .filter(|v| truthy(&PyValue::from_json(v)))
                .map(jstr)
                .unwrap_or_default();
            let detail = json!({
                "client": client, "skill": name, "key": info.get("key"), "content_hash": take_chars(&ch, 12),
                "run_id": info.get("run_id"), "gate_status": info.get("gate_status"),
                "overrides": info.get("overrides"), "outcome": info.get("outcome"), "reasons": reasons,
                "reason": redact(info.get("override_reason").unwrap_or(&Value::Null)),
            });
            let _ = audit_svc.event(&actor, "skill.import.override", &detail);
        });
        Ok(ImportGate {
            mode: self.setting_str(setting),
            check,
            override_reason: reason.filter(|r| !r.is_empty()),
            audit: Some(audit),
        })
    }

    /// POST /import: import into the working tree, then record what the client has RIGHT NOW as the last-applied
    /// state, so the first plan is a normal change/unchanged instead of a wall of conflicts. Skills pass the gate (F8).
    #[allow(clippy::too_many_arguments)]
    pub fn import_items(
        &self,
        client: &str,
        selection: Option<&[(String, String)]>,
        on_conflict: Option<&str>,
        enable: bool,
        overrides: &IndexMap<(String, String), String>,
        override_reason: Option<&str>,
        actor: &str,
    ) -> HubResult<Value> {
        let gate = self.import_gate(override_reason, actor, client, "eval_gate")?;
        let _g = self.mutating()?;
        let mut out = self
            .importer
            .do_import(client, selection, on_conflict, enable, overrides, &gate)?;
        let adopted = self.adopt_live(client)?;
        if let Value::Object(m) = &mut out {
            m.insert("adopted_live".into(), json!(adopted));
            m.insert("gate_mode".into(), json!(gate.mode));
        }
        Ok(out)
    }

    /// POST /clients/{cid}/observed/import: content skills from the client's stored observation (working tree only;
    /// enable defaults to false).
    pub fn import_observed(
        &self,
        client: &str,
        items: &[Value],
        on_conflict: &str,
        enable: bool,
        override_reason: Option<&str>,
        actor: &str,
    ) -> HubResult<Value> {
        let gate = self.import_gate(override_reason, actor, client, "eval_gate_observed")?;
        let _g = self.mutating()?;
        let mut out = self
            .importer
            .import_observed(client, items, on_conflict, enable, &gate, &self.observe_svc)?;
        let enabled = out["enabled"] == Value::Bool(true);
        let adopted = if enabled { Some(self.adopt_live(client)?) } else { None };
        if let Value::Object(m) = &mut out {
            if let Some(n) = adopted {
                m.insert("adopted_live".into(), json!(n));
            }
            m.insert("gate_mode".into(), json!(gate.mode));
        }
        Ok(out)
    }

    /// `_adopt_live(client)`: lock entries for live files that already match what the hub would write.
    fn adopt_live(&self, client: &str) -> HubResult<i64> {
        let cat = self.work.catalog()?;
        if !cat.clients.contains_key(client) {
            return Ok(0);
        }
        let ip = self.planner.plan_client(&cat, client, &[])?;
        let mut lock = ip.lock.clone().unwrap_or_else(|| Lock::new(client));
        let mut n = 0;
        for pf in &ip.files {
            let d = &pf.desired;
            let key = pf.key();
            if lock.files.contains_key(&key)
                || !d.managed
                || d.content.is_none()
                || !pf.merged.unparseable.is_empty()
                || !pf.adoptable
            {
                continue;
            }
            let (Some(live), Some(slice)) = (&pf.live, &pf.merged.slice_live) else {
                continue;
            };
            lock.files.insert(
                key,
                LockEntry {
                    root: d.root.clone(),
                    path: d.path.clone(),
                    kind: d.kind.clone(),
                    sha256: sha256(live),
                    slice_sha256: slice.clone(),
                    merge: d.merge.clone(),
                    managed_keys: d.managed_keys.clone(),
                    created: false,
                    source_ids: d.source_ids.clone(),
                    mode: d.mode,
                },
            );
            n += 1;
        }
        if n > 0 {
            lock.adopted_at = now();
            self.locks.save(&mut lock, false)?;
        }
        Ok(n)
    }

    /// POST /clients/{cid}/discover.
    pub fn discover(&self, cid: &str) -> HubResult<Value> {
        self.importer.discover(cid)
    }

    // -- collections, profiles, matrix (api/routers/organize.py) ---------------------------------------------
    /// GET /collections (paged by the API layer): `CollectionDoc.model_dump(mode="json")` rows.
    pub fn list_collections(&self) -> HubResult<Vec<Value>> {
        Ok(self.work.list_collections()?.iter().map(to_value).collect())
    }

    pub fn suggestions(&self) -> HubResult<Vec<Value>> {
        suggest(&self.work.catalog()?)
    }

    pub fn accept_suggestions(&self, ids: &[String], overrides: &IndexMap<String, PyValue>) -> HubResult<Vec<Value>> {
        let _g = self.mutating()?;
        self.organizer.accept_suggestions(ids, overrides)
    }

    pub fn create_collection(&self, body: &PyValue) -> HubResult<Value> {
        let _g = self.mutating()?;
        Ok(to_value(&self.organizer.create_collection(body)?))
    }

    pub fn reorder_collections(&self, ids: &[String]) -> HubResult<Vec<Value>> {
        let _g = self.mutating()?;
        Ok(self.organizer.reorder(ids)?.iter().map(to_value).collect())
    }

    pub fn update_collection(&self, cid: &str, body: &PyValue) -> HubResult<Value> {
        let _g = self.mutating()?;
        Ok(to_value(&self.organizer.update_collection(cid, body)?))
    }

    pub fn delete_collection(&self, cid: &str) -> HubResult<()> {
        let _g = self.mutating()?;
        self.work.delete_collection(cid)
    }

    pub fn add_members(&self, cid: &str, members: &[Member]) -> HubResult<Value> {
        let _g = self.mutating()?;
        Ok(to_value(&self.organizer.add_members(cid, members)?))
    }

    pub fn remove_member(&self, cid: &str, kind: &str, name: &str) -> HubResult<Value> {
        let _g = self.mutating()?;
        Ok(to_value(&self.organizer.remove_member(cid, kind, name)?))
    }

    /// POST /items/bulk: the result rows.
    pub fn bulk(
        &self,
        items: &[Member],
        add_to: Option<&str>,
        remove_from: Option<&str>,
        add_tags: &[String],
        remove_tags: &[String],
    ) -> HubResult<Vec<Value>> {
        let _g = self.mutating()?;
        self.organizer.bulk(items, add_to, remove_from, add_tags, remove_tags)
    }

    pub fn get_profile(&self, client: &str) -> HubResult<Value> {
        Ok(to_value(&self.work.get_profile(client)?))
    }

    pub fn put_profile(&self, client: &str, body: &PyValue) -> HubResult<Value> {
        let _g = self.mutating()?;
        if !self.work.names_of("clients")?.iter().any(|c| c == client) {
            return Err(Problem::not_found(format!("no such client: {client}")));
        }
        Ok(to_value(&self.work.put_profile(client, body)?))
    }

    pub fn matrix(&self) -> HubResult<Value> {
        self.organizer.matrix()
    }

    pub fn toggle(&self, client: &str, kind: &str, name: &str, enabled: bool) -> HubResult<Value> {
        let _g = self.mutating()?;
        self.organizer.toggle(client, kind, name, enabled)
    }

    pub fn bulk_toggle(&self, clients: &[String], items: &[Member], enabled: bool) -> HubResult<Vec<Value>> {
        let _g = self.mutating()?;
        self.organizer.bulk_toggle(clients, items, enabled)
    }

    // -- memory inbox (api/routers/content.py) ---------------------------------------------------------------
    /// GET /memory/inbox?client.
    pub fn inbox_list(&self, client: Option<&str>) -> HubResult<Vec<Value>> {
        self.inbox.list(client)
    }

    /// POST /memory/inbox (role checks are done by the API layer).
    pub fn inbox_add(&self, client: &str, title: &str, body: &str, mtype: &str) -> HubResult<Value> {
        let _g = self.mutating()?;
        self.inbox.add(client, title, body, mtype)
    }

    pub fn inbox_promote(&self, iid: &str, edits: Option<&PyValue>) -> HubResult<Value> {
        let _g = self.mutating()?;
        self.inbox.promote(iid, edits)
    }

    pub fn inbox_reject(&self, iid: &str) -> HubResult<Value> {
        let _g = self.mutating()?;
        self.inbox.reject(iid)
    }
}
