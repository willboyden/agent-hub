//! Port of WS-B1's rows of backend/tests/core/test_api.py: the content, client, organize, import and inbox routes over
//! the hub's own HTTP pipeline (in process, as Starlette's TestClient), with the FakeAdapter registered.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod b1_support;

use b1_support::{API, Api, T, add_client, api, hub, hub_with, pv, seed_content};
use serde_json::{Value, json};

fn key(t: &T, name: &str, role: &str, client: Option<&str>, scopes: &[&str]) -> String {
    let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_string()).collect();
    t.hub.auth.create_key(name, role, client, &scopes).unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_string()
}

fn bearer(k: &str) -> String {
    format!("Bearer {k}")
}

/// `_applied_setup`: a client with one of every kind, committed; an admin header.
fn applied_setup(t: &T) -> String {
    add_client(&t.hub, &t.home()).unwrap();
    seed_content(&t.hub, "fake1").unwrap();
    t.hub.commit("c", "t").unwrap();
    bearer(&key(t, "adm", "admin", None, &[]))
}

async fn get(a: &Api, path: &str, auth: &str) -> b1_support::Resp {
    a.req("GET", path, &[("authorization", auth)], None).await
}

async fn post(a: &Api, path: &str, auth: &str, body: Value) -> b1_support::Resp {
    a.req("POST", path, &[("authorization", auth)], Some(body)).await
}

#[tokio::test]
async fn test_health_is_open_everything_else_needs_a_key() {
    let t = hub().unwrap();
    let a = api(&t);
    assert_eq!(
        a.req("GET", &format!("{API}/health"), &[], None).await.json()["status"],
        "ok"
    );
    for path in [
        format!("{API}/clients"),
        format!("{API}/openapi.json"),
        "/metrics".into(),
        format!("{API}/audit"),
    ] {
        let r = a.req("GET", &path, &[], None).await;
        assert!(r.status == 401 && r.code() == "unauthorized", "{path}");
    }
    assert_eq!(get(&a, &format!("{API}/clients"), &bearer("hc_nope")).await.status, 401);
}

#[tokio::test]
async fn test_hostile_ids_in_urls() {
    let t = hub().unwrap();
    let a = api(&t);
    let h = bearer(&key(&t, "t-admin", "admin", None, &[]));
    let long = "x".repeat(65);
    for name in ["..%2F..%2Fetc%2Fpasswd", "%2e%2e", "A", "a%0Ab", long.as_str(), "a%00b"] {
        let r = get(&a, &format!("{API}/skills/{name}"), &h).await;
        assert!(matches!(r.status, 400 | 404), "{name} {}", r.status);
        let r = a
            .req(
                "PUT",
                &format!("{API}/skills/{name}"),
                &[("authorization", &h)],
                Some(json!({"description": "d", "body": ""})),
            )
            .await;
        assert!(matches!(r.status, 400 | 404 | 405 | 422), "{name} {}", r.status);
    }
}

#[tokio::test]
async fn test_viewer_can_read_but_not_write() {
    let t = hub().unwrap();
    let a = api(&t);
    let (v, adm) = (
        bearer(&key(&t, "t-viewer", "viewer", None, &[])),
        bearer(&key(&t, "t-admin", "admin", None, &[])),
    );
    assert_eq!(get(&a, &format!("{API}/clients"), &v).await.status, 200);
    let body = || Some(json!({"title": "t", "body": "b"}));
    assert_eq!(
        a.req(
            "PUT",
            &format!("{API}/instructions/one"),
            &[("authorization", &v)],
            body()
        )
        .await
        .status,
        403
    );
    assert_eq!(
        a.req(
            "PUT",
            &format!("{API}/instructions/one"),
            &[("authorization", &adm)],
            body()
        )
        .await
        .status,
        200
    );
}

#[tokio::test]
async fn test_client_token_scopes() {
    let t = hub().unwrap();
    let a = api(&t);
    add_client(&t.hub, &t.home()).unwrap();
    let (v, adm) = (
        bearer(&key(&t, "t-viewer", "viewer", None, &[])),
        bearer(&key(&t, "t-admin", "admin", None, &[])),
    );
    let tok = bearer(&key(&t, "agent", "client", Some("fake1"), &["inbox:write"]));
    let body = json!({"client": "fake1", "title": "Idea", "body": "text", "type": "reference"});
    let r = a
        .req(
            "POST",
            &format!("{API}/memory/inbox"),
            &[("authorization", &tok)],
            Some(body.clone()),
        )
        .await;
    assert!(r.status == 201 && r.json()["client"] == "fake1");
    let mut other = body.clone();
    other["client"] = json!("fake2");
    assert_eq!(
        a.req(
            "POST",
            &format!("{API}/memory/inbox"),
            &[("authorization", &tok)],
            Some(other)
        )
        .await
        .status,
        403
    );
    for (m, p) in [
        ("GET", format!("{API}/clients")),
        ("GET", format!("{API}/memory/inbox")),
        ("POST", format!("{API}/plan")),
        ("POST", format!("{API}/memory/inbox/x/promote")),
        ("GET", "/metrics".to_string()),
    ] {
        assert_eq!(a.req(m, &p, &[("authorization", &tok)], None).await.status, 403, "{p}");
    }
    let unscoped = bearer(&key(&t, "agent2", "client", Some("fake1"), &[]));
    assert_eq!(
        a.req(
            "POST",
            &format!("{API}/memory/inbox"),
            &[("authorization", &unscoped)],
            Some(body.clone())
        )
        .await
        .status,
        403
    );
    // a viewer may not write memory; only an admin promotes
    assert_eq!(
        a.req(
            "POST",
            &format!("{API}/memory/inbox"),
            &[("authorization", &v)],
            Some(body)
        )
        .await
        .status,
        403
    );
    let item = get(&a, &format!("{API}/memory/inbox"), &adm).await.json()["items"][0].clone();
    let id = item["id"].as_str().unwrap();
    let r = a
        .req(
            "POST",
            &format!("{API}/memory/inbox/{id}/promote"),
            &[("authorization", &adm)],
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        get(&a, &format!("{API}/memory/{id}"), &adm).await.json()["meta"]["title"],
        "Idea"
    );
}

#[tokio::test]
async fn test_adapters_floor_settings_and_policy_check() {
    let t = hub().unwrap();
    let a = api(&t);
    let h = bearer(&key(&t, "t-admin", "admin", None, &[]));
    let ads = get(&a, &format!("{API}/adapters"), &h).await.json()["items"].clone();
    assert!(ads[0]["id"] == "fake" && ads[0].get("caps").is_some());
    assert_eq!(get(&a, &format!("{API}/adapters/fake"), &h).await.status, 200);
    assert_eq!(get(&a, &format!("{API}/adapters/nope"), &h).await.status, 404);
    let floor = get(&a, &format!("{API}/policy/floor"), &h).await.json();
    assert_eq!(floor["network_default"], "deny");
    for m in ["PUT", "POST", "DELETE", "PATCH"] {
        assert_eq!(
            a.req(m, &format!("{API}/policy/floor"), &[("authorization", &h)], None)
                .await
                .status,
            405,
            "{m}"
        );
    }
    let chk = a
        .req(
            "POST",
            &format!("{API}/policy/check"),
            &[("authorization", &h)],
            Some(json!({"rules": [{"kind": "path_read", "match": "~/**", "decision": "allow"}]})),
        )
        .await
        .json();
    assert!(chk["ok"] == false && chk["violations"][0]["code"] == "floor_path_read");
    let r = a
        .req(
            "POST",
            &format!("{API}/policy/check"),
            &[("authorization", &h)],
            Some(json!({"mcp": [{"name": "s", "transport": "stdio", "command": "c"}]})),
        )
        .await;
    assert_eq!(r.json()["ok"], false);
    assert_eq!(
        a.req(
            "POST",
            &format!("{API}/policy/check"),
            &[("authorization", &h)],
            Some(json!({"bogus": 1}))
        )
        .await
        .status,
        422
    );
    let s = get(&a, &format!("{API}/settings"), &h).await.json();
    assert!(!s.to_string().contains("hf_token") && s["info"]["knowledge"]["token"] == "[unset]");
    let put = a
        .req(
            "PUT",
            &format!("{API}/settings"),
            &[("authorization", &h)],
            Some(json!({"plan_retention": 5})),
        )
        .await;
    assert_eq!(put.json()["editable"]["plan_retention"], 5);
    assert_eq!(
        a.req(
            "PUT",
            &format!("{API}/settings"),
            &[("authorization", &h)],
            Some(json!({"content_dir": "/etc"}))
        )
        .await
        .status,
        422
    );
    assert_eq!(
        a.req(
            "PUT",
            &format!("{API}/settings"),
            &[("authorization", &h)],
            Some(json!({"plan_retention": 0}))
        )
        .await
        .status,
        422
    );
    assert_eq!(get(&a, &format!("{API}/content/validate"), &h).await.json()["ok"], true);
}

#[tokio::test]
async fn test_validate_spec_without_generic_adapter_is_503() {
    let t = hub().unwrap();
    let a = api(&t);
    let h = bearer(&key(&t, "t-admin", "admin", None, &[]));
    let r = a
        .req(
            "POST",
            &format!("{API}/clients/validate-spec"),
            &[("authorization", &h)],
            Some(json!({"spec": {}})),
        )
        .await;
    assert!(r.status == 503 && r.code() == "adapter_missing");
}

#[tokio::test]
async fn test_import_over_http() {
    let t = hub().unwrap();
    let a = api(&t);
    let h = bearer(&key(&t, "t-admin", "admin", None, &[]));
    std::fs::create_dir(t.home().join("agents")).unwrap();
    std::fs::write(t.home().join("agents/helper.md"), "body").unwrap();
    add_client(&t.hub, &t.home()).unwrap();
    let d = a
        .req(
            "POST",
            &format!("{API}/clients/fake1/discover"),
            &[("authorization", &h)],
            None,
        )
        .await
        .json();
    assert_eq!(d["items"]["agent"][0]["name"], "helper");
    let r = a
        .req(
            "POST",
            &format!("{API}/import"),
            &[("authorization", &h)],
            Some(json!({"client": "fake1", "items": [{"kind": "agent", "name": "helper"}], "on_conflict": "skip"})),
        )
        .await
        .json();
    assert_eq!(r["results"][0]["status"], "created");
    let bad = a
        .req(
            "POST",
            &format!("{API}/import"),
            &[("authorization", &h)],
            Some(json!({"client": "fake1", "on_conflict": "nope"})),
        )
        .await;
    assert_eq!(bad.status, 400);
    assert_eq!(
        get(&a, &format!("{API}/agents/helper"), &h).await.json()["meta"]["name"],
        "helper"
    );
}

#[tokio::test]
async fn test_trust_loopback_defaults_off_and_health_warns_when_on() {
    let t = hub().unwrap();
    let a = api(&t);
    assert!(!t.hub.cfg().trust_loopback);
    assert_eq!(
        a.req("GET", &format!("{API}/health"), &[], None).await.json()["warnings"],
        json!([])
    );
    t.hub.cfg.write().unwrap().trust_loopback = true;
    let w = a.req("GET", &format!("{API}/health"), &[], None).await.json()["warnings"][0].clone();
    assert!(w.as_str().unwrap().contains("TRUST_LOOPBACK"));
}

#[tokio::test]
async fn test_rendered_for_client() {
    let t = hub().unwrap();
    let a = api(&t);
    let h = applied_setup(&t);
    let q =
        |kind: &str, name: &str, extra: &str| format!("{API}/clients/fake1/rendered?kind={kind}&name={name}{extra}");
    let r = get(&a, &q("skill", "alpha", ""), &h).await.json();
    let mut paths: Vec<String> = r["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["path"].as_str().unwrap().to_string())
        .collect();
    paths.sort();
    assert_eq!(paths, ["skills/alpha/SKILL.md", "skills/alpha/ref/notes.md"]);
    assert!(r["artifacts"][0]["merge"] == "own" && r["artifacts"][0]["source_ids"] == json!(["alpha"]));
    let inst = get(&a, &q("instruction", "style", ""), &h).await.json();
    assert!(
        inst["artifacts"][0]["merge"] == "block"
            && inst["artifacts"][0]["content"]
                .as_str()
                .unwrap()
                .contains("Be concise.")
    );
    t.hub
        .work
        .put("skill", "beta", &pv(json!({"description": "B", "body": "b\n"})), None)
        .unwrap();
    t.hub.commit("beta", "t").unwrap();
    let miss = get(&a, &q("skill", "beta", ""), &h).await;
    assert!(
        miss.status == 404
            && miss.code() == "not_rendered"
            && miss.json()["detail"].as_str().unwrap().contains("not enabled")
    );
    t.hub.organizer.toggle("fake1", "skill", "beta", true).unwrap();
    assert_eq!(get(&a, &q("skill", "beta", ""), &h).await.status, 404); // committed
    let w = get(&a, &q("skill", "beta", "&source=working"), &h).await;
    assert!(w.status == 200 && w.json()["artifacts"][0]["path"] == "skills/beta/SKILL.md");
    t.hub
        .work
        .put(
            "instruction",
            "leak",
            &pv(json!({"title": "L", "order": 9, "body": "api_key = sk-abcdefghijklmnopqrstuv\n"})),
            None,
        )
        .unwrap();
    t.hub
        .organizer
        .enable_for_client("fake1", "instruction", &["leak".into()])
        .unwrap();
    let w = get(&a, &q("instruction", "leak", "&source=working"), &h).await;
    assert!(!String::from_utf8_lossy(&w.body).contains("sk-abcdefghij"));
    assert_eq!(get(&a, &q("bogus", "x", ""), &h).await.status, 400);
}

#[tokio::test]
async fn test_effective_rules_carry_source() {
    let t = hub().unwrap();
    let a = api(&t);
    let h = applied_setup(&t);
    t.hub
        .work
        .put(
            "rule",
            "via-coll",
            &pv(json!({"title": "c", "kind": "tool", "match": "shell", "decision": "ask"})),
            None,
        )
        .unwrap();
    t.hub
        .work
        .put_collection(
            "rc",
            &pv(json!({"id": "rc", "title": "RC", "members": [{"kind": "rule", "name": "via-coll"}]})),
        )
        .unwrap();
    let mut prof = t.hub.work.get_profile("fake1").unwrap();
    prof.collections.push("rc".into());
    t.hub
        .work
        .put_profile("fake1", &pv(serde_json::to_value(&prof).unwrap()))
        .unwrap();
    t.hub.commit("more", "t").unwrap();
    let rules = get(&a, &format!("{API}/clients/fake1/effective"), &h).await.json()["rules"].clone();
    let src = |k: &str, m: &str| -> Value {
        rules
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["kind"] == k && r["match"] == m)
            .map(|r| r["source"].clone())
            .unwrap_or(Value::Null)
    };
    assert_eq!(src("path_read", &format!("~/.{}", "aws/**")), "floor");
    assert_eq!(src("tool", "web_search"), "floor");
    assert_eq!(src("tool", "web_fetch"), "profile");
    assert_eq!(src("tool", "shell"), "content");
}

#[tokio::test]
async fn test_spec_yaml_accepted_and_bounded() {
    let generic = agent_hub::adapters::generic::adapter()
        .and_then(Result::ok)
        .expect("generic adapter");
    let t = hub_with(vec![generic], &[]).unwrap();
    let a = api(&t);
    let h = bearer(&key(&t, "adm", "admin", None, &[]));
    let spec_yaml = "version: 1\ninstructions:\n  root: project\n  file: AGENTS.md\n  mode: block\n";
    let vsp = format!("{API}/clients/validate-spec");
    let r = post(
        &a,
        &vsp,
        &h,
        json!({"spec_yaml": spec_yaml, "roots": {"project": "/tmp/dry"}}),
    )
    .await;
    assert!(r.status == 200 && r.json()["ok"] == true, "{}", r.json());
    assert_eq!(
        post(&a, &vsp, &h, json!({"spec": {}, "spec_yaml": spec_yaml}))
            .await
            .status,
        422
    );
    for bad in [
        json!("a: &x 1\nb: *x\n"),
        json!("- list\n"),
        json!(format!("x: {}", "y".repeat(70000))),
        json!(5),
    ] {
        assert_eq!(post(&a, &vsp, &h, json!({"spec_yaml": bad})).await.status, 422, "{bad}");
    }
    let home = t.home();
    let made = a
        .req(
            "POST",
            &format!("{API}/clients"),
            &[("authorization", &h)],
            Some(
                json!({"id": "gen1", "adapter": "generic", "display_name": "G", "spec_yaml": spec_yaml,
                         "roots": {"project": home.to_string_lossy()}}),
            ),
        )
        .await;
    assert!(made.status == 201 && made.json()["spec"]["instructions"]["file"] == "AGENTS.md");
    let two = a
        .req(
            "POST",
            &format!("{API}/clients"),
            &[("authorization", &h)],
            Some(json!({"id": "gen2", "adapter": "generic", "display_name": "G",
                         "spec": {"version": 1, "instructions": {"root": "project", "file": "A.md", "mode": "block"}},
                         "roots": {"project": home.to_string_lossy()}})),
        )
        .await;
    assert_eq!(two.status, 201);
}

#[tokio::test]
async fn test_suggestions_and_import_and_status_over_http() {
    let t = hub().unwrap();
    let a = api(&t);
    let h = applied_setup(&t);
    let viewer = bearer(&key(&t, "t-viewer", "viewer", None, &[]));
    for n in ["dep-a", "dep-b", "dep-c"] {
        t.hub
            .work
            .put("skill", n, &pv(json!({"description": n, "body": "b\n"})), None)
            .unwrap();
    }
    let items = get(&a, &format!("{API}/collections/suggestions"), &h).await.json()["items"].clone();
    assert!(items.as_array().unwrap().iter().any(|s| s["id"] == "dep"));
    assert_eq!(
        get(&a, &format!("{API}/collections/suggestions"), &viewer).await.status,
        200
    );
    let accp = format!("{API}/collections/suggestions/accept");
    assert_eq!(post(&a, &accp, &viewer, json!({"ids": ["dep"]})).await.status, 403);
    let r = post(
        &a,
        &accp,
        &h,
        json!({"ids": ["dep"], "overrides": {"dep": {"title": "Deps"}}}),
    )
    .await;
    assert!(r.status == 201 && r.json()["items"][0]["title"] == "Deps");
    assert_eq!(post(&a, &accp, &h, json!({"ids": ["dep"]})).await.status, 404); // exists now
    // client status contract: adopted vs applied are separate
    std::fs::create_dir_all(t.home().join("agents")).unwrap();
    std::fs::write(t.home().join("agents/helper.md"), "helper").unwrap();
    let r = a
        .req(
            "POST",
            &format!("{API}/import"),
            &[("authorization", &h)],
            Some(json!({"client": "fake1", "items": [{"kind": "agent", "name": "helper", "on_conflict": "link"}]})),
        )
        .await;
    assert_eq!(r.status, 200);
    let c = get(&a, &format!("{API}/clients/fake1"), &h).await.json();
    assert!(!c["adopted_at"].is_null() && c["last_applied_at"].is_null());
    let keys: std::collections::BTreeSet<&String> = c["drift"].as_object().unwrap().keys().collect();
    assert_eq!(
        keys.into_iter().collect::<Vec<_>>(),
        ["edited_outside", "pending_changes"]
    );
    assert!(matches!(
        c["status"].as_str(),
        Some("never_applied" | "pending_changes")
    ));
    let d = get(&a, &format!("{API}/clients/fake1/drift"), &h).await.json();
    let keys: std::collections::BTreeSet<&String> = d["drift"].as_object().unwrap().keys().collect();
    assert!(keys.into_iter().collect::<Vec<_>>() == ["edited_outside", "pending_changes"] && d.get("files").is_some());
}
