//! services/hub.py methods for observed inventories (F3/F9) and the Evaluations view: `_hub_view`, `observe`,
//! `observed_view`, `eval_rows`, `eval_list`, `eval_select`, `eval_run`, `eval_detail`. Reading never starts a process.
//! Owner: WS-C.

use std::collections::BTreeSet;

use hub_core::ids::valid_id;
use hub_core::{HubResult, Problem};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::super::observe::{HubView, ObserveService};
use super::super::skilleval::security_severity;
use super::super::store::page_list;
use super::{Hub, SELECT_CAP};
use crate::adapters::base::OBSERVED_ORIGINS;

/// Run-record fields left out of the Evaluations rows (`Hub._HEAVY`).
const HEAVY: [&str; 4] = ["validators", "validators_llm", "security_findings", "static"];

fn pystr(v: &Value) -> String {
    hub_core::value::PyValue::from_json(v).py_str()
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(m)) => !m.is_empty(),
    }
}

/// `str(d[k])` (a missing key reads as None).
fn sget(v: &Value, k: &str) -> String {
    v.get(k).map_or_else(|| "None".to_string(), pystr)
}

fn arr(v: Option<&Value>) -> Vec<Value> {
    v.and_then(Value::as_array).cloned().unwrap_or_default()
}

/// `dict(Counter(...))` in first-seen order.
fn counter(keys: impl Iterator<Item = String>) -> Value {
    let mut m: IndexMap<String, i64> = IndexMap::new();
    for k in keys {
        *m.entry(k).or_insert(0) += 1;
    }
    Value::Object(m.into_iter().map(|(k, v)| (k, json!(v))).collect())
}

impl Hub {
    /// `_enabled_map(cat)`: (kind, name) -> the clients that enable it.
    fn enabled_map(cat: &super::super::content::Catalog) -> IndexMap<(String, String), Vec<String>> {
        let mut out: IndexMap<(String, String), Vec<String>> = IndexMap::new();
        for cid in cat.clients.keys() {
            let profile = cat.profiles.get(cid).cloned().unwrap_or_default();
            let (via, _) = super::super::resolver::enabled_keys(&profile, cat);
            for k in via.keys() {
                out.entry(k.clone()).or_default().push(cid.clone());
            }
        }
        out
    }

    /// `_hub_view(client)`: what the working tree has, for the comparison with a client's observed skills.
    pub fn hub_view(&self, client: &str) -> HubResult<HubView> {
        let cat = self.work.catalog()?;
        let emap = Self::enabled_map(&cat);
        let mut view = HubView::default();
        for item in cat.of_kind("skill") {
            let h = self.skilleval.current_hash(&format!("hub:{}", item.name));
            view.hash_of.insert(item.name.clone(), h.clone());
            if let Some(h) = h.filter(|h| !h.is_empty()) {
                view.by_hash.entry(h).or_default().push(item.name.clone());
            }
            if emap
                .get(&("skill".to_string(), item.name.clone()))
                .is_some_and(|c| c.iter().any(|c| c == client))
            {
                view.enabled.insert(item.name.clone());
            }
        }
        Ok(view)
    }

    /// POST /clients/{cid}/observe: read what the client REALLY has (adapter.observe, read-only) and replace the stored
    /// observation.
    pub fn observe(&self, cid: &str, actor: &str) -> HubResult<Value> {
        if !self.work.names_of("clients")?.iter().any(|c| c == cid) {
            return Err(Problem::not_found(format!("no such client: {cid}")));
        }
        let doc = self.work.get_client(cid)?;
        let Some(adapter) = self.registry.get(&doc.adapter) else {
            return Err(
                Problem::unavailable(format!("adapter {} is not installed", hub_core::py::repr(&doc.adapter)))
                    .code("adapter_missing"),
            );
        };
        let cfg = super::super::resolver::client_config(&doc, None)
            .map_err(|e| Problem::bad_request(format!("client roots invalid: {e}")))?;
        let fs = super::super::fs::discovery_fs(&cfg.roots, &cfg.params, &self.cfg().discover_extra_roots);
        let obs = adapter.observe(&cfg, &fs, self.observe_runner.as_ref()).map_err(|e| {
            Problem::unavailable(format!("adapter observe failed ({})", e.py_type)).code("observe_failed")
        })?;
        self.observe_svc.store(cid, &obs)?;
        let keys: BTreeSet<String> = self
            .observe_svc
            .index(cid)
            .map(|idx| arr(idx.get("skills")).iter().map(|s| sget(s, "key")).collect())
            .unwrap_or_default();
        self.skilleval.prune_observed(cid, &keys);
        let doc2 = self
            .observe_svc
            .annotated(cid)
            .unwrap_or_else(|| json!({"items": [], "notes": []}));
        let items = arr(doc2.get("items"));
        let counts = ObserveService::count(&items);
        let g = |k: &str| doc2.get(k).cloned().unwrap_or(Value::Null);
        let _ = self.audit_svc.event(
            actor,
            "client.observe",
            &json!({"client": cid, "total": items.len(), "complete": g("complete")}),
        );
        Ok(json!({
            "client": cid, "observed_at": g("observed_at"), "complete": g("complete"), "total": items.len(),
            "by_origin": counts["by_origin"], "by_hub_status": counts["by_hub_status"],
            "notes": doc2.get("notes").cloned().unwrap_or_else(|| json!([])),
            "source": doc2.get("source").cloned().unwrap_or_else(|| json!("")),
        }))
    }

    /// `_evaluation_of(subject, content_hash)`: the latest run's state/verdict and the gate, None when neither exists.
    fn evaluation_of(&self, subject: &str, content_hash: &str) -> Value {
        let latest = self.skilleval.records(subject, 1).into_iter().next();
        let g = self.skilleval.gate(content_hash);
        let status = g.get("status").cloned().unwrap_or(Value::Null);
        if latest.is_none() && status == "missing" {
            return Value::Null;
        }
        let pick = |k: &str| match &latest {
            Some(l) => l.get(k).cloned().unwrap_or(Value::Null),
            None => g.get(k).cloned().unwrap_or(Value::Null),
        };
        json!({"state": self.skilleval.freshness(latest.as_ref(), Some(content_hash)), "verdict": pick("verdict"),
               "gate": status, "run_id": pick("run_id")})
    }

    /// GET /clients/{cid}/observed.
    pub fn observed_view(
        &self,
        cid: &str,
        origin: Option<&str>,
        hub_status: Option<&str>,
        q: Option<&str>,
        limit: i64,
        cursor: Option<&str>,
    ) -> HubResult<Value> {
        if !self.work.names_of("clients")?.iter().any(|c| c == cid) {
            return Err(Problem::not_found(format!("no such client: {cid}")));
        }
        if let Some(o) = origin
            && !OBSERVED_ORIGINS.contains(&o)
        {
            return Err(Problem::bad_request(format!(
                "origin must be one of {}",
                OBSERVED_ORIGINS.join(", ")
            )));
        }
        if let Some(h) = hub_status
            && !["in_hub_same", "in_hub_differs", "not_in_hub"].contains(&h)
        {
            return Err(Problem::bad_request(
                "hub_status must be in_hub_same, in_hub_differs or not_in_hub",
            ));
        }
        let Some(doc) = self.observe_svc.annotated(cid) else {
            return Err(
                Problem::not_found("this client has not been observed yet; POST /clients/{id}/observe first")
                    .code("not_observed"),
            );
        };
        let mut items = arr(doc.get("items"));
        let mut summary = Map::new();
        summary.insert("total".into(), json!(items.len()));
        if let Value::Object(c) = ObserveService::count(&items) {
            summary.extend(c);
        }
        if let Some(o) = origin.filter(|o| !o.is_empty()) {
            items.retain(|i| i.get("origin").and_then(Value::as_str) == Some(o));
        }
        if let Some(h) = hub_status.filter(|h| !h.is_empty()) {
            items.retain(|i| i.get("hub_status").and_then(Value::as_str) == Some(h));
        }
        if let Some(q) = q.filter(|q| !q.is_empty()) {
            let ql = q.to_lowercase();
            items.retain(|i| {
                ["name", "path", "description"]
                    .iter()
                    .any(|k| sget(i, k).to_lowercase().contains(&ql))
            });
        }
        items.sort_by_key(|i| (sget(i, "path"), sget(i, "key")));
        let mut page = page_list(items, limit, cursor);
        if let Some(Value::Array(rows)) = page.get_mut("items") {
            for i in rows.iter_mut() {
                let ev = self.evaluation_of(&format!("{cid}:{}", sget(i, "key")), &sget(i, "content_hash"));
                if let Some(m) = i.as_object_mut() {
                    m.insert("evaluation".into(), ev);
                }
            }
        }
        let g = |k: &str| doc.get(k).cloned().unwrap_or(Value::Null);
        let mut out = Map::new();
        out.insert("client".into(), json!(cid));
        out.insert("observed_at".into(), g("observed_at"));
        out.insert("source".into(), doc.get("source").cloned().unwrap_or_else(|| json!("")));
        out.insert("complete".into(), g("complete"));
        out.insert("notes".into(), doc.get("notes").cloned().unwrap_or_else(|| json!([])));
        out.insert("summary".into(), Value::Object(summary));
        if let Value::Object(p) = page {
            out.extend(p);
        }
        Ok(Value::Object(out))
    }

    /// `_eval_sources(source)`: hub | all | one observed client.
    fn eval_sources(&self, source: &str) -> HubResult<Vec<String>> {
        if source == "hub" {
            return Ok(vec!["hub".into()]);
        }
        let observed: Vec<String> = self
            .work
            .names_of("clients")?
            .into_iter()
            .filter(|c| self.observe_svc.index(c).is_some())
            .collect();
        if source == "all" {
            let mut v = vec!["hub".to_string()];
            v.extend(observed);
            return Ok(v);
        }
        if !valid_id(source) {
            return Err(Problem::bad_request("source must be all, hub or a client id"));
        }
        Ok(if observed.iter().any(|c| c == source) {
            vec![source.to_string()]
        } else {
            Vec::new()
        })
    }

    /// `eval_rows(source)`: one row per subject of `source` (hub skills, then each observed client's skills).
    pub fn eval_rows(&self, source: &str) -> HubResult<Vec<Value>> {
        let svc = &self.skilleval;
        let run = svc.running();
        let queued = svc.queued_subjects();
        let row = |subject: String,
                   src: &str,
                   name: &str,
                   key: &str,
                   path: &str,
                   origin: &str,
                   h: Option<String>,
                   extra: Map<String, Value>| {
            let latest = svc.records(&subject, 1).into_iter().next();
            let g = h.as_deref().filter(|h| !h.is_empty()).map(|h| svc.gate(h));
            let latest_v = match &latest {
                Some(l) if truthy(Some(l)) => {
                    let mut m: Map<String, Value> = l
                        .as_object()
                        .map(|o| {
                            o.iter()
                                .filter(|(k, _)| !HEAVY.contains(&k.as_str()))
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(sev) = security_severity(l) {
                        m.insert("security_severity".into(), sev);
                    }
                    Value::Object(m)
                }
                _ => Value::Null,
            };
            let mut r = Map::new();
            r.insert("subject".into(), json!(subject));
            r.insert("source".into(), json!(src));
            r.insert("skill".into(), json!(name));
            r.insert("name".into(), json!(name));
            r.insert("key".into(), json!(key));
            r.insert("path".into(), json!(path));
            r.insert("origin".into(), json!(origin));
            r.insert("content_hash".into(), h.clone().map_or(Value::Null, Value::String));
            r.insert("state".into(), json!(svc.freshness(latest.as_ref(), h.as_deref())));
            r.insert("latest".into(), latest_v);
            r.insert("queued".into(), json!(queued.contains(&subject)));
            r.insert(
                "running".into(),
                json!(
                    run.as_ref()
                        .is_some_and(|x| x.get("subject").and_then(Value::as_str) == Some(subject.as_str()))
                ),
            );
            r.insert(
                "gate".into(),
                g.map_or(Value::Null, |g| g.get("status").cloned().unwrap_or(Value::Null)),
            );
            r.extend(extra);
            Value::Object(r)
        };
        let mut rows = Vec::new();
        for src in self.eval_sources(source)? {
            if src == "hub" {
                let mut names = self.work.names("skill")?;
                names.sort();
                for name in names {
                    let subject = format!("hub:{name}");
                    let h = svc.current_hash(&subject);
                    rows.push(row(subject, "hub", &name, &name, &name, "hub", h, Map::new()));
                }
            } else {
                let doc = self.observe_svc.annotated(&src).unwrap_or_else(|| json!({"items": []}));
                for i in arr(doc.get("items")) {
                    let mut extra = Map::new();
                    for k in ["hub_status", "hub_name", "enabled_for_client", "description"] {
                        extra.insert(k.into(), i.get(k).cloned().unwrap_or(Value::Null));
                    }
                    rows.push(row(
                        format!("{src}:{}", sget(&i, "key")),
                        &src,
                        &sget(&i, "name"),
                        &sget(&i, "key"),
                        &sget(&i, "path"),
                        &sget(&i, "origin"),
                        Some(sget(&i, "content_hash")),
                        extra,
                    ));
                }
            }
        }
        Ok(rows)
    }

    /// GET /skill-evaluations. `summary` counts the rows of `source` BEFORE the other filters (facets).
    #[allow(clippy::too_many_arguments)]
    pub fn eval_list(
        &self,
        source: &str,
        state: Option<&str>,
        verdict: Option<&str>,
        origin: Option<&str>,
        gate: Option<&str>,
        q: Option<&str>,
        limit: i64,
        cursor: Option<&str>,
    ) -> HubResult<Value> {
        if let Some(s) = state
            && !["none", "current", "stale"].contains(&s)
        {
            return Err(Problem::bad_request("state must be none, current or stale"));
        }
        if let Some(g) = gate
            && !["missing", "blocked", "warn", "ok"].contains(&g)
        {
            return Err(Problem::bad_request("gate must be missing, blocked, warn or ok"));
        }
        let mut rows = self.eval_rows(source)?;
        let verdict_of = |r: &Value| {
            r.get("latest")
                .and_then(|l| l.get("verdict"))
                .filter(|v| truthy(Some(v)))
                .map_or_else(|| "none".to_string(), pystr)
        };
        let gate_of = |r: &Value| {
            r.get("gate")
                .filter(|v| truthy(Some(v)))
                .map_or_else(|| "missing".to_string(), pystr)
        };
        let summary = json!({
            "total": rows.len(),
            "by_state": counter(rows.iter().map(|r| sget(r, "state"))),
            "by_verdict": counter(rows.iter().map(verdict_of)),
            "by_gate": counter(rows.iter().map(gate_of)),
        });
        if let Some(s) = state.filter(|s| !s.is_empty()) {
            rows.retain(|r| sget(r, "state") == s);
        }
        if let Some(v) = verdict.filter(|v| !v.is_empty()) {
            rows.retain(|r| verdict_of(r) == v);
        }
        if let Some(o) = origin.filter(|o| !o.is_empty()) {
            rows.retain(|r| sget(r, "origin") == o);
        }
        if let Some(g) = gate.filter(|g| !g.is_empty()) {
            rows.retain(|r| gate_of(r) == g);
        }
        if let Some(q) = q.filter(|q| !q.is_empty()) {
            let ql = q.to_lowercase();
            rows.retain(|r| {
                sget(r, "name").to_lowercase().contains(&ql) || sget(r, "path").to_lowercase().contains(&ql)
            });
        }
        let mut out = match page_list(rows, limit, cursor) {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        out.insert("summary".into(), summary);
        out.insert("running".into(), self.skilleval.running().unwrap_or(Value::Null));
        Ok(Value::Object(out))
    }

    /// Subjects for `select`: none_or_stale (never evaluated, or changed since) | all.
    pub fn eval_select(&self, source: &str, select: &str) -> HubResult<Vec<String>> {
        if !["none_or_stale", "all"].contains(&select) {
            return Err(Problem::bad_request("select must be none_or_stale or all"));
        }
        Ok(self
            .eval_rows(source)?
            .into_iter()
            .filter(|r| {
                truthy(r.get("content_hash"))
                    && (select == "all" || matches!(r.get("state").and_then(Value::as_str), Some("none" | "stale")))
            })
            .map(|r| sget(&r, "subject"))
            .collect())
    }

    /// POST /skill-evaluations/run: exactly one of `subjects` / `select`; a selection is capped per request.
    pub fn eval_run(
        &self,
        subjects: Option<&[String]>,
        select: Option<&str>,
        source: &str,
        tiers: &str,
        force: bool,
        actor: &str,
    ) -> HubResult<Value> {
        if subjects.is_none() == select.is_none() {
            return Err(Problem::unprocessable("send exactly one of `subjects` or `select`").code("subjects_or_select"));
        }
        let mut chosen: Vec<String> = match subjects {
            Some(s) => s.to_vec(),
            None => self.eval_select(source, select.unwrap_or("all"))?,
        };
        if chosen.len() > 5000 {
            return Err(Problem::unprocessable("at most 5000 subjects per request").code("too_many_subjects"));
        }
        let mut over = Vec::new();
        if subjects.is_none() && chosen.len() > SELECT_CAP {
            over = chosen.split_off(SELECT_CAP);
        }
        let res = self.skilleval.enqueue(&chosen, tiers, force, actor)?;
        let mut skipped = arr(res.get("skipped"));
        skipped.extend(over.into_iter().map(|s| json!({"subject": s, "reason": "request_cap"})));
        Ok(json!({"queued": res.get("queued").cloned().unwrap_or_else(|| json!([])), "skipped": skipped}))
    }

    /// GET /skill-evaluations/{subject}: the detail plus, for an observed subject, its observed fields.
    pub fn eval_detail(&self, subject: &str) -> HubResult<Value> {
        let mut out = self.skilleval.detail(subject, super::super::skilleval::KEEP_RUNS)?;
        let src = sget(&out, "source");
        let skill = out.get("skill").cloned().unwrap_or(Value::Null);
        let Some(m) = out.as_object_mut() else {
            return Ok(out);
        };
        if src == "hub" {
            m.insert("name".into(), skill.clone());
            m.insert("path".into(), skill);
            m.insert("origin".into(), json!("hub"));
        } else {
            let doc = self.observe_svc.annotated(&src).unwrap_or_else(|| json!({"items": []}));
            if let Some(ent) = arr(doc.get("items")).into_iter().find(|i| i.get("key") == Some(&skill)) {
                for k in [
                    "name",
                    "path",
                    "origin",
                    "description",
                    "hub_status",
                    "hub_name",
                    "enabled_for_client",
                ] {
                    m.insert(k.into(), ent.get(k).cloned().unwrap_or(Value::Null));
                }
            }
        }
        Ok(out)
    }
}
