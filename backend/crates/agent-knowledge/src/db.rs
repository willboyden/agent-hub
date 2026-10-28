//! Port of db.py: the small SQLite registry (namespaces, hashed tokens, daily usage, shared-index registry) and the
//! bootstrap admin key. The DB and the key live under `data_dir` (0700) with mode 0600. Same DDL text as Python (SQLite
//! keeps it verbatim in `sqlite_master`), autocommit, so a `knowledge.db` either implementation wrote opens in the other.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use hub_core::PyValue;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension, params};

use crate::pyjson::{self, Style, obj};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS namespaces(name TEXT PRIMARY KEY, backend TEXT NOT NULL, embedding_model TEXT NOT NULL,
  dim INTEGER NOT NULL, description TEXT NOT NULL DEFAULT '', created REAL NOT NULL);
CREATE TABLE IF NOT EXISTS tokens(id TEXT PRIMARY KEY, client TEXT NOT NULL, hash TEXT NOT NULL UNIQUE,
  scopes TEXT NOT NULL, rate_per_min INTEGER NOT NULL, daily_queries INTEGER NOT NULL, daily_writes INTEGER NOT NULL,
  created REAL NOT NULL);
CREATE TABLE IF NOT EXISTS usage(token_id TEXT NOT NULL, day TEXT NOT NULL, queries INTEGER NOT NULL DEFAULT 0,
  writes INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(token_id, day));
CREATE TABLE IF NOT EXISTS indexes(name TEXT PRIMARY KEY, kind TEXT NOT NULL, path TEXT NOT NULL, project TEXT NOT NULL);
";

/// `hash_secret(secret)`: sha256 hex of the UTF-8 text (secrets are 256-bit random; no brute-force surface).
pub fn hash_secret(secret: &str) -> String {
    hub_core::common::sha256(secret.as_bytes())
}

/// `Path.mkdir(parents=True, exist_ok=True, mode=mode)`: missing parents get the default mode, the leaf gets `mode`
/// (both under the umask).
pub fn mkdir_parents(path: &Path, mode: u32) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.is_dir()
    {
        fs::create_dir_all(parent)?;
    }
    match fs::DirBuilder::new().mode(mode).create(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(e) => Err(e),
    }
}

/// `Principal`: who a bearer token is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub id: String,
    pub client: String,
    pub admin: bool,
    /// ns -> "read" | "write", in the stored order
    pub scopes: Vec<(String, String)>,
    pub rate_per_min: i64,
    pub daily_queries: i64,
    pub daily_writes: i64,
}

impl Principal {
    pub fn admin() -> Self {
        Self {
            id: "admin".into(),
            client: "admin".into(),
            admin: true,
            scopes: Vec::new(),
            rate_per_min: 0,
            daily_queries: 0,
            daily_writes: 0,
        }
    }

    /// `can(ns, mode)`.
    pub fn can(&self, ns: &str, mode: &str) -> bool {
        if self.admin {
            return true;
        }
        match self.scopes.iter().rev().find(|(n, _)| n == ns).map(|(_, m)| m.as_str()) {
            Some("write") => true,
            Some("read") => mode == "read",
            _ => false,
        }
    }
}

/// A namespaces row.
#[derive(Debug, Clone, PartialEq)]
pub struct NsRow {
    pub name: String,
    pub backend: String,
    pub embedding_model: String,
    pub dim: i64,
    pub description: String,
    pub created: f64,
}

/// An indexes row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRow {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub project: String,
}

pub struct Db {
    pub dir: PathBuf,
    conn: Mutex<Connection>,
}

#[allow(clippy::needless_pass_by_value)]
fn sql_err(e: rusqlite::Error) -> io::Error {
    io::Error::other(e.to_string())
}

fn int_at(v: ValueRef<'_>) -> i64 {
    match v {
        ValueRef::Integer(i) => i,
        #[allow(clippy::cast_possible_truncation)]
        ValueRef::Real(f) => f as i64,
        _ => 0,
    }
}

fn real_at(v: ValueRef<'_>) -> f64 {
    match v {
        #[allow(clippy::cast_precision_loss)]
        ValueRef::Integer(i) => i as f64,
        ValueRef::Real(f) => f,
        _ => 0.0,
    }
}

fn text_at(v: ValueRef<'_>) -> String {
    match v {
        ValueRef::Text(t) | ValueRef::Blob(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => hub_core::py::float::repr(f),
        ValueRef::Null => String::new(),
    }
}

/// Today in UTC (`time.strftime("%Y-%m-%d", time.gmtime())`).
fn today() -> String {
    let d = time::OffsetDateTime::now_utc().date();
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

fn scopes_of(text: &str) -> Vec<(String, String)> {
    let Ok(PyValue::List(items)) = pyjson::loads_str(text) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|s| Some((s.get("ns")?.as_str()?.to_string(), s.get("mode")?.as_str()?.to_string())))
        .collect()
}

/// `json.dumps([{"ns": .., "mode": ..}, ...])`.
pub fn scopes_json(scopes: &[(String, String)]) -> String {
    pyjson::dumps(&scopes_value(scopes), Style::Std)
}

pub fn scopes_value(scopes: &[(String, String)]) -> PyValue {
    PyValue::List(
        scopes
            .iter()
            .map(|(n, m)| obj([("ns", PyValue::str(n.as_str())), ("mode", PyValue::str(m.as_str()))]))
            .collect(),
    )
}

impl Db {
    /// `Db(data_dir)`: the directory is created 0700 (and chmod'ed), the DB file chmod'ed 0600.
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        mkdir_parents(data_dir, 0o700)?;
        fs::set_permissions(data_dir, fs::Permissions::from_mode(0o700))?;
        let file = data_dir.join("knowledge.db");
        let conn = Connection::open(&file).map_err(sql_err)?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(sql_err)?;
        conn.execute_batch(SCHEMA).map_err(sql_err)?;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            dir: data_dir.to_path_buf(),
            conn: Mutex::new(conn),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> io::Result<T> {
        let c = self.conn.lock().map_err(|_| io::Error::other("db lock poisoned"))?;
        f(&c).map_err(sql_err)
    }

    /// `admin_key()`: create (once) and return the bootstrap admin key; the file is 0600.
    pub fn admin_key(&self) -> io::Result<String> {
        let p = self.dir.join("admin.key");
        if !p.exists() {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&p)?;
            f.write_all(format!("akn_admin_{}\n", hub_core::common::token_urlsafe(32)).as_bytes())?;
        }
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600))?;
        let text = fs::read_to_string(&p)?;
        Ok(hub_core::py::text::strip(&text).to_string())
    }

    // --- namespaces ----------------------------------------------------------------------------------------------
    pub fn ns_names(&self) -> io::Result<Vec<String>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT name FROM namespaces ORDER BY name")?;
            st.query_map([], |r| Ok(text_at(r.get_ref(0)?)))?.collect()
        })
    }

    pub fn ns_row(&self, name: &str) -> io::Result<Option<NsRow>> {
        self.with(|c| {
            c.query_row("SELECT * FROM namespaces WHERE name=?", [name], |r| {
                Ok(NsRow {
                    name: text_at(r.get_ref(0)?),
                    backend: text_at(r.get_ref(1)?),
                    embedding_model: text_at(r.get_ref(2)?),
                    dim: int_at(r.get_ref(3)?),
                    description: text_at(r.get_ref(4)?),
                    created: real_at(r.get_ref(5)?),
                })
            })
            .optional()
        })
    }

    pub fn ns_insert(&self, row: &NsRow) -> io::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO namespaces VALUES(?,?,?,?,?,?)",
                params![
                    row.name,
                    row.backend,
                    row.embedding_model,
                    row.dim,
                    row.description,
                    row.created
                ],
            )
            .map(|_| ())
        })
    }

    pub fn ns_set_description(&self, name: &str, description: &str) -> io::Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE namespaces SET description=? WHERE name=?",
                params![description, name],
            )
            .map(|_| ())
        })
    }

    pub fn ns_delete(&self, name: &str) -> io::Result<()> {
        self.with(|c| c.execute("DELETE FROM namespaces WHERE name=?", [name]).map(|_| ()))
    }

    // --- tokens --------------------------------------------------------------------------------------------------
    /// `add_token(...)` -> (id, secret). The secret is returned once; only its hash is stored.
    pub fn add_token(
        &self,
        client: &str,
        scopes: &[(String, String)],
        rate: i64,
        dq: i64,
        dw: i64,
    ) -> io::Result<(String, String)> {
        let tid = hub_core::common::token_hex(6);
        let secret = format!("akn_{tid}_{}", hub_core::common::token_urlsafe(32));
        let now = hub_core::common::now();
        self.with(|c| {
            c.execute(
                "INSERT INTO tokens VALUES(?,?,?,?,?,?,?,?)",
                params![
                    tid,
                    client,
                    hash_secret(&secret),
                    scopes_json(scopes),
                    rate,
                    dq,
                    dw,
                    now
                ],
            )
        })?;
        Ok((tid, secret))
    }

    /// `token_by_secret(secret)`.
    pub fn token_by_secret(&self, secret: &str) -> io::Result<Option<Principal>> {
        let h = hash_secret(secret);
        self.with(|c| {
            c.query_row("SELECT * FROM tokens WHERE hash=?", [h], |r| {
                Ok(Principal {
                    id: text_at(r.get_ref(0)?),
                    client: text_at(r.get_ref(1)?),
                    admin: false,
                    scopes: scopes_of(&text_at(r.get_ref(3)?)),
                    rate_per_min: int_at(r.get_ref(4)?),
                    daily_queries: int_at(r.get_ref(5)?),
                    daily_writes: int_at(r.get_ref(6)?),
                })
            })
            .optional()
        })
    }

    /// `list_tokens()`: dicts without the hash, oldest first.
    pub fn list_tokens(&self) -> io::Result<Vec<(String, PyValue)>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM tokens ORDER BY created")?;
            st.query_map([], |r| {
                let id = text_at(r.get_ref(0)?);
                let scopes = pyjson::loads_str(&text_at(r.get_ref(3)?)).unwrap_or(PyValue::List(Vec::new()));
                Ok((
                    id.clone(),
                    obj([
                        ("id", PyValue::Str(id)),
                        ("client", PyValue::Str(text_at(r.get_ref(1)?))),
                        ("scopes", scopes),
                        ("rate_per_min", PyValue::Int(int_at(r.get_ref(4)?))),
                        ("daily_queries", PyValue::Int(int_at(r.get_ref(5)?))),
                        ("daily_writes", PyValue::Int(int_at(r.get_ref(6)?))),
                        ("created", PyValue::Float(real_at(r.get_ref(7)?))),
                    ]),
                ))
            })?
            .collect()
        })
    }

    /// `drop_token(tid)`.
    pub fn drop_token(&self, tid: &str) -> io::Result<bool> {
        self.with(|c| {
            c.execute("DELETE FROM usage WHERE token_id=?", [tid])?;
            Ok(c.execute("DELETE FROM tokens WHERE id=?", [tid])? > 0)
        })
    }

    /// `drop_ns_scopes(ns)`: a dropped namespace must not leave live grants (a later namespace of the same name would
    /// inherit them).
    pub fn drop_ns_scopes(&self, ns: &str) -> io::Result<()> {
        self.with(|c| {
            let rows: Vec<(String, String)> = {
                let mut st = c.prepare("SELECT id, scopes FROM tokens")?;
                st.query_map([], |r| Ok((text_at(r.get_ref(0)?), text_at(r.get_ref(1)?))))?
                    .collect::<rusqlite::Result<_>>()?
            };
            for (id, scopes) in rows {
                let Ok(PyValue::List(items)) = pyjson::loads_str(&scopes) else {
                    continue;
                };
                let kept: Vec<PyValue> = items
                    .into_iter()
                    .filter(|s| s.get("ns").and_then(PyValue::as_str) != Some(ns))
                    .collect();
                c.execute(
                    "UPDATE tokens SET scopes=? WHERE id=?",
                    params![pyjson::dumps(&PyValue::List(kept), Style::Std), id],
                )?;
            }
            Ok(())
        })
    }

    // --- usage ---------------------------------------------------------------------------------------------------
    /// `bump(tid, field, n, limit)`: add n to today's counter unless that would exceed the limit.
    pub fn bump(&self, tid: &str, field: &str, n: i64, limit: i64) -> io::Result<bool> {
        let col = if field == "queries" { "queries" } else { "writes" };
        let day = today();
        self.with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO usage(token_id, day) VALUES(?,?)",
                params![tid, day],
            )?;
            let cur: i64 = c.query_row(
                &format!("SELECT {col} FROM usage WHERE token_id=? AND day=?"),
                params![tid, day],
                |r| Ok(int_at(r.get_ref(0)?)),
            )?;
            if cur + n > limit {
                return Ok(false);
            }
            c.execute(
                &format!("UPDATE usage SET {col}={col}+? WHERE token_id=? AND day=?"),
                params![n, tid, day],
            )?;
            Ok(true)
        })
    }

    /// `usage(tid)` -> (queries, writes) today.
    pub fn usage(&self, tid: &str) -> io::Result<(i64, i64)> {
        let day = today();
        self.with(|c| {
            Ok(c.query_row(
                "SELECT queries, writes FROM usage WHERE token_id=? AND day=?",
                params![tid, day],
                |r| Ok((int_at(r.get_ref(0)?), int_at(r.get_ref(1)?))),
            )
            .optional()?
            .unwrap_or((0, 0)))
        })
    }

    // --- shared-index registry -----------------------------------------------------------------------------------
    pub fn indexes(&self) -> io::Result<Vec<IndexRow>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM indexes ORDER BY name")?;
            st.query_map([], |r| {
                Ok(IndexRow {
                    name: text_at(r.get_ref(0)?),
                    kind: text_at(r.get_ref(1)?),
                    path: text_at(r.get_ref(2)?),
                    project: text_at(r.get_ref(3)?),
                })
            })?
            .collect()
        })
    }

    pub fn index(&self, name: &str) -> io::Result<Option<IndexRow>> {
        Ok(self.indexes()?.into_iter().find(|r| r.name == name))
    }

    pub fn index_insert(&self, row: &IndexRow) -> io::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO indexes VALUES(?,?,?,?)",
                params![row.name, row.kind, row.path, row.project],
            )
            .map(|_| ())
        })
    }

    /// Rows deleted.
    pub fn index_delete(&self, name: &str) -> io::Result<usize> {
        self.with(|c| c.execute("DELETE FROM indexes WHERE name=?", [name]))
    }

    /// Raw `SELECT hash FROM tokens` (tests: hashes at rest).
    pub fn token_hashes(&self) -> io::Result<Vec<String>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT hash FROM tokens")?;
            st.query_map([], |r| Ok(text_at(r.get_ref(0)?)))?.collect()
        })
    }
}
