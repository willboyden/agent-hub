//! services/store.py: SQLite (WAL, synchronous=NORMAL), file mode 0600, inline append-only migrations, one
//! connection behind a lock. The audit table rejects UPDATE/DELETE via triggers. The schema is identical, so a hub.db
//! written by Python opens unchanged. Owner: CORE (LOCKED).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
use serde_json::{Map, Value};

pub const MIGRATIONS: [(&str, &[&str]); 1] = [(
    "0001_init",
    &[
        "CREATE TABLE audit (id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL NOT NULL, actor TEXT NOT NULL, role TEXT NOT NULL, method TEXT NOT NULL, path TEXT NOT NULL, status INTEGER NOT NULL, params TEXT NOT NULL)",
        "CREATE TRIGGER audit_no_update BEFORE UPDATE ON audit BEGIN SELECT RAISE(ABORT, 'audit is append-only'); END",
        "CREATE TRIGGER audit_no_delete BEFORE DELETE ON audit BEGIN SELECT RAISE(ABORT, 'audit is append-only'); END",
        "CREATE TABLE api_keys (id TEXT PRIMARY KEY, name TEXT NOT NULL, role TEXT NOT NULL, prefix TEXT NOT NULL, hash TEXT NOT NULL UNIQUE, client TEXT, scopes TEXT NOT NULL DEFAULT '[]', created_at REAL NOT NULL, last_used_at REAL, revoked_at REAL)",
        "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    ],
)];

/// A row as Python's `sqlite3.Row` would give it: column name -> JSON value (INTEGER -> int, REAL -> float).
pub type Row = Map<String, Value>;

pub struct Store {
    conn: Mutex<Connection>,
}

fn to_json(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef as V;
    match v {
        V::Null => Value::Null,
        V::Integer(i) => Value::from(i),
        V::Real(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        V::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        V::Blob(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
    }
}

impl Store {
    /// `Store(path)`: parent dir 0700 (created), WAL, synchronous NORMAL, file chmod 0600, migrate.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let memory = path.as_os_str() == ":memory:";
        if !memory
            && let Some(parent) = path.parent()
            && !parent.exists()
        {
            let _ = std::fs::create_dir_all(parent);
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        let conn = Connection::open(path)?;
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL")?;
        if !memory {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        let s = Self { conn: Mutex::new(conn) };
        s.migrate()?;
        Ok(s)
    }

    /// `migrate()`: returns the names applied now.
    pub fn migrate(&self) -> rusqlite::Result<Vec<String>> {
        let Ok(conn) = self.conn.lock() else {
            return Ok(Vec::new());
        };
        conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_migrations (name TEXT PRIMARY KEY, applied_at REAL)")?;
        let mut done = Vec::new();
        {
            let mut st = conn.prepare("SELECT name FROM schema_migrations")?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?;
            for r in rows {
                done.push(r?);
            }
        }
        let mut applied = Vec::new();
        for (name, stmts) in MIGRATIONS {
            if done.iter().any(|d| d == name) {
                continue;
            }
            conn.execute_batch("BEGIN")?;
            let res = (|| {
                for s in stmts {
                    conn.execute_batch(s)?;
                }
                conn.execute("INSERT INTO schema_migrations VALUES (?, strftime('%s','now'))", [name])?;
                Ok::<(), rusqlite::Error>(())
            })();
            match res {
                Ok(()) => conn.execute_batch("COMMIT")?,
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(e);
                }
            }
            applied.push(name.to_string());
        }
        Ok(applied)
    }

    /// `execute(sql, params)`: returns the number of rows changed.
    pub fn execute(&self, sql: &str, params: &[SqlValue]) -> rusqlite::Result<usize> {
        let Ok(conn) = self.conn.lock() else { return Ok(0) };
        conn.execute(sql, params_from_iter(params.iter()))
    }

    /// `all(sql, params)`.
    pub fn all(&self, sql: &str, params: &[SqlValue]) -> rusqlite::Result<Vec<Row>> {
        let Ok(conn) = self.conn.lock() else {
            return Ok(Vec::new());
        };
        let mut st = conn.prepare(sql)?;
        let names: Vec<String> = st.column_names().iter().map(|s| (*s).to_string()).collect();
        let rows = st.query_map(params_from_iter(params.iter()), |r| {
            let mut m = Map::new();
            for (i, n) in names.iter().enumerate() {
                m.insert(n.clone(), to_json(r.get_ref(i)?));
            }
            Ok(m)
        })?;
        rows.collect()
    }

    /// `one(sql, params)`.
    pub fn one(&self, sql: &str, params: &[SqlValue]) -> rusqlite::Result<Option<Row>> {
        Ok(self.all(sql, params)?.into_iter().next())
    }
}

/// `paginate(limit, cursor)`: (limit clamped to 1..=500, offset = max(0, int(cursor)) or 0).
pub fn paginate(limit: i64, cursor: Option<&str>) -> (usize, usize) {
    let off = cursor
        .filter(|c| !c.is_empty())
        .and_then(hub_core::py::text::parse_int)
        .map_or(0, |o| o.max(0));
    let lim = limit.clamp(1, 500);
    (usize::try_from(lim).unwrap_or(1), usize::try_from(off).unwrap_or(0))
}

/// `page_list(items, limit, cursor)`: `{"items": chunk, "next_cursor": str(off + limit) | None}`.
pub fn page_list(items: Vec<Value>, limit: i64, cursor: Option<&str>) -> Value {
    let (lim, off) = paginate(limit, cursor);
    let n = items.len();
    let chunk: Vec<Value> = items.into_iter().skip(off).take(lim).collect();
    let mut m = Map::new();
    m.insert("items".into(), Value::Array(chunk));
    m.insert(
        "next_cursor".into(),
        if off + lim < n {
            Value::String((off + lim).to_string())
        } else {
            Value::Null
        },
    );
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paginate_like_python() {
        assert_eq!(paginate(0, None), (1, 0));
        assert_eq!(paginate(1000, Some("x")), (500, 0));
        assert_eq!(paginate(10, Some("-5")), (10, 0));
        assert_eq!(paginate(10, Some(" 3 ")), (10, 3));
    }

    #[test]
    fn audit_is_append_only_in_the_schema() -> rusqlite::Result<()> {
        let d = tempfile::tempdir().map_err(|_| rusqlite::Error::InvalidQuery)?;
        let s = Store::open(&d.path().join("hub.db"))?;
        s.execute(
            "INSERT INTO audit(ts,actor,role,method,path,status,params) VALUES(1,'a','admin','POST','/x',200,'{}')",
            &[],
        )?;
        assert!(s.execute("UPDATE audit SET actor='b'", &[]).is_err());
        assert!(s.execute("DELETE FROM audit", &[]).is_err());
        let mode = std::fs::metadata(d.path().join("hub.db"))
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0);
        assert_eq!(mode, 0o600);
        Ok(())
    }
}
