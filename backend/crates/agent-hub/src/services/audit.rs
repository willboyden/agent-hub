//! services/audit.py `AuditService`: the append-only audit trail (the DB refuses UPDATE/DELETE). `params` is stored as
//! `json.dumps(redact(params), default=str)`. Security control S-AUDIT. Owner: CORE (LOCKED).

use std::sync::Arc;

use hub_core::common::{now, redact};
use hub_core::py::json as pyjson;
use hub_core::{HubResult, Problem};
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value};

use super::store::{Store, paginate};

pub struct AuditService {
    db: Arc<Store>,
}

impl AuditService {
    pub fn new(db: Arc<Store>) -> Self {
        Self { db }
    }

    /// `record(actor=, role=, method=, path=, status=, params=)`.
    pub fn record(
        &self,
        actor: &str,
        role: &str,
        method: &str,
        path: &str,
        status: u16,
        params: &Value,
    ) -> HubResult<()> {
        let text = pyjson::dumps(&redact(params), &pyjson::Opts::default());
        self.db
            .execute(
                "INSERT INTO audit(ts,actor,role,method,path,status,params) VALUES(?,?,?,?,?,?,?)",
                &[
                    SqlValue::Real(now()),
                    SqlValue::Text(actor.into()),
                    SqlValue::Text(role.into()),
                    SqlValue::Text(method.into()),
                    SqlValue::Text(path.into()),
                    SqlValue::Integer(i64::from(status)),
                    SqlValue::Text(text),
                ],
            )
            .map(|_| ())
            .map_err(|_| Problem::internal())
    }

    /// `event(actor, what, detail)`: a domain event (method EVENT, role system, status 200).
    pub fn event(&self, actor: &str, what: &str, detail: &Value) -> HubResult<()> {
        self.record(actor, "system", "EVENT", what, 200, detail)
    }

    /// `list_page(limit, cursor)`: newest first.
    pub fn list_page(&self, limit: i64, cursor: Option<&str>) -> HubResult<Value> {
        let (lim, off) = paginate(limit, cursor);
        let rows = self
            .db
            .all(
                "SELECT * FROM audit ORDER BY id DESC LIMIT ? OFFSET ?",
                &[
                    SqlValue::Integer(i64::try_from(lim + 1).unwrap_or(501)),
                    SqlValue::Integer(i64::try_from(off).unwrap_or(0)),
                ],
            )
            .map_err(|_| Problem::internal())?;
        let more = rows.len() > lim;
        let items: Vec<Value> = rows
            .into_iter()
            .take(lim)
            .map(|r| {
                let mut m = Map::new();
                for k in ["id", "ts", "actor", "role", "method", "path", "status"] {
                    m.insert(k.into(), r.get(k).cloned().unwrap_or(Value::Null));
                }
                let params = r
                    .get("params")
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(Value::Null);
                m.insert("params".into(), params);
                Value::Object(m)
            })
            .collect();
        let mut out = Map::new();
        out.insert("items".into(), Value::Array(items));
        out.insert(
            "next_cursor".into(),
            if more {
                Value::String((off + lim).to_string())
            } else {
                Value::Null
            },
        );
        Ok(Value::Object(out))
    }
}
