//! services/hub.py methods that judge configs against the policy floor and report host exposure: policy_check,
//! audit_client, doctor. Owner: WS-F.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use hub_core::models::{Finding, issues_from, mcp_doc, rule_doc};
use hub_core::value::PyValue;
use hub_core::{HubResult, Problem, pyd};
use indexmap::IndexMap;
use serde_json::{Value, json};

use super::Hub;
use crate::adapters::base::{McpItem, Rule};
use crate::config::app_root;
use crate::services::doctor::{DoctorContext, DoctorFs, RealDoctorFs, run_doctor};
use crate::services::fs::discovery_fs;
use crate::services::resolver::{manage_flags, to_mcp};

/// Python truthiness (`x or []`).
fn truthy(v: &PyValue) -> bool {
    match v {
        PyValue::None => false,
        PyValue::Bool(b) => *b,
        PyValue::Int(i) => *i != 0,
        PyValue::Float(f) => *f != 0.0,
        PyValue::Str(s) | PyValue::BigInt(s) => !s.is_empty(),
        PyValue::Bytes(b) => !b.is_empty(),
        PyValue::List(l) | PyValue::Set(l) => !l.is_empty(),
        PyValue::Dict(d) => !d.is_empty(),
        PyValue::Date { .. } | PyValue::DateTime(_) => true,
    }
}

/// `enumerate(body.get(key, []) or [])`: the elements Python iterates. A str iterates its characters and a dict its
/// keys; anything else that is truthy is not iterable (`TypeError`, an unexpected error = 500).
fn iterate(v: Option<&PyValue>) -> HubResult<Vec<PyValue>> {
    match v {
        None => Ok(Vec::new()),
        Some(v) if !truthy(v) => Ok(Vec::new()),
        Some(PyValue::List(l) | PyValue::Set(l)) => Ok(l.clone()),
        Some(PyValue::Str(s)) => Ok(s.chars().map(|c| PyValue::Str(c.to_string())).collect()),
        Some(PyValue::Dict(d)) => Ok(d.iter().map(|(k, _)| k.clone()).collect()),
        Some(_) => Err(Problem::internal()),
    }
}

fn unprocessable_with(detail: &str, err: &pyd::ValidationError, path: &str) -> Problem {
    let issues = issues_from(err, path);
    Problem::unprocessable(detail).with("errors", serde_json::to_value(issues).unwrap_or(Value::Null))
}

/// `McpItem.model_validate(mdoc.model_dump(include=set(McpItem.model_fields)))`.
fn mcp_item_from(v: &PyValue) -> McpItem {
    let s = |k: &str| v.get(k).and_then(PyValue::as_str).map(str::to_string);
    let l = |k: &str| {
        v.get(k)
            .and_then(PyValue::as_list)
            .map(|l| l.iter().map(PyValue::py_str).collect())
            .unwrap_or_default()
    };
    McpItem {
        name: s("name").unwrap_or_default(),
        transport: s("transport").unwrap_or_default(),
        command: s("command"),
        args: l("args"),
        url: s("url"),
        env_names: l("env_names"),
        sandbox_profile: s("sandbox_profile"),
        egress_hosts: l("egress_hosts"),
        scan_status: s("scan_status").unwrap_or_else(|| "unscanned".into()),
        pinned_ref: s("pinned_ref"),
    }
}

fn findings_json(findings: &[Finding]) -> Value {
    Value::Array(
        findings
            .iter()
            .map(|f| serde_json::to_value(f).unwrap_or(Value::Null))
            .collect(),
    )
}

impl Hub {
    /// POST /policy/check (`{"rules": [...], "mcp": [...]}` dry run against the floor).
    pub fn policy_check(&self, body: &IndexMap<String, PyValue>) -> HubResult<Value> {
        let mut unknown: Vec<String> = body.keys().filter(|k| *k != "rules" && *k != "mcp").cloned().collect();
        if !unknown.is_empty() {
            unknown.sort();
            return Err(Problem::unprocessable(format!(
                "unknown keys: {}",
                crate::services::floor::list_repr(&unknown)
            )));
        }
        let mut findings: Vec<Finding> = Vec::new();
        let mut rules: Vec<Rule> = Vec::new();
        for (i, r) in iterate(body.get("rules"))?.iter().enumerate() {
            // {"id": f"check-{i}", "title": "check", **r}: r must be a mapping (TypeError otherwise)
            let PyValue::Dict(pairs) = r else {
                return Err(Problem::internal());
            };
            let mut doc: Vec<(PyValue, PyValue)> = vec![
                (PyValue::str("id"), PyValue::str(format!("check-{i}"))),
                (PyValue::str("title"), PyValue::str("check")),
            ];
            for (k, v) in pairs {
                match doc.iter_mut().find(|(dk, _)| dk == k) {
                    Some(slot) => slot.1 = v.clone(),
                    None => doc.push((k.clone(), v.clone())),
                }
            }
            let v = pyd::validate(rule_doc(), &PyValue::Dict(doc))
                .map_err(|e| unprocessable_with("bad rule", &e, &format!("rules.{i}")))?;
            let rule = Rule {
                kind: v.get("kind").map(PyValue::py_str).unwrap_or_default(),
                matches: v.get("match").map(PyValue::py_str).unwrap_or_default(),
                decision: v.get("decision").map(PyValue::py_str).unwrap_or_default(),
                reason: None,
            };
            let id = v.get("id").map(PyValue::py_str).unwrap_or_default();
            if let Some(f) = self.floor.check_rule(&rule, "permissions", Some(&id))? {
                findings.push(f);
            }
            rules.push(rule);
        }
        for (i, m) in iterate(body.get("mcp"))?.iter().enumerate() {
            let v = pyd::validate(mcp_doc(), m).map_err(|e| unprocessable_with("bad mcp", &e, &format!("mcp.{i}")))?;
            findings.extend(self.floor.check_mcp(&mcp_item_from(&v), &rules, "mcp")?);
        }
        Ok(json!({
            "ok": !findings.iter().any(|f| f.severity == "error"),
            "violations": findings_json(&findings),
        }))
    }

    /// GET /clients/{cid}/audit: the live client config against the floor (advisory concerns included).
    pub fn audit_client(&self, cid: &str) -> HubResult<Value> {
        let (_, cat) = self.approved_client(cid)?; // A-6: the shared approval helper (delivery_ops.rs)
        let res = self.resolver.resolve(&cat, cid)?;
        let (Some(adapter), Some(cfg)) = (res.adapter.as_ref(), res.cfg.as_ref()) else {
            return Err(Problem::unavailable("adapter not installed").code("adapter_missing"));
        };
        let extra = self.cfg().discover_extra_roots;
        let dc = adapter
            .discover(cfg, &discovery_fs(&cfg.roots, &cfg.params, &extra))
            .map_err(|e| {
                Problem::unavailable(format!("adapter discover failed ({})", e.py_type)).code("discover_failed")
            })?;
        let mut findings: Vec<Finding> = Vec::new();
        if let Some(ps) = &dc.permissions {
            findings.extend(self.floor.check_permission_set(ps, "permissions")?);
            findings.extend(self.floor.missing_denies(ps)?);
        } else {
            findings.push(Finding {
                code: "permissions_unreadable".into(),
                severity: "info".into(),
                message: "the adapter reports no permissions for this client".into(),
                item: None,
                concern: Some("permissions".into()),
                rule: None,
            });
        }
        for m in &dc.mcp_servers {
            let hub_item = cat
                .items
                .get(&("mcp".to_string(), m.name.clone()))
                .filter(|i| i.valid());
            let Some(hub_item) = hub_item else {
                findings.push(Finding {
                    code: "mcp_not_in_hub".into(),
                    severity: "error".into(),
                    message: format!(
                        "live MCP server {} is not a scanned item in the hub catalog",
                        hub_core::py::repr(&m.name)
                    ),
                    item: Some(m.name.clone()),
                    concern: Some("mcp".into()),
                    rule: None,
                });
                continue;
            };
            let mut live = to_mcp(hub_item, Some(self.attestor.as_ref()))?;
            let hosts: BTreeSet<String> = live.egress_hosts.iter().chain(&m.egress_hosts).cloned().collect();
            live.egress_hosts = hosts.into_iter().collect();
            findings.extend(self.floor.check_mcp(&live, &res.kept_rules, "mcp")?);
        }
        Ok(json!({
            "client": cid,
            "ok": !findings.iter().any(|f| f.severity == "error"),
            "findings": findings_json(&findings),
            "manage": res.manage,
        }))
    }

    /// GET /doctor (`knowledge` = the knowledge summary, fetched by the caller first). Read-only; never secrets.
    pub fn doctor(&self, knowledge: Option<&Value>) -> HubResult<Value> {
        self.doctor_with(knowledge, None, None)
    }

    /// `doctor(knowledge, fs=None, home=None)`: everything host-facing is injectable for tests.
    pub fn doctor_with(
        &self,
        knowledge: Option<&Value>,
        fs: Option<&dyn DoctorFs>,
        home: Option<&Path>,
    ) -> HubResult<Value> {
        let cfg = self.cfg();
        let mut clients: Vec<Value> = Vec::new();
        // an unreadable content dir is itself reported by the content-dir check
        if let Ok(cat) = self.work.catalog() {
            for (cid, doc) in &cat.clients {
                let adapter = self.registry.get(&doc.adapter);
                clients.push(json!({
                    "id": cid,
                    "adapter": doc.adapter,
                    "roots": doc.roots,
                    "params": doc.params.to_json(),
                    "manage": manage_flags(doc, adapter.as_deref()),
                }));
            }
        }
        let extra = cfg.discover_extra_roots.clone();
        let gaps = |cid: &str| -> Option<Vec<String>> {
            let cat = self.work.catalog().ok()?;
            let res = self.resolver.resolve(&cat, cid).ok()?;
            let (adapter, rcfg) = (res.adapter.as_ref()?, res.cfg.as_ref()?);
            let dc = adapter
                .discover(rcfg, &discovery_fs(&rcfg.roots, &rcfg.params, &extra))
                .ok()?;
            let ps = dc.permissions.as_ref()?;
            Some(
                self.floor
                    .missing_denies(ps)
                    .ok()?
                    .iter()
                    .map(|f| format!("deny {}", f.rule.clone().unwrap_or_default().replacen(':', " ", 1)))
                    .collect(),
            )
        };
        // PATH lookup only, never a subprocess; an optional tool must not break the report
        let evaluator = self.skilleval.doctor_info().ok();
        let env: HashMap<String, String> = std::env::vars_os()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
            .collect();
        let real = RealDoctorFs;
        let ctx = DoctorContext {
            cfg: &cfg,
            fs: fs.unwrap_or(&real),
            env: &env,
            home: home.map_or_else(crate::config::home, Path::to_path_buf),
            app_root: app_root(),
            uid: nix::unistd::getuid().as_raw(),
            clients,
            knowledge: knowledge.cloned(),
            permission_gaps: &gaps,
            floor_read: self.floor.p.deny_path_read.clone(),
            floor_write: self.floor.p.deny_path_write.clone(),
            skill_evaluator: evaluator,
        };
        run_doctor(&ctx)
    }
}
