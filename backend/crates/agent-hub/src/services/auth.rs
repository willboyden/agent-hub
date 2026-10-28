//! services/auth.py: API keys and roles (admin / viewer / client). Keys are `hc_` + 256-bit random; only the SHA-256
//! hex digest is stored (unsalted: there is no dictionary to attack) and compared in constant time. The first start
//! writes a bootstrap admin key file (0600). Security control S-AUTH. Owner: CORE (LOCKED).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hub_core::common::{new_id, now, sha256, token_urlsafe, write_secret_file};
use hub_core::ids::valid_id;
use hub_core::py::{json as pyjson, text};
use hub_core::{HubResult, Problem};
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value};
use subtle::ConstantTimeEq;

use super::store::{Row, Store};

pub const BOOTSTRAP_KEY_FILE: &str = "bootstrap-admin.key";
pub const ROLES: [&str; 3] = ["admin", "viewer", "client"];
pub const SCOPES: [&str; 1] = ["inbox:write"];

/// Who is calling. `name` is the key's name (or "public"/"loopback"), `key_id` the key row id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub name: String,
    pub role: String,
    pub key_id: Option<String>,
    pub client: Option<String>,
    pub scopes: Vec<String>,
}

impl Principal {
    pub fn public() -> Self {
        Self {
            name: "public".into(),
            role: "public".into(),
            key_id: None,
            client: None,
            scopes: Vec::new(),
        }
    }
    pub fn loopback() -> Self {
        Self {
            name: "loopback".into(),
            role: "admin".into(),
            key_id: None,
            client: None,
            scopes: Vec::new(),
        }
    }
}

fn hash(secret: &str) -> String {
    sha256(secret.as_bytes())
}

fn sql_err(_: rusqlite::Error) -> Problem {
    Problem::internal()
}

pub struct AuthService {
    db: Arc<Store>,
}

impl AuthService {
    pub fn new(db: Arc<Store>) -> Self {
        Self { db }
    }

    fn view(r: &Row) -> Value {
        let mut m = Map::new();
        for k in ["id", "name", "role", "prefix", "client"] {
            m.insert(k.into(), r.get(k).cloned().unwrap_or(Value::Null));
        }
        let scopes = r
            .get("scopes")
            .and_then(Value::as_str)
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Array(vec![]));
        m.insert("scopes".into(), scopes);
        m.insert("created_at".into(), r.get("created_at").cloned().unwrap_or(Value::Null));
        m.insert(
            "last_used_at".into(),
            r.get("last_used_at").cloned().unwrap_or(Value::Null),
        );
        Value::Object(m)
    }

    /// `create_key(name, role, client, scopes)`: the view plus `secret` (the only time it is ever returned).
    pub fn create_key(&self, name: &str, role: &str, client: Option<&str>, scopes: &[String]) -> HubResult<Value> {
        if !ROLES.contains(&role) {
            return Err(Problem::bad_request("role must be admin, viewer or client"));
        }
        let bad: Vec<Value> = scopes
            .iter()
            .filter(|s| !SCOPES.contains(&s.as_str()))
            .map(|s| Value::String(text::take_chars(s, 40).to_string()))
            .collect();
        if !bad.is_empty() {
            let bad_repr = format!(
                "[{}]",
                bad.iter()
                    .filter_map(Value::as_str)
                    .map(text::repr)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Err(Problem::unprocessable(format!(
                "unknown scope(s) {bad_repr}: the only hub token scope is inbox:write (knowledge access uses tokens issued by the knowledge service)"
            ))
            .code("unknown_scope"));
        }
        if role == "client" && !client.is_some_and(|c| !c.is_empty() && valid_id(c)) {
            return Err(Problem::bad_request("a client token needs a valid `client` id"));
        }
        if role != "client" && (client.is_some_and(|c| !c.is_empty()) || !scopes.is_empty()) {
            return Err(Problem::bad_request("client/scopes only apply to role=client"));
        }
        if text::strip(name).is_empty() || text::char_len(name) > 100 {
            return Err(Problem::bad_request("name must be 1..100 characters"));
        }
        let secret = format!("hc_{}", token_urlsafe(32));
        let kid = new_id("key_");
        let scopes_json = pyjson::dumps(
            &Value::Array(scopes.iter().cloned().map(Value::String).collect()),
            &pyjson::Opts::default(),
        );
        self.db
            .execute(
                "INSERT INTO api_keys(id,name,role,prefix,hash,client,scopes,created_at) VALUES(?,?,?,?,?,?,?,?)",
                &[
                    SqlValue::Text(kid.clone()),
                    SqlValue::Text(text::strip(name).to_string()),
                    SqlValue::Text(role.into()),
                    SqlValue::Text(secret.chars().take(8).collect()),
                    SqlValue::Text(hash(&secret)),
                    client.map_or(SqlValue::Null, |c| SqlValue::Text(c.into())),
                    SqlValue::Text(scopes_json),
                    SqlValue::Real(now()),
                ],
            )
            .map_err(sql_err)?;
        let row = self
            .db
            .one("SELECT * FROM api_keys WHERE id=?", &[SqlValue::Text(kid)])
            .map_err(sql_err)?
            .unwrap_or_default();
        let mut v = Self::view(&row);
        if let Value::Object(m) = &mut v {
            m.insert("secret".into(), Value::String(secret));
        }
        Ok(v)
    }

    /// `list_keys()`: unrevoked keys, oldest first.
    pub fn list_keys(&self) -> HubResult<Vec<Value>> {
        let rows = self
            .db
            .all(
                "SELECT * FROM api_keys WHERE revoked_at IS NULL ORDER BY created_at",
                &[],
            )
            .map_err(sql_err)?;
        Ok(rows.iter().map(Self::view).collect())
    }

    /// `revoke(key_id)`.
    pub fn revoke(&self, key_id: &str) -> HubResult<()> {
        let n = self
            .db
            .execute(
                "UPDATE api_keys SET revoked_at=? WHERE id=? AND revoked_at IS NULL",
                &[SqlValue::Real(now()), SqlValue::Text(key_id.into())],
            )
            .map_err(sql_err)?;
        if n == 0 {
            return Err(Problem::not_found(format!("no such key: {key_id}")));
        }
        Ok(())
    }

    /// `authenticate(secret)`: the principal of an unrevoked key (and `last_used_at` updated), else None.
    pub fn authenticate(&self, secret: &str) -> Option<Principal> {
        let h = hash(secret);
        let row = self
            .db
            .one(
                "SELECT * FROM api_keys WHERE hash=? AND revoked_at IS NULL",
                &[SqlValue::Text(h.clone())],
            )
            .ok()??;
        let stored = row.get("hash").and_then(Value::as_str).unwrap_or_default();
        if !bool::from(stored.as_bytes().ct_eq(h.as_bytes())) {
            return None;
        }
        let _ = self.db.execute(
            "UPDATE api_keys SET last_used_at=? WHERE hash=?",
            &[SqlValue::Real(now()), SqlValue::Text(h)],
        );
        let s = |k: &str| row.get(k).and_then(Value::as_str).map(str::to_string);
        let scopes: Vec<String> = s("scopes")
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        Some(Principal {
            name: s("name").unwrap_or_default(),
            role: s("role").unwrap_or_default(),
            key_id: s("id"),
            client: s("client"),
            scopes,
        })
    }

    /// `ensure_bootstrap_key(data_dir)`: on first start (no key rows at all) mint an admin key and write it 0600.
    pub fn ensure_bootstrap_key(&self, data_dir: &Path) -> HubResult<Option<PathBuf>> {
        if self
            .db
            .one("SELECT 1 FROM api_keys LIMIT 1", &[])
            .map_err(sql_err)?
            .is_some()
        {
            return Ok(None);
        }
        let info = self.create_key("bootstrap-admin", "admin", None, &[])?;
        let path = data_dir.join(BOOTSTRAP_KEY_FILE);
        let secret = info.get("secret").and_then(Value::as_str).unwrap_or_default();
        write_secret_file(&path, &format!("{secret}\n")).map_err(|_| Problem::internal())?;
        Ok(Some(path))
    }
}
