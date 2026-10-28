//! Port of services/applylog.py: per-client apply history at `<data>/applies/<client>/<apply_id>.json`
//! (`json.dumps(rec, indent=1)`, 0600, dir 0700, newest 200 kept). apply_id = `%Y%m%dT%H%M%SZ-<plan id>`, so an apply
//! can be undone. Owner: WS-B2.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use hub_core::{HubResult, Problem};
use regex::Regex;
use serde_json::Value;

pub const APPLY_ID_RE: &str = r"\d{8}T\d{6}Z-[A-Za-z0-9_]{1,40}";
pub const KEEP: usize = 200;

/// Python `re.fullmatch` with `\d` = any Unicode decimal digit (str patterns are Unicode-aware).
static APPLY_ID: LazyLock<Regex> = LazyLock::new(|| hub_core::re(&format!("^(?:{APPLY_ID_RE})$")));

pub struct ApplyLog {
    pub dir: PathBuf,
}

/// `json.dumps(rec, indent=1)`.
pub fn dump_record(rec: &Value) -> String {
    hub_core::py::json::dumps(
        rec,
        &hub_core::py::json::Opts {
            indent: Some(1),
            ..hub_core::py::json::Opts::default()
        },
    )
}

/// The sort key `(r.get("time", 0), r.get("apply_id", ""))`.
fn key(r: &Value) -> (f64, String) {
    (
        r.get("time").and_then(Value::as_f64).unwrap_or(0.0),
        r.get("apply_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    )
}

impl ApplyLog {
    pub fn new(dir: &Path) -> Self {
        Self { dir: dir.to_path_buf() }
    }

    fn client_dir(&self, client: &str) -> HubResult<PathBuf> {
        if !hub_core::ids::valid_id(client) {
            return Err(Problem::bad_request("invalid client id").code("invalid_id"));
        }
        Ok(self.dir.join(client))
    }

    pub fn save(&self, rec: &Value) -> HubResult<()> {
        let client = rec
            .get("client")
            .and_then(Value::as_str)
            .ok_or_else(Problem::internal)?;
        let id = rec
            .get("apply_id")
            .and_then(Value::as_str)
            .ok_or_else(Problem::internal)?;
        let d = self.client_dir(client)?;
        hub_core::common::atomic_write(&d.join(format!("{id}.json")), dump_record(rec).as_bytes(), 0o600, 0o700)?;
        let recs = self.list(client)?;
        for old in recs.iter().skip(KEEP) {
            if let Some(oid) = old.get("apply_id").and_then(Value::as_str) {
                let _ = std::fs::remove_file(d.join(format!("{oid}.json")));
            }
        }
        Ok(())
    }

    pub fn get(&self, client: &str, apply_id: &str) -> HubResult<Value> {
        if !APPLY_ID.is_match(apply_id) {
            return Err(Problem::bad_request("invalid apply id").code("invalid_id"));
        }
        let path = self.client_dir(client)?.join(format!("{apply_id}.json"));
        let rec: Value = std::fs::read(&path)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .ok_or_else(|| Problem::not_found(format!("no such apply: {apply_id}")))?;
        if !rec.is_object() {
            return Err(Problem::internal()); // Python: `assert isinstance(rec, dict)`
        }
        Ok(rec)
    }

    /// newest first, by (time, apply_id)
    pub fn list(&self, client: &str) -> HubResult<Vec<Value>> {
        let d = self.client_dir(client)?;
        let mut out = Vec::new();
        if d.is_dir() {
            for e in std::fs::read_dir(&d)?.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.ends_with(".json") {
                    continue;
                }
                let Some(rec) = std::fs::read(e.path())
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok())
                    .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                else {
                    continue;
                };
                if rec.is_object() {
                    out.push(rec);
                }
            }
        }
        out.sort_by(|a, b| {
            let (ka, kb) = (key(a), key(b));
            kb.0.partial_cmp(&ka.0)
                .unwrap_or(Ordering::Equal)
                .then_with(|| kb.1.cmp(&ka.1))
        });
        Ok(out)
    }

    pub fn latest_active(&self, client: &str) -> HubResult<Option<Value>> {
        Ok(self
            .list(client)?
            .into_iter()
            .find(|r| !truthy(r.get("rolled_back_at"))))
    }

    pub fn update(&self, client: &str, apply_id: &str, patch: &Value) -> HubResult<()> {
        let mut rec = self.get(client, apply_id)?;
        if let (Some(m), Some(p)) = (rec.as_object_mut(), patch.as_object()) {
            for (k, v) in p {
                m.insert(k.clone(), v.clone());
            }
        }
        self.save(&rec)
    }

    /// `{"apply_id", "time", "plan_id", "content_hash", "rolled_back_at", "counts": {add, change, remove}}`
    pub fn summary(rec: &Value) -> Value {
        let files = rec.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
        let count = |a: &str| {
            files
                .iter()
                .filter(|f| f.get("action").and_then(Value::as_str) == Some(a))
                .count()
        };
        serde_json::json!({
            "apply_id": rec.get("apply_id").cloned().unwrap_or(Value::Null),
            "time": rec.get("time").cloned().unwrap_or(Value::Null),
            "plan_id": rec.get("plan_id").cloned().unwrap_or(Value::Null),
            "content_hash": rec.get("content_hash").cloned().unwrap_or(Value::Null),
            "rolled_back_at": rec.get("rolled_back_at").cloned().unwrap_or(Value::Null),
            "counts": {"add": count("add"), "change": count("change"), "remove": count("remove")},
        })
    }
}

/// Python truthiness of a JSON value.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use serde_json::json;

    use super::*;

    type R = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn records_round_trip_sorted_and_capped() -> R {
        let d = tempfile::tempdir()?;
        let log = ApplyLog::new(d.path());
        for i in 0..(KEEP + 3) {
            let rec = json!({"apply_id": format!("20261027T{:06}Z-plan_x{i}", i), "client": "c1",
                             "time": 1000.0 + i as f64, "plan_id": "p", "content_hash": "h",
                             "files": [{"action": "add"}, {"action": "change"}, {"action": "add"}]});
            log.save(&rec)?;
        }
        let all = log.list("c1")?;
        assert_eq!(all.len(), KEEP);
        assert_eq!(all[0]["time"], json!(1202.0));
        let id = all[0]["apply_id"].as_str().ok_or("id")?.to_string();
        let p = d.path().join("c1").join(format!("{id}.json"));
        assert_eq!(std::fs::metadata(&p)?.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(d.path().join("c1"))?.permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(std::fs::read_to_string(&p)?, dump_record(&all[0]));
        assert!(std::fs::read_to_string(&p)?.starts_with("{\n \"apply_id\": "));
        let s = ApplyLog::summary(&all[0]);
        assert_eq!(s["counts"], json!({"add": 2, "change": 1, "remove": 0}));
        log.update("c1", &id, &json!({"rolled_back_at": 5.0}))?;
        assert_eq!(log.get("c1", &id)?["rolled_back_at"], json!(5.0));
        assert_ne!(log.latest_active("c1")?.ok_or("none")?["apply_id"], json!(id));
        Ok(())
    }

    #[test]
    fn hostile_ids_are_400_and_missing_is_404() -> R {
        let d = tempfile::tempdir()?;
        let log = ApplyLog::new(d.path());
        let e = log.get("c1", "../../etc/passwd").err().ok_or("accepted")?;
        assert_eq!(
            (e.status, e.code.as_str(), e.detail.as_str()),
            (400, "invalid_id", "invalid apply id")
        );
        let e = log.get("../x", "20261027T000000Z-p").err().ok_or("accepted")?;
        assert_eq!((e.status, e.detail.as_str()), (400, "invalid client id"));
        let e = log.get("c1", "20261027T000000Z-p").err().ok_or("found")?;
        assert_eq!(
            (e.status, e.detail.as_str()),
            (404, "no such apply: 20261027T000000Z-p")
        );
        assert!(log.list("c1")?.is_empty());
        Ok(())
    }
}
