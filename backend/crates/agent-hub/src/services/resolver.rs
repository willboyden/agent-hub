//! Port of services/resolver.py: one client's effective content: enabled items (profile + collections), manage flags,
//! adapter caps, floor enforcement of rules and MCP, the RenderContext the adapter renders. Pure over a Catalog: it reads
//! nothing and writes nothing. Owner: WS-B1.

use std::path::Path;
use std::sync::Arc;

use hub_core::HubResult;
use hub_core::ids::{CONCERNS, KINDS, concern_of_kind, default_manage, plural};
use hub_core::models::{ClientDoc, Finding, ProfileDoc};
use hub_core::pathsafe::{PathError, expand_root};
use hub_core::value::PyValue;
use indexmap::IndexMap;
use serde_json::{Value, json};

use super::attest::Attestor;
use super::content::{Catalog, Exc, Item, XResult, truthy};
use super::floor::Floor;
use super::targets::TargetGuard;
use crate::adapters::AdapterRegistry;
use crate::adapters::base::{
    AdapterCaps, AgentItem, ClientAdapter, ClientConfig, Diag, InstructionItem, McpItem, MemoryItem, PermissionSet,
    RenderContext, Rule, SkillItem,
};

/// concern -> AdapterCaps field
pub const CAP_OF_CONCERN: [(&str, &str); 6] = [
    ("skills", "skills"),
    ("agents", "agents"),
    ("instructions", "instructions"),
    ("mcp", "mcp"),
    ("permissions", "permissions"),
    ("memory", "memory"),
];

/// `str(Path(p))` of an absolute path: pathlib drops empty and `.` segments and a trailing slash, and keeps exactly two
/// leading slashes (POSIX) but folds three or more into one.
pub fn pathlib_str(p: &str) -> String {
    let root = if p.starts_with("//") && !p.starts_with("///") {
        "//"
    } else if p.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    let joined = parts.join("/");
    if root.is_empty() && joined.is_empty() {
        ".".into()
    } else {
        format!("{root}{joined}")
    }
}

/// `client_config(doc, adapter=None)`: roots expanded (`expand_root`), PathError for a bad root.
pub fn client_config(doc: &ClientDoc, adapter: Option<&dyn ClientAdapter>) -> Result<ClientConfig, PathError> {
    let mut roots = IndexMap::new();
    for (k, v) in &doc.roots {
        let p = expand_root(v)?;
        roots.insert(k.clone(), pathlib_str(&p.to_string_lossy()));
    }
    Ok(ClientConfig {
        id: doc.id.clone(),
        adapter: doc.adapter.clone(),
        display_name: doc.display_name.clone(),
        description: doc.description.clone(),
        roots,
        params: doc.params.clone(),
        spec: doc.spec.clone(),
        strict: doc.strict,
        manage: manage_flags(doc, adapter),
        icon: doc.icon.clone(),
        color: doc.color.clone(),
    })
}

/// `_flags(raw)`: the known concerns of a dict whose values are real bools.
fn flags(raw: Option<&PyValue>) -> Vec<(String, bool)> {
    let Some(PyValue::Dict(d)) = raw else {
        return Vec::new();
    };
    d.iter()
        .filter_map(|(k, v)| match (k.as_str(), v) {
            (Some(k), PyValue::Bool(b)) if CONCERNS.contains(&k) => Some((k.to_string(), *b)),
            _ => None,
        })
        .collect()
}

/// `manage_flags(doc, adapter=None)`: every concern -> bool (the client's typed `manage`, else its `params.manage`,
/// else the adapter's documented default, else DEFAULT_MANAGE).
pub fn manage_flags(doc: &ClientDoc, adapter: Option<&dyn ClientAdapter>) -> IndexMap<String, bool> {
    let mut out: IndexMap<String, bool> = CONCERNS
        .iter()
        .map(|c| ((*c).to_string(), default_manage(c).unwrap_or(false)))
        .collect();
    let mut layers: Vec<Vec<(String, bool)>> = Vec::new();
    if let Some(a) = adapter {
        let d = a.default_config();
        layers.push(flags(d.params.get("manage")));
        layers.push(
            d.manage
                .iter()
                .filter(|(k, _)| CONCERNS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
        );
    }
    layers.push(flags(doc.params.get("manage")));
    layers.push(
        doc.manage
            .iter()
            .filter(|(k, _)| CONCERNS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
    );
    for layer in layers {
        for (k, v) in layer {
            out.insert(k, v);
        }
    }
    out
}

/// `generic.caps_for_spec(spec)`; when it raises, Python falls back to the generic adapter's own (permissive) caps.
fn generic_caps_for_spec(spec: &PyValue) -> Option<AdapterCaps> {
    crate::adapters::generic::caps_for_spec(spec).ok()
}

/// `client_caps(adapter, doc)`: adapter caps (generic: from the spec).
pub fn client_caps(adapter: &dyn ClientAdapter, doc: &ClientDoc) -> HubResult<AdapterCaps> {
    if adapter.id() == "generic"
        && let Some(spec) = doc.spec.as_ref().filter(|s| truthy(s))
        && let Some(caps) = generic_caps_for_spec(spec)
    {
        return Ok(caps);
    }
    Ok(adapter.caps())
}

/// `cap_supported(caps, kind)`.
pub fn cap_supported(caps: &AdapterCaps, kind: &str) -> bool {
    match concern_of_kind(kind) {
        Some("skills") => caps.skills,
        Some("agents") => caps.agents,
        Some("instructions") => caps.instructions,
        Some("mcp") => caps.mcp,
        Some("permissions") => caps.permissions,
        Some("memory") => caps.memory,
        _ => false,
    }
}

#[derive(Clone)]
pub struct Resolved {
    pub client: ClientDoc,
    pub cfg: Option<ClientConfig>,
    pub adapter: Option<Arc<dyn ClientAdapter>>,
    pub manage: IndexMap<String, bool>,
    /// plural -> [{name, via}]
    pub entries: IndexMap<String, Vec<Value>>,
    pub ctx: Option<RenderContext>,
    pub findings: Vec<Finding>,
    pub diagnostics: Vec<Diag>,
    pub warnings: Vec<String>,
    /// content rules that passed the floor
    pub kept_rules: Vec<Rule>,
    /// "profile" | "content" per kept rule
    pub kept_sources: Vec<String>,
    pub allow_hosts: Vec<String>,
    pub permissions: PermissionSet,
    pub mcp: Vec<McpItem>,
}

impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("client", &self.client.id)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Resolved {
    fn eq(&self, other: &Self) -> bool {
        self.client == other.client && self.entries == other.entries && self.findings == other.findings
    }
}

/// (kind, name) -> provenance ["direct", "collection:<id>"]
pub type EnabledMap = IndexMap<(String, String), Vec<String>>;

/// `enabled_keys(profile, catalog)`: (enabled map, warnings).
pub fn enabled_keys(profile: &ProfileDoc, catalog: &Catalog) -> (EnabledMap, Vec<String>) {
    let mut via: EnabledMap = IndexMap::new();
    let mut warnings = Vec::new();
    for cid in &profile.collections {
        let Some(coll) = catalog.collections.get(cid) else {
            warnings.push(format!(
                "profile references missing collection {}",
                hub_core::py::text::repr(cid)
            ));
            continue;
        };
        for m in &coll.members {
            via.entry((m.kind.clone(), m.name.clone()))
                .or_default()
                .push(format!("collection:{cid}"));
        }
    }
    for kind in KINDS {
        for name in profile.enable.get(plural(kind).unwrap_or_default()) {
            via.entry((kind.to_string(), name.clone()))
                .or_default()
                .push("direct".into());
        }
    }
    for kind in KINDS {
        for name in profile.disable.get(plural(kind).unwrap_or_default()) {
            via.shift_remove(&(kind.to_string(), name.clone()));
        }
    }
    (via, warnings)
}

fn opt_str(v: Option<&PyValue>) -> Option<String> {
    v.and_then(PyValue::as_str).map(str::to_string)
}

fn str_vec(v: Option<&PyValue>) -> Vec<String> {
    v.and_then(PyValue::as_list)
        .map(|l| l.iter().map(PyValue::py_str).collect())
        .unwrap_or_default()
}

/// `to_mcp(item, attestor)` keeping Python's exception types (an unusable attestation key is 503).
pub fn to_mcp_x(item: &Item, attestor: Option<&Attestor>) -> XResult<McpItem> {
    let Some(m) = item.meta.as_ref() else {
        return Err(Exc::other(
            "ValidationError",
            "McpItem: name, transport: Field required",
        ));
    };
    let g = |k: &str| m.get(k);
    let mut scan_status = opt_str(g("scan_status")).unwrap_or_else(|| "unscanned".into());
    if scan_status == "clean" {
        let ok = match attestor {
            None => false,
            Some(a) => a.valid_x(
                g("scan_digest").and_then(PyValue::as_str),
                g("command").unwrap_or(&PyValue::None),
                g("args").unwrap_or(&PyValue::None),
                g("url").unwrap_or(&PyValue::None),
                g("pinned_ref").unwrap_or(&PyValue::None),
                g("transport").unwrap_or(&PyValue::None),
            )?,
        };
        if !ok {
            scan_status = "unscanned".into();
        }
    }
    Ok(McpItem {
        name: opt_str(g("name")).unwrap_or_default(),
        transport: opt_str(g("transport")).unwrap_or_default(),
        command: opt_str(g("command")),
        args: str_vec(g("args")),
        url: opt_str(g("url")),
        env_names: str_vec(g("env_names")),
        sandbox_profile: opt_str(g("sandbox_profile")),
        egress_hosts: str_vec(g("egress_hosts")),
        scan_status,
        pinned_ref: opt_str(g("pinned_ref")),
    })
}

/// `to_mcp(item, attestor)`: McpItem with `scan_status` downgraded unless the attestation digest is valid.
pub fn to_mcp(item: &Item, attestor: Option<&Attestor>) -> HubResult<McpItem> {
    to_mcp_x(item, attestor).map_err(Exc::into_problem)
}

fn diag(severity: &str, code: &str, message: String, item: Option<String>) -> Diag {
    Diag {
        severity: severity.into(),
        code: code.into(),
        message,
        item,
    }
}

/// The SkillMeta extras (`model_extra`): every key that is not `name`/`description`, in order.
fn skill_extra(item: &Item) -> PyValue {
    PyValue::Dict(
        item.meta
            .as_ref()
            .and_then(PyValue::as_dict)
            .unwrap_or_default()
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), Some("name" | "description")))
            .cloned()
            .collect(),
    )
}

pub struct Resolver {
    pub floor: Arc<Floor>,
    pub registry: AdapterRegistry,
    pub guard: Option<Arc<TargetGuard>>,
    pub attestor: Option<Arc<Attestor>>,
}

impl Resolver {
    pub fn new(
        floor: Arc<Floor>,
        registry: AdapterRegistry,
        guard: Option<Arc<TargetGuard>>,
        attestor: Option<Arc<Attestor>>,
    ) -> Self {
        Self {
            floor,
            registry,
            guard,
            attestor,
        }
    }

    pub fn resolve(&self, catalog: &Catalog, client_id: &str) -> HubResult<Resolved> {
        self.resolve_x(catalog, client_id).map_err(Exc::into_problem)
    }

    /// `resolve(catalog, client_id)` keeping Python's exception types.
    pub fn resolve_x(&self, catalog: &Catalog, client_id: &str) -> XResult<Resolved> {
        let Some(doc) = catalog.clients.get(client_id) else {
            return Err(Exc::other("KeyError", hub_core::py::text::repr(client_id)));
        };
        let adapter = self.registry.get(&doc.adapter);
        let mut res = Resolved {
            client: doc.clone(),
            cfg: None,
            adapter: adapter.clone(),
            manage: manage_flags(doc, adapter.as_deref()),
            entries: IndexMap::new(),
            ctx: None,
            findings: Vec::new(),
            diagnostics: Vec::new(),
            warnings: Vec::new(),
            kept_rules: Vec::new(),
            kept_sources: Vec::new(),
            allow_hosts: Vec::new(),
            permissions: PermissionSet::default(),
            mcp: Vec::new(),
        };
        let cfg = match client_config(doc, adapter.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                res.diagnostics
                    .push(diag("error", "bad_root", format!("client roots invalid: {e}"), None));
                return Ok(res);
            }
        };
        res.cfg = Some(cfg.clone());
        if let Some(guard) = &self.guard {
            for (rname, rpath) in &cfg.roots {
                if let Some(prob) = guard.root_problem(Path::new(rpath)) {
                    res.diagnostics.push(diag(
                        "error",
                        "bad_root",
                        format!("root {}: {prob}", hub_core::py::text::repr(rname)),
                        None,
                    ));
                    return Ok(res);
                }
            }
        }
        let Some(adapter) = adapter else {
            res.diagnostics.push(diag(
                "error",
                "adapter_missing",
                format!("adapter {} is not installed", hub_core::py::text::repr(&doc.adapter)),
                None,
            ));
            return Ok(res);
        };
        let caps = client_caps(adapter.as_ref(), doc)?;
        let profile = catalog.profiles.get(client_id).cloned().unwrap_or_default();
        let (via, warnings) = enabled_keys(&profile, catalog);
        res.warnings.extend(warnings);
        let direct = |k: &str, n: &str| plural(k).is_some_and(|pl| profile.enable.get(pl).iter().any(|x| x == n));

        let mut chosen: IndexMap<&str, Vec<&Item>> = KINDS.iter().map(|k| (*k, Vec::new())).collect();
        let mut keys: Vec<(&(String, String), &Vec<String>)> = via.iter().collect();
        keys.sort_by(|a, b| a.0.cmp(b.0));
        for ((kind, name), provenance) in keys {
            let Some(item) = catalog.items.get(&(kind.clone(), name.clone())) else {
                res.warnings.push(format!(
                    "{kind} {} is enabled but does not exist",
                    hub_core::py::text::repr(name)
                ));
                continue;
            };
            if !item.valid() {
                let msg = item
                    .issues
                    .iter()
                    .map(|i| format!("{}: {}", i.path, i.message))
                    .collect::<Vec<_>>()
                    .join("; ");
                res.diagnostics.push(diag(
                    "error",
                    "invalid_content",
                    hub_core::py::text::take_chars(&msg, 300).to_string(),
                    Some(format!("{kind}:{name}")),
                ));
                continue;
            }
            let applies = item.applies_to();
            if !applies.is_empty() && !applies.iter().any(|a| a == client_id) {
                res.warnings.push(format!(
                    "{kind} {} is not meant for this client (applies_to)",
                    hub_core::py::text::repr(name)
                ));
                continue;
            }
            let pl = plural(kind).unwrap_or_default();
            if !cap_supported(&caps, kind) {
                let is_direct = direct(kind, name);
                let sev = if doc.strict && is_direct {
                    "error"
                } else if !is_direct {
                    "info"
                } else {
                    "warn"
                };
                res.diagnostics.push(diag(
                    sev,
                    "concern_unsupported",
                    format!("{} cannot consume {pl}", adapter.id()),
                    Some(format!("{kind}:{name}")),
                ));
                continue;
            }
            if let Some(v) = chosen.get_mut(kind.as_str()) {
                v.push(item);
            }
            res.entries
                .entry(pl.to_string())
                .or_default()
                .push(json!({"name": name, "via": provenance}));
        }
        let pick = |k: &str| chosen.get(k).cloned().unwrap_or_default();

        // rules -> floor
        let content_rules: Vec<(String, Rule)> = pick("rule")
            .iter()
            .map(|i| {
                (
                    i.name.clone(),
                    Rule {
                        kind: i.meta_str("kind"),
                        matches: i.meta_str("match"),
                        decision: i.meta_str("decision"),
                        reason: opt_str(i.meta_field("reason")),
                    },
                )
            })
            .collect();
        let mut mcp_clean: IndexMap<String, bool> = IndexMap::new();
        for i in pick("mcp") {
            let m = to_mcp_x(i, self.attestor.as_deref())?;
            mcp_clean.insert(i.name.clone(), m.scan_status == "clean");
        }
        let fr = self.floor.enforce(&content_rules, Some(&mcp_clean))?;
        res.findings.extend(fr.findings.iter().cloned());
        res.permissions = fr.permissions.clone();
        res.allow_hosts = fr.allow_hosts.clone();
        let kept = fr.kept.clone();
        res.kept_rules = kept.clone();
        let prov: IndexMap<String, Vec<String>> = res
            .entries
            .get("rules")
            .map(|l| {
                l.iter()
                    .map(|e| {
                        let name = e["name"].as_str().unwrap_or_default().to_string();
                        let via: Vec<String> = e["via"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                            .unwrap_or_default();
                        (name, via)
                    })
                    .collect()
            })
            .unwrap_or_default();
        res.kept_sources = fr
            .kept_ids
            .iter()
            .map(|rid| {
                if prov.get(rid).is_some_and(|v| v.iter().any(|x| x == "direct")) {
                    "profile".to_string()
                } else {
                    "content".to_string()
                }
            })
            .collect();
        if !caps.permissions {
            res.warnings.push(format!(
                "{} cannot express permissions: the floor cannot be enforced natively",
                adapter.id()
            ));
        }

        // mcp -> floor
        let mut mcp_ok: Vec<McpItem> = Vec::new();
        for it in pick("mcp") {
            let m = to_mcp_x(it, self.attestor.as_deref())?;
            let fs = self.floor.check_mcp(&m, &kept, "mcp")?;
            if fs.is_empty() {
                mcp_ok.push(m);
            } else {
                res.findings.extend(fs);
            }
        }
        res.mcp = mcp_ok.clone();

        let mut instr = pick("instruction");
        let order_of = |i: &Item| match i.meta_field("order") {
            Some(PyValue::Int(n)) => *n,
            _ => 100,
        };
        instr.sort_by(|a, b| (order_of(a), &a.name).cmp(&(order_of(b), &b.name)));
        let clean_names: Vec<String> = mcp_ok.iter().map(|m| m.name.clone()).collect();
        let mut skills: Vec<&Item> = Vec::new();
        for i in pick("skill") {
            let fs = self.floor.check_skill(&i.name, &skill_extra(i), Some(&clean_names))?;
            let blocked = fs.iter().any(|f| f.severity == "error");
            res.findings.extend(fs);
            if !blocked {
                skills.push(i);
            }
        }
        res.ctx = Some(RenderContext {
            client: cfg,
            skills: skills
                .iter()
                .map(|i| SkillItem {
                    name: i.name.clone(),
                    description: i.meta_str("description"),
                    files: i.files.clone(),
                    groups: i.groups(),
                    tags: i.tags(),
                    source: i.source(),
                })
                .collect(),
            agents: pick("agent")
                .iter()
                .map(|i| AgentItem {
                    name: i.name.clone(),
                    description: i.meta_str("description"),
                    capabilities: str_vec(i.meta_field("capabilities")),
                    model_tier: opt_str(i.meta_field("model_tier")),
                    mode: i.meta_str("mode"),
                    read_only: matches!(i.meta_field("read_only"), Some(PyValue::Bool(true))),
                    body: i.body.clone(),
                })
                .collect(),
            instructions: instr
                .iter()
                .map(|i| InstructionItem {
                    id: i.name.clone(),
                    title: i.meta_str("title"),
                    order: order_of(i),
                    body: i.body.clone(),
                })
                .collect(),
            mcp_servers: mcp_ok,
            memory: pick("memory")
                .iter()
                .map(|i| MemoryItem {
                    id: i.name.clone(),
                    kind: i.meta_str("type"),
                    title: i.meta_str("title"),
                    body: i.body.clone(),
                })
                .collect(),
            permissions: fr.permissions,
            endpoints: catalog.endpoints.clone(),
        });
        Ok(res)
    }

    /// Why an item does not render for the client (None = it is not blocked). Errors of the floor are reported as
    /// None here; the WS-B1 callers use [`Resolver::block_reason_x`], which propagates them like Python.
    pub fn block_reason(&self, catalog: &Catalog, resolved: &Resolved, kind: &str, name: &str) -> Option<String> {
        self.block_reason_x(catalog, resolved, kind, name).ok().flatten()
    }

    /// `block_reason(catalog, resolved, kind, name)`.
    pub fn block_reason_x(
        &self,
        catalog: &Catalog,
        resolved: &Resolved,
        kind: &str,
        name: &str,
    ) -> XResult<Option<String>> {
        let Some(item) = catalog.items.get(&(kind.to_string(), name.to_string())) else {
            return Ok(Some("item does not exist".into()));
        };
        if !item.valid() {
            let msg = item
                .issues
                .iter()
                .map(|i| i.message.clone())
                .collect::<Vec<_>>()
                .join("; ");
            return Ok(Some(format!(
                "content is invalid: {}",
                hub_core::py::text::take_chars(&msg, 200)
            )));
        }
        let applies = item.applies_to();
        if !applies.is_empty() && !applies.contains(&resolved.client.id) {
            return Ok(Some(format!("applies_to limits it to {}", applies.join(", "))));
        }
        if kind == "mcp" {
            let m = to_mcp_x(item, self.attestor.as_deref())?;
            let fs = self.floor.check_mcp(&m, &resolved.kept_rules, "mcp")?;
            if let Some(f) = fs.first() {
                return Ok(Some(f.message.clone()));
            }
        }
        if kind == "rule" && item.meta.is_some() {
            let r = Rule {
                kind: item.meta_str("kind"),
                matches: item.meta_str("match"),
                decision: item.meta_str("decision"),
                reason: None,
            };
            if let Some(f) = self.floor.check_rule(&r, "permissions", Some(name))?
                && f.severity == "error"
            {
                return Ok(Some(f.message));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pathlib_normalises_like_python() {
        // python3 -c "from pathlib import Path; print(str(Path(p)))"
        for (p, want) in [
            ("/a//b/./c/", "/a/b/c"),
            ("/", "/"),
            ("//a/b", "//a/b"),
            ("///a", "/a"),
            ("/a/../b", "/a/../b"),
        ] {
            assert_eq!(pathlib_str(p), want, "{p}");
        }
    }
}
