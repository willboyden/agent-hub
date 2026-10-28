//! Port of services/planner.py: pure. Content catalog + live files + lock -> a per-client plan. Writes nothing (the
//! applier does). resolve -> render -> re-check rendered artifacts against the floor and target guard -> merge with the
//! live files -> per-file actions (add | change | remove | unchanged | conflict | advisory), diffs (difflib, scrubbed),
//! summary, blocked reasons and a digest that `apply` must match. Owner: WS-B2.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use hub_core::common::{scrub, sha256};
use hub_core::ids::concern_of_kind;
use hub_core::models::{ClientPlan, Finding, PlanFile};
use hub_core::py::text::{char_len, splitlines, strip, take_chars};
use hub_core::value::PyValue;
use hub_core::{HubResult, Problem};
use serde_json::{Map, Value, json};

use super::content::Catalog;
use super::difflib::unified_diff;
use super::floor::norm_path;
use super::fs::{LiveFiles, MAX_READ};
use super::lock::{Lock, LockEntry, LockStore};
use super::merge::{Desired, Merged, find_region, get_dotted, load_doc, merge_file};
use super::resolver::{Resolved, Resolver, client_caps};
use crate::adapters::AdapterRegistry;
use crate::adapters::base::{Artifact, Diag};

pub const MAX_DIFF: usize = 100_000;
/// Diagnostic codes that block apply even for a non-strict client.
pub const HARD_CODES: [&str; 10] = [
    "bad_root",
    "adapter_missing",
    "unsafe_path",
    "unknown_root",
    "duplicate_artifact",
    "merge_mode_mismatch",
    "render_failed",
    "invalid_content",
    "bad_artifact",
    "root_missing",
];

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedFile {
    pub desired: Desired,
    pub action: String,
    pub reason: String,
    pub merged: Merged,
    pub live: Option<Vec<u8>>,
    /// previous lock entry
    pub entry: Option<LockEntry>,
    pub adopted: bool,
    /// whole file after apply (None: file deleted / not written)
    pub final_bytes: Option<Vec<u8>>,
    pub delete_file: bool,
    pub adoptable: bool,
    /// concern no longer managed: drop the old lock entry, keep the file
    pub released: bool,
}

impl PlannedFile {
    /// `PlannedFile(desired, action)` with the dataclass defaults.
    pub fn new(desired: Desired, action: &str) -> Self {
        Self {
            desired,
            action: action.into(),
            reason: String::new(),
            merged: Merged::default(),
            live: None,
            entry: None,
            adopted: false,
            final_bytes: None,
            delete_file: false,
            adoptable: true,
            released: false,
        }
    }

    pub fn key(&self) -> String {
        self.desired.key()
    }
}

pub struct ClientPlanInternal {
    pub plan: ClientPlan,
    pub files: Vec<PlannedFile>,
    pub resolved: Resolved,
    pub lock: Option<Lock>,
}

/// `unified(live, new, path)`: the scrubbed, capped unified diff shown in plans.
pub fn unified(live: Option<&[u8]>, new: Option<&[u8]>, path: &str) -> String {
    let (a, b) = (live.unwrap_or_default(), new.unwrap_or_default());
    if a.contains(&0) || b.contains(&0) {
        return format!("[binary file, {} -> {} bytes]\n", a.len(), b.len());
    }
    let (ta, tb) = (String::from_utf8_lossy(a), String::from_utf8_lossy(b));
    let from = if live.is_some() {
        format!("a/{path}")
    } else {
        "/dev/null".into()
    };
    let to = if new.is_some() {
        format!("b/{path}")
    } else {
        "/dev/null".into()
    };
    let text = scrub(&unified_diff(
        &splitlines(&ta, true),
        &splitlines(&tb, true),
        &from,
        &to,
    ));
    if char_len(&text) <= MAX_DIFF {
        text
    } else {
        format!("{}\n[diff truncated]\n", take_chars(&text, MAX_DIFF))
    }
}

/// `adopt_match(adopt, client, root, path)`.
pub fn adopt_match(adopt: &[String], client: &str, root: &str, path: &str) -> bool {
    let (a, b) = (format!("{root}:{path}"), format!("{client}:{root}:{path}"));
    adopt.iter().any(|x| *x == a || *x == b)
}

fn toml_is_table(v: &toml::Value) -> bool {
    matches!(v, toml::Value::Table(_))
}

/// `toml_block_mcp_servers(content)`: server names of a rendered TOML MCP block (merge='block': `[<key_path>.<name>]`
/// tables between hub markers, as the generic adapter emits them), or why it cannot be inspected. The floor re-check
/// refuses what it cannot read: a block that does not parse, or that holds anything besides server tables under one
/// key path, is a finding, not a pass. (Parsed with the `toml` crate, PORTING.md D8.)
pub fn toml_block_mcp_servers(content: &[u8]) -> (Vec<String>, Option<String>) {
    let fail = |why: &str| (Vec::new(), Some(why.to_string()));
    let text = String::from_utf8_lossy(content);
    let region = match find_region(&text, "hash") {
        Ok(Some((a, b))) => {
            let mut r = text[a..b].to_string();
            if !r.ends_with('\n') {
                r.push('\n');
            }
            r
        }
        _ => return fail("no agent-hub marker block"),
    };
    let Ok(doc) = region.parse::<toml::Table>() else {
        return fail("not valid TOML");
    };
    let mut prefixes: BTreeSet<Vec<String>> = BTreeSet::new();
    for line in splitlines(&region, false) {
        let head = strip(line);
        if !head.starts_with('[') {
            continue;
        }
        if head.starts_with("[[") {
            return fail("an array-of-tables header");
        }
        let Ok(node) = head.parse::<toml::Table>() else {
            return fail("an unreadable table header");
        };
        let mut path: Vec<String> = Vec::new();
        let mut cur = toml::Value::Table(node);
        loop {
            match cur {
                toml::Value::Table(t) if t.len() == 1 => {
                    let Some((k, v)) = t.into_iter().next() else { break };
                    path.push(k);
                    cur = v;
                }
                _ => break,
            }
        }
        if path.len() < 2 {
            return fail("a table outside any key path");
        }
        path.pop();
        prefixes.insert(path);
    }
    if prefixes.is_empty() {
        return (
            Vec::new(),
            (!doc.is_empty()).then(|| "keys outside any server table".to_string()),
        );
    }
    if prefixes.len() > 1 {
        return fail("tables under more than one key path");
    }
    let Some(prefix) = prefixes.into_iter().next() else {
        return fail("tables under more than one key path");
    };
    let mut node = toml::Value::Table(doc);
    for key in &prefix {
        match node {
            toml::Value::Table(t) if t.len() == 1 && t.contains_key(key) => {
                let Some(next) = t.into_iter().next().map(|(_, v)| v) else {
                    return fail("content outside its server tables");
                };
                node = next;
            }
            _ => return fail("content outside its server tables"),
        }
    }
    match node {
        toml::Value::Table(t) if t.values().all(toml_is_table) => {
            let mut names: Vec<String> = t.keys().cloned().collect();
            names.sort();
            (names, None)
        }
        _ => fail("a server entry that is not a table"),
    }
}

/// Python's repr of a list of str (`['a', 'b']`).
fn repr_list(items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|s| hub_core::py::repr(s)).collect();
    format!("[{}]", parts.join(", "))
}

fn diag(code: &str, message: impl Into<String>) -> Diag {
    Diag {
        severity: "error".into(),
        code: code.into(),
        message: message.into(),
        item: None,
    }
}

/// `Finding(code="floor_violation_rendered", severity="error", concern=..., item=..., message=...)`.
fn rendered_violation(concern: &str, item: &str, msg: String) -> Finding {
    Finding {
        code: "floor_violation_rendered".into(),
        severity: "error".into(),
        message: msg,
        item: Some(item.into()),
        concern: Some(concern.into()),
        rule: None,
    }
}

pub struct Planner {
    pub resolver: Arc<Resolver>,
    pub registry: AdapterRegistry,
    pub locks: Arc<LockStore>,
    pub max_live: u64,
}

impl Planner {
    pub fn new(resolver: Arc<Resolver>, registry: AdapterRegistry, locks: Arc<LockStore>, max_live: u64) -> Self {
        Self {
            resolver,
            registry,
            locks,
            max_live,
        }
    }

    /// `plan_client(catalog, client_id, adopt=frozenset())`.
    pub fn plan_client(&self, catalog: &Catalog, client_id: &str, adopt: &[String]) -> HubResult<ClientPlanInternal> {
        let res = self.resolver.resolve(catalog, client_id)?;
        self.plan_resolved(res, client_id, adopt)
    }

    /// The part of `plan_client` after `resolver.resolve`: render, re-check, merge, summarise. (Separate so the
    /// planner can be exercised with a hand-built `Resolved`.)
    pub fn plan_resolved(&self, mut res: Resolved, client_id: &str, adopt: &[String]) -> HubResult<ClientPlanInternal> {
        let mut diags: Vec<Diag> = res.diagnostics.clone();
        let lock = self.locks.load(client_id);
        let mut desired: Vec<Desired> = Vec::new();
        if let (Some(ctx), Some(adapter), Some(_)) = (&res.ctx, &res.adapter, &res.cfg) {
            match adapter.render(ctx) {
                Err(e) => diags.push(diag("render_failed", format!("adapter raised {}", e.py_type))),
                Ok(rr) => {
                    diags.extend(rr.diagnostics.iter().cloned());
                    let (d, more) = self.desired(&res, &rr.artifacts);
                    desired = d;
                    diags.extend(more);
                    let extra = self.recheck_rendered(&res, &rr.artifacts)?;
                    res.findings.extend(extra);
                }
            }
        }
        let roots = res.cfg.as_ref().map(|c| c.roots.clone()).unwrap_or_default();
        let live = LiveFiles::new(&roots, self.max_live);
        let seen: BTreeSet<String> = desired.iter().map(Desired::key).collect();
        // removals: lock entries the hub owns that are no longer rendered (only for concerns still managed)
        if let (Some(lock), Some(_)) = (&lock, &res.cfg) {
            let mut entries: Vec<(&String, &LockEntry)> = lock.files.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for (key, e) in entries {
                let concern = concern_of_kind(&e.kind).unwrap_or("skills");
                if seen.contains(key) || !res.manage.get(concern).copied().unwrap_or(false) {
                    continue;
                }
                desired.push(Desired {
                    root: e.root.clone(),
                    path: e.path.clone(),
                    kind: e.kind.clone(),
                    merge: e.merge.clone(),
                    source_ids: e.source_ids.clone(),
                    mode: e.mode,
                    managed_keys: e.managed_keys.clone(),
                    content: None,
                    managed: true,
                });
            }
        }
        let mut groups: HashMap<String, Vec<Desired>> = HashMap::new();
        for d in desired {
            groups.entry(d.key()).or_default().push(d);
        }
        let mut keys: Vec<String> = groups.keys().cloned().collect();
        keys.sort();
        let mut files = Vec::new();
        for key in keys {
            let ds = groups.remove(&key).unwrap_or_default();
            files.push(self.plan_file(client_id, &ds, &live, lock.as_ref(), adopt)?);
        }
        let findings = Self::downgrade(&res);
        let cp = self.summarise(client_id, &files, &diags, findings, &res);
        Ok(ClientPlanInternal {
            plan: cp,
            files,
            resolved: res,
            lock,
        })
    }

    /// `_recheck_rendered`: the floor is enforced on what the adapter actually RENDERED, not just on what went in: an
    /// adapter bug (or a hostile spec) must not smuggle an unapproved MCP server or an allow entry for a floor-denied
    /// path/tool into a client config.
    fn recheck_rendered(&self, res: &Resolved, artifacts: &[Artifact]) -> HubResult<Vec<Finding>> {
        let (Some(ctx), Some(adapter)) = (&res.ctx, &res.adapter) else {
            return Ok(Vec::new());
        };
        let floor = &self.resolver.floor;
        let approved: BTreeSet<&str> = ctx.mcp_servers.iter().map(|m| m.name.as_str()).collect();
        // Python's client_caps never raises (generic falls back to the adapter's own caps on any error)
        let caps = client_caps(adapter.as_ref(), &res.client).unwrap_or_else(|_| adapter.caps());
        let native: BTreeSet<String> = ["web_fetch", "web_search", "shell"]
            .iter()
            .filter_map(|k| caps.tool_map.get(*k).cloned().flatten())
            .filter(|v| !v.is_empty())
            .collect();
        let mut stems: Vec<String> = Vec::new();
        for g in floor.p.deny_path_read.iter().chain(floor.p.deny_path_write.iter()) {
            let s = norm_path(g);
            let cut = s.find(['*', '?', '[']).unwrap_or(s.len());
            let s = s[..cut].trim_end_matches('/').to_string();
            if char_len(&s) > 3 {
                stems.push(s);
            }
        }
        let home = crate::config::home().to_string_lossy().to_string();
        let mut out: Vec<Finding> = Vec::new();
        for a in artifacts {
            if a.kind == "mcp" && a.merge == "block" {
                let (names, why) = toml_block_mcp_servers(&a.content);
                if let Some(why) = why {
                    out.push(rendered_violation(
                        "mcp",
                        take_chars(&a.path, 60),
                        format!("rendered TOML MCP block cannot be inspected ({why}); it is refused"),
                    ));
                }
                let mut extra: Vec<String> = names.into_iter().filter(|n| !approved.contains(n.as_str())).collect();
                extra.sort();
                extra.dedup();
                if let Some(first) = extra.first() {
                    out.push(rendered_violation(
                        "mcp",
                        first,
                        format!(
                            "rendered MCP server(s) {} are not in the approved (clean) set",
                            repr_list(&extra[..extra.len().min(5)])
                        ),
                    ));
                }
                continue;
            }
            let low = a.path.to_lowercase();
            if !(a.kind == "mcp" || a.kind == "rule")
                || !(low.ends_with(".json") || low.ends_with(".yaml") || low.ends_with(".yml"))
            {
                continue;
            }
            let fmt = if low.ends_with(".json") { "json" } else { "yaml" };
            let Ok(doc) = load_doc(&a.content, fmt) else {
                continue; // not parseable here: the merge step reports it
            };
            if a.kind == "mcp" {
                let keys: Vec<String> = if a.managed_keys.is_empty() {
                    let mut ks = Vec::new();
                    for (k, _) in &doc {
                        // Python: `get_dotted(doc, key)` on a non-str key raises AttributeError (unhandled)
                        ks.push(k.as_str().ok_or_else(Problem::internal)?.to_string());
                    }
                    ks
                } else {
                    a.managed_keys.clone()
                };
                for key in keys {
                    if let Some(PyValue::Dict(val)) = get_dotted(&doc, &key) {
                        let mut extra: Vec<String> = Vec::new();
                        for (k, _) in val {
                            let name = k.as_str().ok_or_else(Problem::internal)?;
                            if !approved.contains(name) && !extra.iter().any(|e| e == name) {
                                extra.push(name.to_string());
                            }
                        }
                        extra.sort();
                        if let Some(first) = extra.first() {
                            out.push(rendered_violation(
                                "mcp",
                                first,
                                format!(
                                    "rendered MCP server(s) {} are not in the approved (clean) set",
                                    repr_list(&extra[..extra.len().min(5)])
                                ),
                            ));
                        }
                    }
                }
            } else {
                walk(&PyValue::Dict(doc), "", &native, &stems, &home, &mut out);
            }
        }
        Ok(out)
    }

    /// `_desired(res, artifacts)`: validated Desired entries + diagnostics.
    fn desired(&self, res: &Resolved, artifacts: &[Artifact]) -> (Vec<Desired>, Vec<Diag>) {
        let mut out: Vec<Desired> = Vec::new();
        let mut diags: Vec<Diag> = Vec::new();
        let Some(cfg) = &res.cfg else {
            return (out, diags);
        };
        let live = LiveFiles::new(&cfg.roots, MAX_READ);
        let mut missing_roots: BTreeSet<String> = BTreeSet::new();
        let managed = |concern: &str| res.manage.get(concern).copied().unwrap_or(false);
        for a in artifacts {
            let concern = concern_of_kind(&a.kind);
            let Some(concern) =
                concern.filter(|_| ["own", "json_keys", "yaml_keys", "block"].contains(&a.merge.as_str()))
            else {
                diags.push(diag(
                    "bad_artifact",
                    format!("artifact has unknown kind/merge: {}/{}", a.kind, a.merge),
                ));
                continue;
            };
            let Some(root_dir) = cfg.roots.get(&a.root) else {
                diags.push(diag(
                    "unknown_root",
                    format!("artifact targets unconfigured root {}", hub_core::py::repr(&a.root)),
                ));
                continue;
            };
            // The hub never creates a client root (it only writes inside directories that exist), so a missing one would
            // fail at apply time. Say so in the plan instead. Advisory artifacts are never written, so they never need it.
            if managed(concern) && !Path::new(root_dir).is_dir() {
                if missing_roots.insert(a.root.clone()) {
                    diags.push(diag(
                        "root_missing",
                        format!(
                            "root {} does not exist ({root_dir}): create it first; the hub never creates a client root",
                            hub_core::py::repr(&a.root)
                        ),
                    ));
                }
                continue;
            }
            let tgt = match live.target(&a.root, &a.path) {
                Ok(t) => t,
                Err(e) => {
                    diags.push(diag(
                        "unsafe_path",
                        format!("{}:{}: {e}", a.root, hub_core::py::repr(take_chars(&a.path, 80))),
                    ));
                    continue;
                }
            };
            let guard = self.resolver.guard.as_ref();
            let mut prob = guard.and_then(|g| g.target_problem(&tgt, &a.path, res.client.adapter == "generic"));
            if prob.is_none() && managed(concern) && guard.is_some() {
                // advisory artifacts are never written, so never refused
                prob =
                    super::targets::TargetGuard::sensitive_target(&res.client.adapter, &res.manage, &a.kind, &a.path);
            }
            if let Some(p) = prob.filter(|p| !p.is_empty()) {
                diags.push(diag("unsafe_path", take_chars(&p, 200)));
                continue;
            }
            if a.mode & !0o777 != 0 {
                diags.push(diag(
                    "bad_artifact",
                    format!("{}: setuid/setgid/sticky modes are refused", a.path),
                ));
                continue;
            }
            out.push(Desired {
                root: a.root.clone(),
                path: a.path.clone(),
                kind: a.kind.clone(),
                merge: a.merge.clone(),
                source_ids: a.source_ids.clone(),
                mode: a.mode,
                managed_keys: a.managed_keys.clone(),
                content: Some(a.content.clone()),
                managed: managed(concern),
            });
        }
        // group by key in first-seen order (Python dict order)
        let mut order: Vec<String> = Vec::new();
        let mut by_key: HashMap<String, Vec<Desired>> = HashMap::new();
        for d in out {
            let k = d.key();
            if !by_key.contains_key(&k) {
                order.push(k.clone());
            }
            by_key.entry(k).or_default().push(d);
        }
        let mut clean = Vec::new();
        for key in order {
            let ds = by_key.remove(&key).unwrap_or_default();
            let modes: BTreeSet<&str> = ds.iter().map(|d| d.merge.as_str()).collect();
            if modes.len() > 1 {
                diags.push(diag(
                    "merge_mode_mismatch",
                    format!("{key}: artifacts disagree on the merge mode"),
                ));
            } else if (ds[0].merge == "own" || ds[0].merge == "block") && ds.len() > 1 {
                diags.push(diag(
                    "duplicate_artifact",
                    format!("{key}: {} artifacts for a {} file", ds.len(), ds[0].merge),
                ));
            } else {
                clean.extend(ds);
            }
        }
        (clean, diags)
    }

    fn plan_file(
        &self,
        client: &str,
        ds: &[Desired],
        live_files: &LiveFiles,
        lock: Option<&Lock>,
        adopt: &[String],
    ) -> HubResult<PlannedFile> {
        let Some(d0) = ds.first() else {
            return Err(Problem::internal());
        };
        let entry = lock.and_then(|l| l.files.get(&d0.key()).cloned());
        let mut pf = PlannedFile::new(d0.clone(), "unchanged");
        pf.entry = entry.clone();
        let adopted = adopt_match(adopt, client, &d0.root, &d0.path);
        pf.adopted = adopted;
        match live_files.read(&d0.root, &d0.path) {
            Ok(l) => pf.live = l,
            Err(e) => {
                pf.action = "conflict".into();
                pf.reason = format!("live file unreadable: {e}");
                pf.adoptable = false;
                return Ok(pf);
            }
        }
        let m = merge_file(ds, pf.live.as_deref(), adopted)?;
        pf.merged = m.clone();
        let removal = ds.iter().all(|d| d.content.is_none());
        pf.final_bytes = m.new.clone();
        let managed = if removal { true } else { ds.iter().all(|d| d.managed) };
        if !m.unparseable.is_empty() {
            pf.action = "conflict".into();
            pf.reason = m.unparseable;
            return Ok(pf);
        }
        if !managed {
            // advisory: show what would change, never write, never lock. A lock entry from when the concern WAS managed is
            // released (otherwise verify keeps checking a file the hub no longer owns); the file itself is left in place.
            pf.action = if m.slice_live != m.slice_new || (pf.live.is_none() && m.new.is_some()) {
                "advisory"
            } else {
                "unchanged"
            }
            .into();
            pf.released = entry.is_some() && !ds.iter().any(|d| d.managed);
            let mut reasons: Vec<&str> = Vec::new();
            if pf.action == "advisory" {
                reasons.push("concern is not managed by the hub");
            }
            if pf.released {
                reasons.push("released from the lock, left in place");
            }
            pf.reason = reasons.join("; ");
            return Ok(pf);
        }
        if removal {
            Self::plan_removal(&mut pf, &m, entry.as_ref());
            return Ok(pf);
        }
        if pf.live.is_none() {
            pf.action = if m.new.is_none() { "unchanged" } else { "add" }.into();
            return Ok(pf);
        }
        if m.slice_live == m.slice_new {
            pf.action = "unchanged".into();
            return Ok(pf);
        }
        if m.slice_live.is_none() {
            // merge file exists but the hub's slice is not in it yet
            pf.action = "change".into();
            return Ok(pf);
        }
        if let Some(e) = &entry
            && m.slice_live.as_deref() == Some(e.slice_sha256.as_str())
        {
            pf.action = "change".into();
            return Ok(pf);
        }
        pf.action = "conflict".into();
        pf.reason = if entry.is_some() {
            "live content differs from what the hub last applied"
        } else {
            "file exists and was not created by the hub"
        }
        .into();
        Ok(pf)
    }

    fn plan_removal(pf: &mut PlannedFile, m: &Merged, entry: Option<&LockEntry>) {
        let set = |pf: &mut PlannedFile, action: &str, reason: &str| {
            pf.action = action.into();
            pf.reason = reason.into();
        };
        if pf.live.is_none() || m.slice_live.is_none() {
            set(pf, "unchanged", "already gone");
            return;
        }
        let created = entry.is_some_and(|e| e.created);
        let slice_matches = entry.is_some_and(|e| m.slice_live.as_deref() == Some(e.slice_sha256.as_str()));
        if pf.desired.merge == "own" {
            if !created {
                set(
                    pf,
                    "unchanged",
                    "not created by the hub: released from the lock, left in place",
                );
            } else if slice_matches {
                pf.action = "remove".into();
                pf.delete_file = true;
            } else {
                set(pf, "conflict", "hub-created file was edited since it was applied");
            }
            return;
        }
        if !slice_matches {
            set(pf, "conflict", "managed slice was edited since it was applied");
            return;
        }
        if m.empty_after && created {
            pf.action = "remove".into();
            pf.delete_file = true;
        } else {
            set(
                pf,
                "change",
                "removing the hub-managed slice; the rest of the file is preserved",
            );
        }
    }

    /// `_downgrade(res)`: findings about an advisory concern are warnings (the hub is not writing there).
    fn downgrade(res: &Resolved) -> Vec<Finding> {
        res.findings
            .iter()
            .map(|f| {
                let advisory = f
                    .concern
                    .as_deref()
                    .is_some_and(|c| !c.is_empty() && !res.manage.get(c).copied().unwrap_or(false));
                if f.severity == "error" && advisory {
                    Finding {
                        severity: "warn".into(),
                        message: format!("{} (advisory concern: not written)", f.message),
                        ..f.clone()
                    }
                } else {
                    f.clone()
                }
            })
            .collect()
    }

    fn summarise(
        &self,
        client: &str,
        files: &[PlannedFile],
        diags: &[Diag],
        findings: Vec<Finding>,
        res: &Resolved,
    ) -> ClientPlan {
        let mut pfiles = Vec::new();
        let mut counts: Map<String, Value> = Map::new();
        for pf in files {
            let d = &pf.desired;
            let n = counts.get(&pf.action).and_then(Value::as_i64).unwrap_or(0);
            counts.insert(pf.action.clone(), Value::from(n + 1));
            let new = if pf.delete_file { None } else { pf.merged.new.as_deref() };
            let diff = if pf.action == "unchanged" {
                String::new()
            } else {
                unified(pf.live.as_deref(), new, &d.path)
            };
            pfiles.push(PlanFile {
                root: d.root.clone(),
                path: d.path.clone(),
                action: pf.action.clone(),
                kind: d.kind.clone(),
                source_ids: d.source_ids.clone(),
                diff,
                reason: pf.reason.clone(),
                merge: d.merge.clone(),
                managed: d.managed && pf.action != "advisory",
            });
        }
        let mut reasons: Vec<String> = Vec::new();
        let strict = res.client.strict;
        for dg in diags {
            if dg.severity == "error" && (strict || HARD_CODES.contains(&dg.code.as_str())) {
                reasons.push(take_chars(&format!("{}: {}", dg.code, dg.message), 200).to_string());
            }
        }
        for f in &findings {
            if f.severity == "error" {
                reasons.push(take_chars(&format!("{}: {}", f.code, f.message), 200).to_string());
            }
        }
        let mut rows: Vec<[String; 4]> = files
            .iter()
            .map(|pf| {
                [
                    pf.key(),
                    pf.action.clone(),
                    sha256(pf.desired.content.as_deref().unwrap_or_default()),
                    pf.merged.slice_new.clone().unwrap_or_default(),
                ]
            })
            .collect();
        rows.sort();
        let src = hub_core::py::json::dumps(&json!(rows), &hub_core::py::json::Opts::default());
        let blocked = !reasons.is_empty();
        reasons.truncate(20);
        ClientPlan {
            client: client.into(),
            files: pfiles,
            diagnostics: diags
                .iter()
                .map(|d| serde_json::to_value(d).unwrap_or(Value::Null))
                .collect(),
            floor_violations: findings,
            summary: counts,
            blocked,
            blocked_reasons: reasons,
            digest: sha256(src.as_bytes()),
        }
    }

    /// `drift(ip)`: live vs rendered vs lock, per file. Hashes are 12-char prefixes only, never contents.
    ///
    /// edited_outside: a lock entry exists and the live slice no longer matches it (someone changed or deleted it).
    /// pending_changes: the hub WOULD change something (render != live) although nobody edited the file.
    pub fn drift(&self, ip: &ClientPlanInternal) -> HubResult<Value> {
        let pre = |h: Option<&str>| -> Value {
            match h {
                Some(h) if !h.is_empty() => Value::String(take_chars(h, 12).to_string()),
                _ => Value::Null,
            }
        };
        let mut rows: Vec<Value> = Vec::new();
        let mut edited: Vec<Value> = Vec::new();
        let mut pending = 0i64;
        for pf in &ip.files {
            let d = &pf.desired;
            let state = match pf.action.as_str() {
                "advisory" => "advisory",
                "unchanged" => "in_sync",
                "add" if pf.entry.is_none() => "never_applied",
                "add" => "missing",
                "change" => "pending",
                "remove" => "orphaned",
                _ if pf.entry.is_some() => "changed_live",
                _ => "unadopted",
            };
            if state == "changed_live" || state == "missing" {
                edited.push(json!({"root": d.root, "path": d.path,
                                   "reason": if state == "changed_live" { "changed" } else { "missing" }}));
            } else if matches!(state, "pending" | "never_applied" | "orphaned" | "unadopted") {
                pending += 1;
            }
            rows.push(json!({
                "root": d.root, "path": d.path, "state": state, "kind": d.kind, "reason": pf.reason,
                "expected": pre(pf.merged.slice_new.as_deref()),
                "actual": pre(pf.merged.slice_live.as_deref()),
                "locked": pf.entry.as_ref().map_or(Value::Null, |e| pre(Some(&e.slice_sha256))),
            }));
        }
        let managed_in_sync = rows
            .iter()
            .filter(|r| r["state"] != "advisory")
            .all(|r| r["state"] == "in_sync");
        let status = if ip.plan.blocked {
            "error"
        } else if ip
            .lock
            .as_ref()
            .is_none_or(|l| l.applied_at == 0.0 && l.adopted_at == 0.0)
        {
            if managed_in_sync { "in_sync" } else { "never_applied" }
        } else if !edited.is_empty() {
            "edited_outside"
        } else if pending > 0 {
            "pending_changes"
        } else {
            "in_sync"
        };
        Ok(json!({"client": ip.plan.client, "status": status, "files": rows,
                  "drift": {"edited_outside": edited, "pending_changes": pending}}))
    }
}

/// `walk(node, key)` of `_recheck_rendered`: allow-lists of a rendered rule config must not allow a native tool the floor
/// caps at `ask`, nor touch a floor-denied path.
fn walk(node: &PyValue, key: &str, native: &BTreeSet<String>, stems: &[String], home: &str, out: &mut Vec<Finding>) {
    match node {
        PyValue::Dict(d) => {
            for (k, v) in d {
                walk(v, &k.py_str(), native, stems, home, out);
            }
        }
        PyValue::List(items) => {
            let kl = key.to_lowercase();
            if !kl.contains("allow") || kl.contains("disallow") {
                return;
            }
            for e in items {
                let Some(e) = e.as_str() else { continue };
                // compare in both the ~ and the absolute spelling
                let low = e.replace("~/", &format!("{home}/"));
                if native.contains(strip(e)) {
                    out.push(rendered_violation(
                        "permissions",
                        take_chars(e, 60),
                        format!(
                            "rendered config allows {}, which the floor caps at 'ask'",
                            hub_core::py::repr(strip(e))
                        ),
                    ));
                } else if stems.iter().any(|s| low.contains(s.as_str()) || e.contains(s.as_str())) {
                    out.push(rendered_violation(
                        "permissions",
                        take_chars(e, 60),
                        "rendered config allows an entry that touches a floor-denied path".into(),
                    ));
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type R = Result<(), Box<dyn std::error::Error>>;

    fn b64(v: &Value) -> Option<Vec<u8>> {
        use base64::Engine as _;
        v.as_str()
            .and_then(|t| base64::engine::general_purpose::STANDARD.decode(t).ok())
    }

    /// planner.unified (difflib + scrub + the 100 000-character cap) equals Python's output.
    #[test]
    fn unified_matches_python_goldens() -> R {
        let doc: Value = serde_json::from_str(include_str!("../../tests/golden/planner_unified.json"))?;
        for (n, c) in doc["cases"].as_array().ok_or("cases")?.iter().enumerate() {
            let got = unified(
                b64(&c["live"]).as_deref(),
                b64(&c["new"]).as_deref(),
                c["path"].as_str().unwrap_or_default(),
            );
            assert_eq!(got, c["out"].as_str().unwrap_or_default(), "case {n}");
        }
        Ok(())
    }

    #[test]
    fn adopt_matches_plain_and_client_scoped() {
        let a = vec!["home:x".to_string(), "c2:home:y".to_string()];
        assert!(adopt_match(&a, "c1", "home", "x"));
        assert!(!adopt_match(&a, "c1", "home", "y"));
        assert!(adopt_match(&a, "c2", "home", "y"));
    }

    #[test]
    fn toml_block_inspection() {
        let ok = b"# agent-hub:begin (managed)\n[mcp_servers.b]\ncommand = \"x\"\n\n[mcp_servers.a]\nargs = []\n# agent-hub:end\n";
        assert_eq!(
            toml_block_mcp_servers(ok),
            (vec!["a".to_string(), "b".to_string()], None)
        );
        let cases: [(&[u8], &str); 7] = [
            (b"[mcp_servers.a]\n", "no agent-hub marker block"),
            (b"# agent-hub:begin\n[a\n# agent-hub:end\n", "not valid TOML"),
            (
                b"# agent-hub:begin\n[[a.b]]\n# agent-hub:end\n",
                "an array-of-tables header",
            ),
            (
                b"# agent-hub:begin\n[a]\nx = 1\n# agent-hub:end\n",
                "a table outside any key path",
            ),
            (
                b"# agent-hub:begin\n[a.x]\n[b.y]\n# agent-hub:end\n",
                "tables under more than one key path",
            ),
            (
                b"# agent-hub:begin\nk = 1\n[a.x]\n# agent-hub:end\n",
                "content outside its server tables",
            ),
            (
                b"# agent-hub:begin\nk = 1\n# agent-hub:end\n",
                "keys outside any server table",
            ),
        ];
        for (c, why) in cases {
            assert_eq!(
                toml_block_mcp_servers(c).1.as_deref(),
                Some(why),
                "{}",
                String::from_utf8_lossy(c)
            );
        }
        assert_eq!(
            toml_block_mcp_servers(b"# agent-hub:begin\n# agent-hub:end\n"),
            (Vec::new(), None)
        );
        assert_eq!(
            toml_block_mcp_servers(b"# agent-hub:begin\n[a.x]\n[a.y]\nq = 1\n# agent-hub:end\n").1,
            None
        );
        assert_eq!(
            toml_block_mcp_servers(b"# agent-hub:begin\n[a]\nx.y = 1\n[a.x]\n# agent-hub:end\n")
                .1
                .as_deref(),
            Some("not valid TOML")
        );
    }
}
