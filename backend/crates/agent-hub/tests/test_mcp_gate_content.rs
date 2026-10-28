//! Port of WS-B1's rows of backend/tests/core/test_mcp_gate.py and test_audit_round2.py: `scan_status: clean` is bound
//! to an HMAC (keyed outside the content repo) of the fields that decide what runs; the attestation key is private and
//! created once. Security control S-ATTEST.

mod b1_support;

use std::os::unix::fs::PermissionsExt;

use agent_hub::services::attest::{Attestor, mcp_blob};
use agent_hub::services::resolver::to_mcp;
use b1_support::{R, T, add_client_with, hub, pv};
use hub_core::value::PyValue;
use serde_json::{Value, json};

fn base() -> Value {
    json!({"transport": "stdio", "command": "srv", "args": ["--x"], "sandbox_profile": "srt", "scan_status": "clean"})
}

#[allow(clippy::needless_pass_by_value)] // called with json!(...) literals
fn with(changes: Value) -> Value {
    let mut b = base();
    if let (Some(m), Some(c)) = (b.as_object_mut(), changes.as_object()) {
        for (k, v) in c {
            m.insert(k.clone(), v.clone());
        }
    }
    b
}

#[allow(clippy::needless_pass_by_value)] // used as `map_err(d)`
fn d(p: hub_core::Problem) -> String {
    format!("{} {}: {}", p.status, p.code, p.detail)
}

/// The `mcp_client` fixture: a client managing MCP with `srv` enabled, committed.
fn mcp_client() -> Result<T, Box<dyn std::error::Error>> {
    let t = hub()?;
    add_client_with(&t.hub, &t.home(), "fake1", json!({"manage": {"mcp": true}})).map_err(d)?;
    t.hub.work.put("mcp", "srv", &pv(base()), None).map_err(d)?;
    t.hub
        .work
        .put_profile("fake1", &pv(json!({"enable": {"mcp": ["srv"]}})))
        .map_err(d)?;
    t.hub.commit("c", "t").map_err(d)?;
    Ok(t)
}

fn codes(t: &T) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let plan = t.hub.plan(None, "committed").map_err(d)?;
    let mut c: Vec<String> = plan.clients[0]
        .floor_violations
        .iter()
        .map(|f| f.code.clone())
        .collect();
    c.sort();
    c.dedup();
    Ok(c)
}

/// What the resolver sees for `srv` right now (the attestation check of `to_mcp`).
fn status_now(t: &T) -> Result<String, Box<dyn std::error::Error>> {
    let it = t.hub.work.load("mcp", "srv").map_err(d)?;
    Ok(to_mcp(&it, Some(t.hub.attestor.as_ref())).map_err(d)?.scan_status)
}

#[test]
fn test_clean_is_bound_to_a_digest_of_the_scanned_fields() -> R {
    let t = mcp_client()?;
    assert!(
        codes(&t)?.is_empty()
            && !t
                .hub
                .work
                .load("mcp", "srv")
                .map_err(d)?
                .meta_str("scan_digest")
                .is_empty()
    );
    // an edit that changes command/args/url/pinned_ref/transport invalidates `clean`, even when the request resends it
    for change in [
        json!({"command": "evil"}),
        json!({"args": ["--y"]}),
        json!({"pinned_ref": "abc123"}),
    ] {
        t.hub
            .work
            .put("mcp", "srv", &pv(with(change.clone())), None)
            .map_err(d)?;
        t.hub.commit("c", "t").map_err(d)?;
        assert_eq!(codes(&t)?, ["mcp_not_clean"], "{change}");
        assert_eq!(t.hub.effective("fake1", "committed").map_err(d)?["mcp"], json!([]));
        t.hub.work.put("mcp", "srv", &pv(base()), None).map_err(d)?; // restoring the scanned fields restores `clean`
        t.hub.commit("c", "t").map_err(d)?;
        assert!(codes(&t)?.is_empty());
    }
    // changing an unrelated field keeps it; a file edit that bypasses the API cannot forge the digest
    t.hub
        .work
        .put("mcp", "srv", &pv(with(json!({"notes": "hello"}))), None)
        .map_err(d)?;
    t.hub.commit("c", "t").map_err(d)?;
    assert!(codes(&t)?.is_empty());
    let p = t.content().join("mcp/srv.yaml");
    std::fs::write(
        &p,
        std::fs::read_to_string(&p)?.replace("command: srv", "command: rogue"),
    )?;
    t.hub.commit("c", "t").map_err(d)?;
    assert_eq!(codes(&t)?, ["mcp_not_clean"]);
    Ok(())
}

/// The content half of test_clean_is_bound_to_a_digest_of_the_scanned_fields: what `to_mcp` makes of each edit.
#[test]
fn clean_is_bound_to_the_scanned_fields_in_to_mcp() -> R {
    let t = hub()?;
    let w = &t.hub.work;
    w.put("mcp", "srv", &pv(base()), None).map_err(d)?;
    assert!(!w.load("mcp", "srv").map_err(d)?.meta_str("scan_digest").is_empty());
    assert_eq!(status_now(&t)?, "clean");
    for change in [
        json!({"command": "evil"}),
        json!({"args": ["--y"]}),
        json!({"pinned_ref": "abc123"}),
        json!({"transport": "http", "url": "https://x.example.com/mcp", "command": null}),
    ] {
        w.put("mcp", "srv", &pv(with(change.clone())), None).map_err(d)?;
        assert_eq!(status_now(&t)?, "unscanned", "{change}");
        w.put("mcp", "srv", &pv(base()), None).map_err(d)?;
        assert_eq!(status_now(&t)?, "clean");
    }
    w.put("mcp", "srv", &pv(with(json!({"notes": "hello"}))), None)
        .map_err(d)?;
    assert_eq!(status_now(&t)?, "clean");
    let p = t.content().join("mcp/srv.yaml");
    std::fs::write(
        &p,
        std::fs::read_to_string(&p)?.replace("command: srv", "command: rogue"),
    )?;
    assert_eq!(status_now(&t)?, "unscanned");
    Ok(())
}

#[test]
fn test_reattesting_from_unscanned_binds_the_current_fields() -> R {
    let t = mcp_client()?;
    t.hub
        .work
        .put(
            "mcp",
            "srv",
            &pv(with(json!({"command": "new", "scan_status": "unscanned"}))),
            None,
        )
        .map_err(d)?;
    t.hub
        .work
        .put("mcp", "srv", &pv(with(json!({"command": "new"}))), None)
        .map_err(d)?; // the operator attests to what is there now
    t.hub.commit("c", "t").map_err(d)?;
    assert!(codes(&t)?.is_empty());
    Ok(())
}

/// The content half of test_reattesting_from_unscanned_binds_the_current_fields.
#[test]
fn reattesting_from_unscanned_binds_the_current_fields_in_to_mcp() -> R {
    let t = hub()?;
    let w = &t.hub.work;
    w.put("mcp", "srv", &pv(base()), None).map_err(d)?;
    w.put(
        "mcp",
        "srv",
        &pv(with(json!({"command": "new", "scan_status": "unscanned"}))),
        None,
    )
    .map_err(d)?;
    assert!(
        w.load("mcp", "srv")
            .map_err(d)?
            .meta_field("scan_digest")
            .is_none_or(PyValue::is_none)
    );
    w.put("mcp", "srv", &pv(with(json!({"command": "new"}))), None)
        .map_err(d)?;
    assert_eq!(status_now(&t)?, "clean");
    // attest_mcp binds `clean` to the current fields too
    w.put(
        "mcp",
        "srv",
        &pv(with(json!({"command": "newer", "scan_status": "unscanned"}))),
        None,
    )
    .map_err(d)?;
    assert_eq!(status_now(&t)?, "unscanned");
    w.attest_mcp("srv").map_err(d)?;
    assert_eq!(status_now(&t)?, "clean");
    Ok(())
}

#[test]
fn test_attestation_key_is_private_and_created_once() -> R {
    let tmp = tempfile::tempdir()?;
    let f = tmp.path().join("state").join("attest.key");
    let a = Attestor::new(&f);
    let args = |x: &str| PyValue::List(vec![PyValue::str(x)]);
    let (c, s) = (PyValue::str("c"), PyValue::str("stdio"));
    let d1 = a
        .digest(&c, &args("a"), &PyValue::None, &PyValue::None, &s)
        .map_err(d)?;
    assert_eq!(std::fs::metadata(&f)?.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read_to_string(&f)?.trim().len(), 64);
    assert_eq!(
        Attestor::new(&f)
            .digest(&c, &args("a"), &PyValue::None, &PyValue::None, &s)
            .map_err(d)?,
        d1
    ); // same key on the next start
    assert_ne!(
        a.digest(&c, &args("b"), &PyValue::None, &PyValue::None, &s)
            .map_err(d)?,
        d1
    );
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644))?;
    let e = Attestor::new(&f)
        .digest(&c, &PyValue::List(vec![]), &PyValue::None, &PyValue::None, &s)
        .err()
        .ok_or("a loose key was accepted")?;
    assert_eq!((e.status, e.code.as_str()), (503, "attest_key_unusable"));
    // a symlink is never a key
    let link = tmp.path().join("state").join("link.key");
    std::os::unix::fs::symlink(&f, &link)?;
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600))?;
    assert!(
        Attestor::new(&link)
            .digest(&c, &PyValue::List(vec![]), &PyValue::None, &PyValue::None, &s)
            .is_err()
    );
    Ok(())
}

#[test]
fn test_forged_or_legacy_digests_do_not_make_a_server_clean() -> R {
    let t = hub()?;
    add_client_with(&t.hub, &t.home(), "fake1", json!({"manage": {"mcp": true}})).map_err(d)?;
    t.hub
        .work
        .put(
            "mcp",
            "srv",
            &pv(json!({"transport": "stdio", "command": "srv", "sandbox_profile": "srt", "scan_status": "clean"})),
            None,
        )
        .map_err(d)?;
    t.hub
        .work
        .put_profile("fake1", &pv(json!({"enable": {"mcp": ["srv"]}})))
        .map_err(d)?;
    t.hub.commit("c", "t").map_err(d)?;
    assert!(
        t.hub.plan(None, "committed").map_err(d)?.clients[0]
            .floor_violations
            .is_empty()
    );
    assert_eq!(
        t.hub.effective("fake1", "committed").map_err(d)?["mcp"][0]["scan_status"],
        "clean"
    );
    forge(&t)?;
    t.hub.commit("forge", "t").map_err(d)?;
    let v = t.hub.plan(None, "committed").map_err(d)?.clients[0]
        .floor_violations
        .clone();
    assert_eq!(v.iter().map(|f| f.code.as_str()).collect::<Vec<_>>(), ["mcp_not_clean"]);
    assert!(v[0].message.contains("hubctl attest-mcp"));
    strip_digest(&t)?;
    t.hub.commit("nodigest", "t").map_err(d)?;
    let v = t.hub.plan(None, "committed").map_err(d)?.clients[0]
        .floor_violations
        .clone();
    assert_eq!(v.iter().map(|f| f.code.as_str()).collect::<Vec<_>>(), ["mcp_not_clean"]);
    let key = std::fs::read_to_string(t.hub.cfg().data_dir.join("attest.key"))?;
    assert!(!std::fs::read_to_string(t.content().join("mcp/srv.yaml"))?.contains(key.trim()));
    Ok(())
}

/// An unkeyed sha256 of the same fields (what a previous version stored, or what an attacker can compute).
fn forge(t: &T) -> R {
    let blob = mcp_blob(
        &PyValue::str("srv"),
        &PyValue::List(vec![]),
        &PyValue::None,
        &PyValue::None,
        &PyValue::str("stdio"),
    );
    let p = t.content().join("mcp/srv.yaml");
    let text = std::fs::read_to_string(&p)?;
    let re = regex::Regex::new(r"scan_digest: \S+")?;
    std::fs::write(
        &p,
        re.replace(
            &text,
            format!("scan_digest: {}", hub_core::common::sha256(&blob)).as_str(),
        )
        .as_ref(),
    )?;
    Ok(())
}

fn strip_digest(t: &T) -> R {
    let p = t.content().join("mcp/srv.yaml");
    let text = std::fs::read_to_string(&p)?;
    let re = regex::Regex::new(r"scan_digest: \S+\n")?;
    std::fs::write(&p, re.replace(&text, "").as_ref())?;
    Ok(())
}

/// The content half of test_forged_or_legacy_digests_do_not_make_a_server_clean.
#[test]
fn forged_or_legacy_digests_do_not_make_a_server_clean_in_to_mcp() -> R {
    let t = hub()?;
    t.hub
        .work
        .put(
            "mcp",
            "srv",
            &pv(json!({"transport": "stdio", "command": "srv", "sandbox_profile": "srt", "scan_status": "clean"})),
            None,
        )
        .map_err(d)?;
    assert_eq!(status_now(&t)?, "clean");
    forge(&t)?;
    assert_eq!(status_now(&t)?, "unscanned");
    strip_digest(&t)?;
    assert_eq!(status_now(&t)?, "unscanned");
    let key = std::fs::read_to_string(t.hub.cfg().data_dir.join("attest.key"))?;
    assert!(!std::fs::read_to_string(t.content().join("mcp/srv.yaml"))?.contains(key.trim()));
    Ok(())
}
