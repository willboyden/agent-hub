//! Port of the WS-A row of backend/tests/core/test_audit_fixes.py: a Hermes-style observe (a big, hostile JSON document
//! on stdout) goes through the hub's STREAMING runner (process-group kill on the first byte over a cap), not the plain
//! one. Owner: WS-A.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

use agent_hub::adapters::base::ClientAdapter;
use agent_hub::adapters::hermes::{HermesAdapter, MAX_OBSERVE_OUT};
use agent_hub::services::fs::StreamingRunner;
use agent_hub::services::hub::default_observe_runner;

#[test]
fn test_hermes_style_observe_runs_through_the_streaming_runner() {
    // `isinstance(hub.observe_runner, StreamingRunner)`: since A-4 the hub holds the runner as a trait object, so the
    // type is checked where the hub builds it, with the caps Python gives it (120 s, 48 MiB stdout, 1 MiB stderr)
    let r: StreamingRunner = default_observe_runner();
    assert!((r.max_timeout - 120.0).abs() < f64::EPSILON);
    assert_eq!(r.max_stdout, 48 * 1024 * 1024);
    assert_eq!(r.max_stderr, 1024 * 1024);
    // the adapter's own cap on what it parses is the same 48 MiB, so the runner kills before the parser would refuse
    assert_eq!(r.max_stdout, MAX_OBSERVE_OUT);
    // and observe() asks for 120 s, the runner's ceiling
    let fake = FakeTimeout::default();
    let mut cfg = HermesAdapter.default_config();
    if let hub_core::value::PyValue::Dict(d) = &mut cfg.params {
        for (k, v) in d.iter_mut() {
            if k.as_str() == Some("compose_project") {
                *v = hub_core::value::PyValue::str("p");
            }
        }
    }
    let _ = HermesAdapter.observe(&cfg, &NoFs, &fake);
    assert_eq!(*fake.0.lock().unwrap(), vec![15.0, 120.0]);
}

#[derive(Default)]
struct FakeTimeout(std::sync::Mutex<Vec<f64>>);

impl agent_hub::adapters::base::CommandRunner for FakeTimeout {
    fn run(&self, argv: &[String], timeout: f64) -> std::io::Result<(i32, String, String)> {
        self.0.lock().unwrap().push(timeout);
        let out = if argv.iter().any(|a| a == "ps") {
            "ctr\n"
        } else {
            r#"{"files": {}}"#
        };
        Ok((0, out.into(), String::new()))
    }
}

struct NoFs;

impl agent_hub::adapters::base::FileSystem for NoFs {
    fn read_bytes(&self, _p: &str) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other("none"))
    }
    fn exists(&self, _p: &str) -> bool {
        false
    }
    fn listdir(&self, _p: &str) -> std::io::Result<Vec<String>> {
        Ok(vec![])
    }
    fn is_dir(&self, _p: &str) -> bool {
        false
    }
    fn size(&self, _p: &str) -> Option<std::io::Result<u64>> {
        None
    }
}
