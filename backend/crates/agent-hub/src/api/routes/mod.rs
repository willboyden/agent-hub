//! The route table, in Python's exact match order (backend/parity/routes.python.json; tests/routes.rs proves equality),
//! and the handler helpers. Every handler is written here in full (CORE); a handler whose service is not ported yet
//! answers 501 `not_ported` through the stub. Owner: CORE (LOCKED). A later route is an ADDITIVE addendum.

pub mod bodies;
pub mod changes;
pub mod clients;
pub mod content;
pub mod knowledge;
pub mod organize;
pub mod skilleval;
pub mod system;

use std::sync::Arc;

use axum::body::Body;
use http::Response;
use hub_core::{HubResult, Problem};
use serde_json::Value;

use super::errors::{json, no_content, problem};
use super::{AppState, Route};
use crate::services::hub::Hub;

pub type Resp = Response<Body>;

/// Run a blocking hub operation off the async runtime (Python runs sync endpoints in a thread pool).
pub async fn run<T: Send + 'static>(
    s: &Arc<AppState>,
    f: impl FnOnce(&Hub) -> HubResult<T> + Send + 'static,
) -> HubResult<T> {
    let hub = s.hub.clone();
    match tokio::task::spawn_blocking(move || f(&hub)).await {
        Ok(r) => r,
        Err(_) => Err(Problem::internal()),
    }
}

/// 200/201/202 JSON or the problem.
pub fn reply(status: u16, r: HubResult<Value>) -> Resp {
    match r {
        Ok(v) => json(status, &v),
        Err(p) => problem(&p),
    }
}

/// 204 or the problem.
pub fn reply_empty(r: HubResult<()>) -> Resp {
    match r {
        Ok(()) => no_content(),
        Err(p) => problem(&p),
    }
}

/// `{"items": [...], "next_cursor": None}`.
pub fn items(v: Vec<Value>) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("items".into(), Value::Array(v));
    m.insert("next_cursor".into(), Value::Null);
    Value::Object(m)
}

macro_rules! h {
    ($f:path) => {
        |s, r| Box::pin($f(s, r))
    };
}

/// Every route, in Starlette match order (index = `order` in routes.python.json).
pub fn table() -> Vec<Route> {
    use Route as R;
    const G: &[&str] = &["GET"];
    const GH: &[&str] = &["GET", "HEAD"];
    const P: &[&str] = &["POST"];
    const U: &[&str] = &["PUT"];
    const D: &[&str] = &["DELETE"];
    const KN: &[&str] = &["DELETE", "GET", "POST"];
    vec![
        R::new(
            GH,
            "/api/v1/openapi.json",
            "/api/v1/openapi.json",
            h!(system::openapi),
            false,
        ),
        R::new(
            G,
            "/api/v1/memory/inbox",
            "/api/v1/memory/inbox",
            h!(content::list_inbox),
            false,
        ),
        R::new(
            P,
            "/api/v1/memory/inbox",
            "/api/v1/memory/inbox",
            h!(content::add_inbox),
            true,
        ),
        R::new(
            P,
            "/api/v1/memory/inbox/{iid}/promote",
            "/api/v1/memory/inbox/{iid}/promote",
            h!(content::promote),
            true,
        ),
        R::new(
            P,
            "/api/v1/memory/inbox/{iid}/reject",
            "/api/v1/memory/inbox/{iid}/reject",
            h!(content::reject),
            false,
        ),
        R::new(G, "/api/v1/health", "/api/v1/health", h!(system::health), false),
        R::new(G, "/api/v1/adapters", "/api/v1/adapters", h!(system::adapters), false),
        R::new(
            G,
            "/api/v1/adapters/{adapter_id}",
            "/api/v1/adapters/{adapter_id}",
            h!(system::adapter),
            false,
        ),
        R::new(
            G,
            "/api/v1/policy/floor",
            "/api/v1/policy/floor",
            h!(system::floor),
            false,
        ),
        R::new(
            P,
            "/api/v1/policy/check",
            "/api/v1/policy/check",
            h!(system::policy_check),
            true,
        ),
        R::new(G, "/api/v1/audit", "/api/v1/audit", h!(system::audit), false),
        R::new(
            G,
            "/api/v1/settings",
            "/api/v1/settings",
            h!(system::get_settings),
            false,
        ),
        R::new(
            U,
            "/api/v1/settings",
            "/api/v1/settings",
            h!(system::put_settings),
            true,
        ),
        R::new(G, "/api/v1/keys", "/api/v1/keys", h!(system::list_keys), false),
        R::new(P, "/api/v1/keys", "/api/v1/keys", h!(system::create_key), true),
        R::new(
            D,
            "/api/v1/keys/{key_id}",
            "/api/v1/keys/{key_id}",
            h!(system::delete_key),
            false,
        ),
        R::new(G, "/api/v1/doctor", "/api/v1/doctor", h!(system::doctor), false),
        R::new(
            G,
            "/api/v1/doctor/summary",
            "/api/v1/doctor/summary",
            h!(system::doctor_summary),
            false,
        ),
        R::new(
            G,
            "/api/v1/content/validate",
            "/api/v1/content/validate",
            h!(system::validate),
            false,
        ),
        R::new(
            G,
            "/api/v1/clients",
            "/api/v1/clients",
            h!(clients::list_clients),
            false,
        ),
        R::new(
            P,
            "/api/v1/clients",
            "/api/v1/clients",
            h!(clients::create_client),
            true,
        ),
        R::new(
            P,
            "/api/v1/clients/validate-spec",
            "/api/v1/clients/validate-spec",
            h!(clients::validate_spec),
            true,
        ),
        R::new(
            G,
            "/api/v1/clients/{cid}",
            "/api/v1/clients/{cid}",
            h!(clients::get_client),
            false,
        ),
        R::new(
            U,
            "/api/v1/clients/{cid}",
            "/api/v1/clients/{cid}",
            h!(clients::put_client),
            true,
        ),
        R::new(
            D,
            "/api/v1/clients/{cid}",
            "/api/v1/clients/{cid}",
            h!(clients::delete_client),
            false,
        ),
        R::new(
            P,
            "/api/v1/clients/{cid}/discover",
            "/api/v1/clients/{cid}/discover",
            h!(clients::discover),
            false,
        ),
        R::new(
            P,
            "/api/v1/clients/{cid}/observe",
            "/api/v1/clients/{cid}/observe",
            h!(clients::observe),
            false,
        ),
        R::new(
            G,
            "/api/v1/clients/{cid}/observed",
            "/api/v1/clients/{cid}/observed",
            h!(clients::observed),
            false,
        ),
        R::new(
            P,
            "/api/v1/clients/{cid}/observed/import",
            "/api/v1/clients/{cid}/observed/import",
            h!(clients::observed_import),
            true,
        ),
        R::new(P, "/api/v1/import", "/api/v1/import", h!(clients::do_import), true),
        R::new(
            G,
            "/api/v1/clients/{cid}/rendered",
            "/api/v1/clients/{cid}/rendered",
            h!(clients::rendered),
            false,
        ),
        R::new(
            G,
            "/api/v1/clients/{cid}/applies",
            "/api/v1/clients/{cid}/applies",
            h!(clients::applies),
            false,
        ),
        R::new(
            P,
            "/api/v1/clients/{cid}/rollback",
            "/api/v1/clients/{cid}/rollback",
            h!(clients::rollback),
            true,
        ),
        R::new(
            G,
            "/api/v1/clients/{cid}/effective",
            "/api/v1/clients/{cid}/effective",
            h!(clients::effective),
            false,
        ),
        R::new(
            P,
            "/api/v1/clients/{cid}/verify",
            "/api/v1/clients/{cid}/verify",
            h!(clients::verify),
            false,
        ),
        R::new(
            G,
            "/api/v1/clients/{cid}/drift",
            "/api/v1/clients/{cid}/drift",
            h!(clients::drift),
            false,
        ),
        R::new(
            G,
            "/api/v1/clients/{cid}/audit",
            "/api/v1/clients/{cid}/audit",
            h!(clients::audit_client),
            false,
        ),
        R::new(G, "/api/v1/skills", "/api/v1/skills", h!(content::list_skills), false),
        R::new(
            G,
            "/api/v1/skills/{name}",
            "/api/v1/skills/{name}",
            h!(content::get_skill),
            false,
        ),
        R::new(
            U,
            "/api/v1/skills/{name}",
            "/api/v1/skills/{name}",
            h!(content::put_skill),
            true,
        ),
        R::new(
            D,
            "/api/v1/skills/{name}",
            "/api/v1/skills/{name}",
            h!(content::delete_skill),
            false,
        ),
        R::new(G, "/api/v1/agents", "/api/v1/agents", h!(content::list_agents), false),
        R::new(
            G,
            "/api/v1/agents/{name}",
            "/api/v1/agents/{name}",
            h!(content::get_agent),
            false,
        ),
        R::new(
            U,
            "/api/v1/agents/{name}",
            "/api/v1/agents/{name}",
            h!(content::put_agent),
            true,
        ),
        R::new(
            D,
            "/api/v1/agents/{name}",
            "/api/v1/agents/{name}",
            h!(content::delete_agent),
            false,
        ),
        R::new(
            G,
            "/api/v1/instructions",
            "/api/v1/instructions",
            h!(content::list_instructions),
            false,
        ),
        R::new(
            G,
            "/api/v1/instructions/{name}",
            "/api/v1/instructions/{name}",
            h!(content::get_instruction),
            false,
        ),
        R::new(
            U,
            "/api/v1/instructions/{name}",
            "/api/v1/instructions/{name}",
            h!(content::put_instruction),
            true,
        ),
        R::new(
            D,
            "/api/v1/instructions/{name}",
            "/api/v1/instructions/{name}",
            h!(content::delete_instruction),
            false,
        ),
        R::new(G, "/api/v1/mcp", "/api/v1/mcp", h!(content::list_mcp), false),
        R::new(
            G,
            "/api/v1/mcp/{name}",
            "/api/v1/mcp/{name}",
            h!(content::get_mcp),
            false,
        ),
        R::new(
            U,
            "/api/v1/mcp/{name}",
            "/api/v1/mcp/{name}",
            h!(content::put_mcp),
            true,
        ),
        R::new(
            D,
            "/api/v1/mcp/{name}",
            "/api/v1/mcp/{name}",
            h!(content::delete_mcp),
            false,
        ),
        R::new(G, "/api/v1/rules", "/api/v1/rules", h!(content::list_rules), false),
        R::new(
            G,
            "/api/v1/rules/{name}",
            "/api/v1/rules/{name}",
            h!(content::get_rule),
            false,
        ),
        R::new(
            U,
            "/api/v1/rules/{name}",
            "/api/v1/rules/{name}",
            h!(content::put_rule),
            true,
        ),
        R::new(
            D,
            "/api/v1/rules/{name}",
            "/api/v1/rules/{name}",
            h!(content::delete_rule),
            false,
        ),
        R::new(G, "/api/v1/memory", "/api/v1/memory", h!(content::list_memory), false),
        R::new(
            G,
            "/api/v1/memory/{name}",
            "/api/v1/memory/{name}",
            h!(content::get_memory),
            false,
        ),
        R::new(
            U,
            "/api/v1/memory/{name}",
            "/api/v1/memory/{name}",
            h!(content::put_memory),
            true,
        ),
        R::new(
            D,
            "/api/v1/memory/{name}",
            "/api/v1/memory/{name}",
            h!(content::delete_memory),
            false,
        ),
        R::new(
            P,
            "/api/v1/skills/{name}/duplicate",
            "/api/v1/skills/{name}/duplicate",
            h!(content::duplicate),
            true,
        ),
        R::new(
            G,
            "/api/v1/collections",
            "/api/v1/collections",
            h!(organize::list_collections),
            false,
        ),
        R::new(
            G,
            "/api/v1/collections/suggestions",
            "/api/v1/collections/suggestions",
            h!(organize::suggestions),
            false,
        ),
        R::new(
            P,
            "/api/v1/collections/suggestions/accept",
            "/api/v1/collections/suggestions/accept",
            h!(organize::accept_suggestions),
            true,
        ),
        R::new(
            P,
            "/api/v1/collections",
            "/api/v1/collections",
            h!(organize::create_collection),
            true,
        ),
        R::new(
            P,
            "/api/v1/collections/reorder",
            "/api/v1/collections/reorder",
            h!(organize::reorder),
            true,
        ),
        R::new(
            U,
            "/api/v1/collections/{cid}",
            "/api/v1/collections/{cid}",
            h!(organize::put_collection),
            true,
        ),
        R::new(
            D,
            "/api/v1/collections/{cid}",
            "/api/v1/collections/{cid}",
            h!(organize::delete_collection),
            false,
        ),
        R::new(
            P,
            "/api/v1/collections/{cid}/members",
            "/api/v1/collections/{cid}/members",
            h!(organize::add_members),
            true,
        ),
        R::new(
            D,
            "/api/v1/collections/{cid}/members/{kind}/{name}",
            "/api/v1/collections/{cid}/members/{kind}/{name}",
            h!(organize::remove_member),
            false,
        ),
        R::new(P, "/api/v1/items/bulk", "/api/v1/items/bulk", h!(organize::bulk), true),
        R::new(
            G,
            "/api/v1/profiles/{client}",
            "/api/v1/profiles/{client}",
            h!(organize::get_profile),
            false,
        ),
        R::new(
            U,
            "/api/v1/profiles/{client}",
            "/api/v1/profiles/{client}",
            h!(organize::put_profile),
            true,
        ),
        R::new(G, "/api/v1/matrix", "/api/v1/matrix", h!(organize::matrix), false),
        R::new(
            P,
            "/api/v1/matrix/toggle",
            "/api/v1/matrix/toggle",
            h!(organize::toggle),
            true,
        ),
        R::new(
            P,
            "/api/v1/matrix/bulk",
            "/api/v1/matrix/bulk",
            h!(organize::bulk_toggle),
            true,
        ),
        R::new(G, "/api/v1/changes", "/api/v1/changes", h!(changes::changes), false),
        R::new(
            P,
            "/api/v1/changes/commit",
            "/api/v1/changes/commit",
            h!(changes::commit),
            true,
        ),
        R::new(
            P,
            "/api/v1/changes/discard",
            "/api/v1/changes/discard",
            h!(changes::discard),
            true,
        ),
        R::new(
            G,
            "/api/v1/changes/history",
            "/api/v1/changes/history",
            h!(changes::history),
            false,
        ),
        R::new(
            P,
            "/api/v1/changes/revert",
            "/api/v1/changes/revert",
            h!(changes::revert),
            true,
        ),
        R::new(P, "/api/v1/plan", "/api/v1/plan", h!(changes::make_plan), true),
        R::new(
            G,
            "/api/v1/plan/{plan_id}",
            "/api/v1/plan/{plan_id}",
            h!(changes::get_plan),
            false,
        ),
        R::new(P, "/api/v1/apply", "/api/v1/apply", h!(changes::apply), true),
        R::new(
            KN,
            "/api/v1/knowledge/{path:path}",
            "/api/v1/knowledge/{path:path}",
            h!(knowledge::proxy),
            false,
        ),
        R::new(
            G,
            "/api/v1/skill-evaluator",
            "/api/v1/skill-evaluator",
            h!(skilleval::evaluator_status),
            false,
        ),
        R::new(
            G,
            "/api/v1/skill-evaluations",
            "/api/v1/skill-evaluations",
            h!(skilleval::list_evaluations),
            false,
        ),
        R::new(
            P,
            "/api/v1/skill-evaluations/run",
            "/api/v1/skill-evaluations/run",
            h!(skilleval::run_evaluations),
            true,
        ),
        R::new(
            G,
            "/api/v1/skill-evaluations/queue",
            "/api/v1/skill-evaluations/queue",
            h!(skilleval::evaluation_queue),
            false,
        ),
        R::new(
            P,
            "/api/v1/skill-evaluations/queue/cancel",
            "/api/v1/skill-evaluations/queue/cancel",
            h!(skilleval::cancel_queue),
            false,
        ),
        R::new(
            G,
            "/api/v1/skill-evaluations/{subject}",
            "/api/v1/skill-evaluations/{subject}",
            h!(skilleval::evaluation_detail),
            false,
        ),
        R::new(
            G,
            "/api/v1/skill-evaluations/{subject}/runs/{run_id}/report",
            "/api/v1/skill-evaluations/{subject}/runs/{run_id}/report",
            h!(skilleval::subject_report),
            false,
        ),
        R::new(
            G,
            "/api/v1/skills/{name}/evaluations",
            "/api/v1/skills/{name}/evaluations",
            h!(skilleval::skill_evaluations),
            false,
        ),
        R::new(
            G,
            "/api/v1/skills/{name}/evaluations/{run_id}/report",
            "/api/v1/skills/{name}/evaluations/{run_id}/report",
            h!(skilleval::evaluation_report),
            false,
        ),
        R::new(
            P,
            "/api/v1/skills/{name}/evaluate",
            "/api/v1/skills/{name}/evaluate",
            h!(skilleval::evaluate),
            true,
        ),
        R::new(G, "/metrics", "/metrics", h!(system::metrics), false),
        R::new(
            G,
            "/{path:path}",
            "/{path:path}",
            h!(crate::api::static_files::spa),
            false,
        ),
    ]
}

/// The metric `route` label Python reports for a matched route template (`/api/v1` + the un-prefixed template for
/// included routers; the catch-all reports `/api/v1/{path:path}` for /api/v1 paths).
pub fn metric_label(template: &str, path: &str) -> String {
    if template == "/{path:path}" && path.starts_with("/api/v1") {
        return "/api/v1/{path:path}".into();
    }
    // FastAPI's OpenAPI document is a plain Starlette Route, which does not put itself in `scope["route"]` (only
    // APIRoute does), so the middleware counts it like an unmatched API path
    if template == "/api/v1/openapi.json" {
        return "unmatched".into();
    }
    template.to_string()
}

/// An empty 501 helper kept for handlers whose Python endpoint has no service behind it yet.
pub fn not_ported(module: &str) -> Resp {
    problem(&Problem::not_ported(module))
}

/// `Ok(())` -> 204.
pub fn empty(r: HubResult<()>) -> Resp {
    reply_empty(r)
}
