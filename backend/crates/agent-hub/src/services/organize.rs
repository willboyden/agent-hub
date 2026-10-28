//! Port of services/organize.py: collections (multi-membership, ordering, bulk ops), profile toggles and the matrix
//! (items grouped by collection x clients). Everything here edits files in the working tree only; nothing reaches a
//! client. Owner: WS-B1.

use std::collections::HashSet;
use std::sync::Arc;

use hub_core::ids::{KINDS, plural, valid_id};
use hub_core::models::{CollectionDoc, Member, ProfileDoc};
use hub_core::value::PyValue;
use hub_core::{HubResult, Problem};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::content::{
    Catalog, ContentStore, Exc, Item, XResult, collection_value, default_sidecar, dict_items, dset, dump_model,
    model_of, profile_value, py_list, selection_list, truthy,
};
use super::resolver::{Resolved, Resolver, cap_supported, client_caps, enabled_keys};
use crate::adapters::AdapterRegistry;

pub struct Organizer {
    pub work: Arc<ContentStore>,
    pub resolver: Arc<Resolver>,
    pub registry: AdapterRegistry,
}

fn repr(s: &str) -> String {
    hub_core::py::text::repr(s)
}

impl Organizer {
    pub fn new(work: Arc<ContentStore>, resolver: Arc<Resolver>, registry: AdapterRegistry) -> Self {
        Self {
            work,
            resolver,
            registry,
        }
    }

    // -- collections -------------------------------------------------------------------------------------------
    fn require_item(&self, kind: &str, name: &str) -> XResult<()> {
        if !KINDS.contains(&kind) {
            return Err(Problem::bad_request(format!("unknown kind: {kind}")).into());
        }
        if !valid_id(name) || !self.work.names_x(kind)?.iter().any(|n| n == name) {
            return Err(Problem::not_found(format!("no such {kind}: {name}")).into());
        }
        Ok(())
    }

    /// `for m in payload.get("members", []) or []: if isinstance(m, dict): self._require_item(...)`.
    fn require_members(&self, payload: &PyValue) -> XResult<()> {
        let members = payload.get("members").cloned().unwrap_or(PyValue::List(Vec::new()));
        if !truthy(&members) {
            return Ok(());
        }
        for m in py_list(&members)? {
            if m.as_dict().is_some() {
                let s = |k: &str| m.get(k).map_or_else(|| "None".to_string(), PyValue::py_str);
                self.require_item(&s("kind"), &s("name"))?;
            }
        }
        Ok(())
    }

    pub fn create_collection(&self, payload: &PyValue) -> HubResult<CollectionDoc> {
        self.create_collection_x(payload).map_err(Exc::into_problem)
    }

    pub fn create_collection_x(&self, payload: &PyValue) -> XResult<CollectionDoc> {
        let cid = match payload.get("id") {
            Some(PyValue::Str(s)) if valid_id(s) => s.clone(),
            _ => {
                return Err(Problem::bad_request("`id` must match ^[a-z0-9][a-z0-9._-]{0,63}$")
                    .code("invalid_id")
                    .into());
            }
        };
        if self.work.names_of_x("collections")?.contains(&cid) {
            return Err(Problem::conflict("collection already exists")
                .code("already_exists")
                .into());
        }
        self.require_members(payload)?;
        self.work.put_collection_x(&cid, payload)
    }

    pub fn update_collection(&self, cid: &str, payload: &PyValue) -> HubResult<CollectionDoc> {
        self.update_collection_x(cid, payload).map_err(Exc::into_problem)
    }

    pub fn update_collection_x(&self, cid: &str, payload: &PyValue) -> XResult<CollectionDoc> {
        self.work.get_collection_x(cid)?;
        self.require_members(payload)?;
        self.work.put_collection_x(cid, payload)
    }

    pub fn add_members(&self, cid: &str, members: &[Member]) -> HubResult<CollectionDoc> {
        self.add_members_x(cid, members).map_err(Exc::into_problem)
    }

    pub fn add_members_x(&self, cid: &str, members: &[Member]) -> XResult<CollectionDoc> {
        let mut coll = self.work.get_collection_x(cid)?;
        let mut have: HashSet<(String, String)> =
            coll.members.iter().map(|m| (m.kind.clone(), m.name.clone())).collect();
        for m in members {
            self.require_item(&m.kind, &m.name)?;
            if have.insert((m.kind.clone(), m.name.clone())) {
                coll.members.push(m.clone());
            }
        }
        self.work.put_collection_x(cid, &collection_value(&coll))
    }

    pub fn remove_member(&self, cid: &str, kind: &str, name: &str) -> HubResult<CollectionDoc> {
        self.remove_member_x(cid, kind, name).map_err(Exc::into_problem)
    }

    pub fn remove_member_x(&self, cid: &str, kind: &str, name: &str) -> XResult<CollectionDoc> {
        let mut coll = self.work.get_collection_x(cid)?;
        let before = coll.members.len();
        coll.members.retain(|m| !(m.kind == kind && m.name == name));
        if coll.members.len() == before {
            return Err(Problem::not_found("not a member of this collection").into());
        }
        self.work.put_collection_x(cid, &collection_value(&coll))
    }

    pub fn reorder(&self, ids: &[String]) -> HubResult<Vec<CollectionDoc>> {
        self.reorder_x(ids).map_err(Exc::into_problem)
    }

    pub fn reorder_x(&self, ids: &[String]) -> XResult<Vec<CollectionDoc>> {
        let known: HashSet<String> = self.work.names_of_x("collections")?.into_iter().collect();
        for i in ids {
            if !known.contains(i) {
                return Err(Problem::not_found(format!("no such collection: {i}")).into());
            }
        }
        if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
            return Err(Problem::bad_request("duplicate ids").into());
        }
        let mut out = Vec::new();
        for (idx, cid) in ids.iter().enumerate() {
            let mut coll = self.work.get_collection_x(cid)?;
            coll.order = i64::try_from((idx + 1) * 10).unwrap_or(i64::MAX);
            out.push(self.work.put_collection_x(cid, &collection_value(&coll))?);
        }
        Ok(out)
    }

    /// Create suggested collections in the working tree. Suggestions are recomputed here, never trusted from the client.
    pub fn accept_suggestions(&self, ids: &[String], overrides: &IndexMap<String, PyValue>) -> HubResult<Vec<Value>> {
        self.accept_suggestions_x(ids, overrides).map_err(Exc::into_problem)
    }

    pub fn accept_suggestions_x(&self, ids: &[String], overrides: &IndexMap<String, PyValue>) -> XResult<Vec<Value>> {
        if ids.is_empty() || ids.iter().collect::<HashSet<_>>().len() != ids.len() {
            return Err(Problem::bad_request("`ids` must be a non-empty list without duplicates").into());
        }
        let cat = self.work.catalog_x()?;
        let known: IndexMap<String, Value> = super::suggest::suggest(&cat)?
            .into_iter()
            .map(|s| (s["id"].as_str().unwrap_or_default().to_string(), s))
            .collect();
        for i in ids {
            if !known.contains_key(i) {
                return Err(Problem::not_found(format!("no such suggestion: {i}")).into());
            }
        }
        for (k, ov) in overrides {
            let bad_keys = ov.as_dict().is_none_or(|d| {
                d.iter()
                    .any(|(x, _)| !matches!(x.as_str(), Some("title" | "description" | "members")))
            });
            if !ids.contains(k) || bad_keys {
                return Err(Problem::bad_request(
                    "overrides may only set title, description and members for accepted ids",
                )
                .into());
            }
        }
        let mut created = Vec::new();
        for i in ids {
            let s = &known[i];
            let mut payload: Vec<(PyValue, PyValue)> = ["title", "description", "icon", "color", "members"]
                .iter()
                .map(|k| (PyValue::str(*k), PyValue::from_json(&s[*k])))
                .collect();
            payload.insert(0, (PyValue::str("id"), PyValue::str(i.as_str())));
            for (k, v) in dict_items(overrides.get(i).cloned().unwrap_or(PyValue::Dict(Vec::new()))) {
                dset(&mut payload, &k.py_str(), v);
            }
            let doc = self.create_collection_x(&PyValue::Dict(payload))?;
            created.push(serde_json::to_value(&doc).unwrap_or(Value::Null));
        }
        Ok(created)
    }

    // -- bulk --------------------------------------------------------------------------------------------------
    /// `[{"kind", "name", "ok", ...}]`
    pub fn bulk(
        &self,
        items: &[Member],
        add_to: Option<&str>,
        remove_from: Option<&str>,
        add_tags: &[String],
        remove_tags: &[String],
    ) -> HubResult<Vec<Value>> {
        let add_to = add_to.filter(|s| !s.is_empty());
        let remove_from = remove_from.filter(|s| !s.is_empty());
        if let Some(c) = add_to {
            self.work.get_collection(c)?;
        }
        if let Some(c) = remove_from {
            self.work.get_collection(c)?;
        }
        let mut results = Vec::new();
        for m in items {
            let mut detail: Vec<Value> = Vec::new();
            let r = (|| -> XResult<()> {
                self.require_item(&m.kind, &m.name)?;
                if let Some(c) = add_to {
                    self.add_members_x(c, std::slice::from_ref(m))?;
                    detail.push(Value::String(format!("added to {c}")));
                }
                if let Some(c) = remove_from {
                    match self.remove_member_x(c, &m.kind, &m.name) {
                        Ok(_) => detail.push(Value::String(format!("removed from {c}"))),
                        Err(e) if e.problem_of(&["NotFound"]).is_some() => {
                            detail.push(Value::String(format!("was not in {c}")));
                        }
                        Err(e) => return Err(e),
                    }
                }
                if !add_tags.is_empty() || !remove_tags.is_empty() {
                    self.retag(&m.kind, &m.name, add_tags, remove_tags)?;
                    detail.push(Value::String("tags updated".into()));
                }
                Ok(())
            })();
            let mut row = Map::new();
            row.insert("kind".into(), Value::String(m.kind.clone()));
            row.insert("name".into(), Value::String(m.name.clone()));
            row.insert("ok".into(), Value::Bool(r.is_ok()));
            row.insert("detail".into(), Value::Array(detail));
            if let Err(e) = r {
                let msg = match e.problem_of(&["NotFound", "BadRequest", "Conflict"]) {
                    Some(p) => p.detail.clone(),
                    None => e.type_name(),
                };
                row.insert("error".into(), Value::String(msg));
            }
            results.push(Value::Object(row));
        }
        Ok(results)
    }

    fn retag(&self, kind: &str, name: &str, add: &[String], remove: &[String]) -> XResult<()> {
        let item = self.work.load_x(kind, name)?;
        let Some(meta) = &item.meta else {
            return Err(Problem::bad_request(format!("{kind} {name} is invalid; fix it before tagging")).into());
        };
        for t in add {
            if !valid_id(t) {
                return Err(Problem::bad_request(format!("invalid tag {}", repr(t)))
                    .code("invalid_id")
                    .into());
            }
        }
        let mut tags: Vec<String> = item.tags().into_iter().filter(|t| !remove.contains(t)).collect();
        let fresh: Vec<String> = add.iter().filter(|t| !tags.contains(t)).cloned().collect();
        tags.extend(fresh);
        let tag_list = PyValue::List(tags.into_iter().map(PyValue::Str).collect());
        if kind == "skill" {
            let mut side = dict_items(item.sidecar.clone().unwrap_or_else(default_sidecar));
            dset(&mut side, "tags", tag_list);
            return self.work.set_sidecar_x(name, &PyValue::Dict(side));
        }
        let model = model_of(kind).ok_or_else(|| Problem::bad_request(format!("unknown kind: {kind}")))?;
        let mut data = dict_items(dump_model(model, meta, true));
        dset(&mut data, "tags", tag_list);
        if matches!(kind, "agent" | "instruction" | "memory") {
            dset(&mut data, "body", PyValue::Str(item.body.clone()));
        }
        self.work.put_x(kind, name, &PyValue::Dict(data), Some(true))?;
        Ok(())
    }

    // -- profiles / toggles ------------------------------------------------------------------------------------
    fn client_ids(&self) -> XResult<Vec<String>> {
        self.work.names_of_x("clients")
    }

    /// `cell_state(cat, resolved, client, kind, name)`.
    pub fn cell_state(
        &self,
        cat: &Catalog,
        resolved: &Resolved,
        client: &str,
        kind: &str,
        name: &str,
    ) -> XResult<Value> {
        let Some(adapter) = &resolved.adapter else {
            return Ok(json!({"state": "blocked",
                             "reason": format!("adapter {} is not installed", repr(&resolved.client.adapter))}));
        };
        if !cap_supported(&client_caps(adapter.as_ref(), &resolved.client)?, kind) {
            return Ok(json!({"state": "unsupported",
                             "reason": format!("{} cannot consume {}", adapter.id(), plural(kind).unwrap_or_default())}));
        }
        let profile = cat.profiles.get(client).cloned().unwrap_or_default();
        let (via, _) = enabled_keys(&profile, cat);
        let prov = via
            .get(&(kind.to_string(), name.to_string()))
            .cloned()
            .unwrap_or_default();
        if let Some(reason) = self.resolver.block_reason_x(cat, resolved, kind, name)? {
            return Ok(json!({"state": "blocked", "reason": reason, "via": prov}));
        }
        if prov.iter().any(|p| p == "direct") {
            return Ok(json!({"state": "on", "via": prov}));
        }
        if !prov.is_empty() {
            return Ok(json!({"state": "via_collection", "via": prov}));
        }
        Ok(json!({"state": "off", "via": []}))
    }

    pub fn toggle(&self, client: &str, kind: &str, name: &str, enabled: bool) -> HubResult<Value> {
        self.toggle_x(client, kind, name, enabled).map_err(Exc::into_problem)
    }

    pub fn toggle_x(&self, client: &str, kind: &str, name: &str, enabled: bool) -> XResult<Value> {
        if !self.client_ids()?.iter().any(|c| c == client) {
            return Err(Problem::not_found(format!("no such client: {client}")).into());
        }
        self.require_item(kind, name)?;
        let cat = self.work.catalog_x()?;
        let resolved = self.resolver.resolve_x(&cat, client)?;
        let cell = self.cell_state(&cat, &resolved, client, kind, name)?;
        let mut prof: ProfileDoc = cat.profiles.get(client).cloned().unwrap_or_default();
        let pl = plural(kind).unwrap_or_default();
        let mut enable: Vec<String> = prof.enable.get(pl).to_vec();
        let mut disable: Vec<String> = prof.disable.get(pl).to_vec();
        let via_coll = prof.collections.iter().any(|cid| {
            cat.collections
                .get(cid)
                .is_some_and(|c| c.members.iter().any(|m| m.kind == kind && m.name == name))
        });
        if enabled {
            let state = cell["state"].as_str().unwrap_or_default();
            if matches!(state, "unsupported" | "blocked") {
                let reason = cell.get("reason").cloned().unwrap_or(Value::Null);
                let shown = reason.as_str().unwrap_or(state).to_string();
                return Err(Problem::conflict(format!("cannot enable: {shown}"))
                    .code(&format!("cell_{state}"))
                    .with("reason", reason)
                    .into());
            }
            disable.retain(|n| n != name);
            if !via_coll && !enable.iter().any(|n| n == name) {
                enable.push(name.to_string());
            }
        } else {
            enable.retain(|n| n != name);
            if via_coll && !disable.iter().any(|n| n == name) {
                disable.push(name.to_string());
            }
        }
        *selection_list(&mut prof.enable, pl) = enable;
        *selection_list(&mut prof.disable, pl) = disable;
        self.work.put_profile_x(client, &profile_value(&prof))?;
        let cat2 = self.work.catalog_x()?;
        let resolved2 = self.resolver.resolve_x(&cat2, client)?;
        let cell2 = self.cell_state(&cat2, &resolved2, client, kind, name)?;
        let mut out = Map::new();
        out.insert("client".into(), Value::String(client.into()));
        out.insert("kind".into(), Value::String(kind.into()));
        out.insert("name".into(), Value::String(name.into()));
        if let Value::Object(c) = cell2 {
            out.extend(c);
        }
        Ok(Value::Object(out))
    }

    pub fn bulk_toggle(&self, clients: &[String], items: &[Member], enabled: bool) -> HubResult<Vec<Value>> {
        let mut out = Vec::new();
        for c in clients {
            for m in items {
                match self.toggle_x(c, &m.kind, &m.name, enabled) {
                    Ok(Value::Object(mut v)) => {
                        v.insert("ok".into(), Value::Bool(true));
                        out.push(Value::Object(v));
                    }
                    Ok(other) => out.push(other),
                    Err(e) => match e.problem_of(&["NotFound", "Conflict", "BadRequest"]) {
                        Some(p) => out.push(json!({"client": c, "kind": m.kind, "name": m.name, "ok": false,
                                                   "error": p.detail, "code": p.code})),
                        None => return Err(e.into_problem()),
                    },
                }
            }
        }
        Ok(out)
    }

    /// Import helper: mark items enabled for a client without the blocked/unsupported gate (the plan still gates).
    pub fn enable_for_client(&self, client: &str, kind: &str, names: &[String]) -> HubResult<()> {
        self.enable_for_client_x(client, kind, names).map_err(Exc::into_problem)
    }

    pub fn enable_for_client_x(&self, client: &str, kind: &str, names: &[String]) -> XResult<()> {
        let mut prof = self.work.get_profile_x(client)?;
        let pl = plural(kind).unwrap_or_default();
        let lst = selection_list(&mut prof.enable, pl);
        for n in names {
            if !lst.contains(n) {
                lst.push(n.clone());
            }
        }
        selection_list(&mut prof.disable, pl).retain(|n| !names.contains(n));
        self.work.put_profile_x(client, &profile_value(&prof))?;
        Ok(())
    }

    // -- matrix ------------------------------------------------------------------------------------------------
    pub fn matrix(&self) -> HubResult<Value> {
        self.matrix_x().map_err(Exc::into_problem)
    }

    pub fn matrix_x(&self) -> XResult<Value> {
        let cat = self.work.catalog_x()?;
        let mut clients: Vec<String> = cat.clients.keys().cloned().collect();
        clients.sort();
        let mut resolved: IndexMap<String, Resolved> = IndexMap::new();
        for c in &clients {
            resolved.insert(c.clone(), self.resolver.resolve_x(&cat, c)?);
        }
        let row = |item: &Item| -> XResult<Value> {
            let mut cells = Map::new();
            for c in &clients {
                let r = &resolved[c];
                cells.insert(c.clone(), self.cell_state(&cat, r, c, &item.kind, &item.name)?);
            }
            Ok(
                json!({"kind": item.kind, "name": item.name, "title": item.title(), "valid": item.valid(),
                      "cells": cells}),
            )
        };
        let mut groups = Vec::new();
        let mut in_any: HashSet<(String, String)> = HashSet::new();
        let mut colls: Vec<&CollectionDoc> = cat.collections.values().collect();
        colls.sort_by(|a, b| (a.order, &a.id).cmp(&(b.order, &b.id)));
        for coll in colls {
            let mut rows = Vec::new();
            for m in &coll.members {
                let key = (m.kind.clone(), m.name.clone());
                if let Some(it) = cat.items.get(&key) {
                    rows.push(row(it)?);
                    in_any.insert(key);
                }
            }
            groups.push(
                json!({"id": coll.id, "title": coll.title, "icon": coll.icon, "color": coll.color,
                               "rows": rows}),
            );
        }
        let mut keys: Vec<&(String, String)> = cat.items.keys().collect();
        keys.sort();
        let mut loose = Vec::new();
        for k in keys {
            if !in_any.contains(k) {
                loose.push(row(&cat.items[k])?);
            }
        }
        groups.push(json!({"id": null, "title": "Uncollected", "icon": "", "color": "", "rows": loose}));
        let cols: Vec<Value> = clients
            .iter()
            .map(|c| {
                let d = &cat.clients[c];
                json!({"id": c, "display_name": d.display_name, "adapter": d.adapter,
                       "adapter_installed": resolved[c].adapter.is_some()})
            })
            .collect();
        Ok(json!({"clients": cols, "groups": groups}))
    }
}
