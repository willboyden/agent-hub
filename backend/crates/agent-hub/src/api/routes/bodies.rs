//! The request body models of api/routers/*.py (all `extra="forbid"`), validated with hub_core::pyd so 422 errors match
//! FastAPI/pydantic exactly (golden: hub-core/tests/golden/models.json). Owner: CORE (LOCKED).

use std::sync::LazyLock;

use hub_core::models::{KIND_LITS, member};
use hub_core::pyd::{Extra, Lit, Model, Ty, optional, required};
use hub_core::value::PyValue;

fn none() -> PyValue {
    PyValue::None
}
fn yes() -> PyValue {
    PyValue::Bool(true)
}
fn no() -> PyValue {
    PyValue::Bool(false)
}
fn empty_list() -> PyValue {
    PyValue::List(Vec::new())
}

const CONFLICT: &[Lit] = &[Lit::S("skip"), Lit::S("rename"), Lit::S("replace"), Lit::S("link")];
const TIERS: &[Lit] = &[Lit::S("static"), Lit::S("tier1"), Lit::S("tier1+2")];

fn strict(name: &'static str, fields: Vec<hub_core::pyd::Field>) -> Model {
    Model {
        name,
        fields,
        extra: Extra::Forbid,
        check: None,
        transform: None,
    }
}

macro_rules! body_model {
    ($static:ident, $getter:ident, $model:expr) => {
        pub static $static: LazyLock<Model> = LazyLock::new(|| $model);
        pub fn $getter() -> &'static Model {
            &$static
        }
    };
}

// system.py
body_model!(
    KEY_IN,
    key_in,
    strict(
        "KeyIn",
        vec![
            required("name", Ty::str()),
            optional("role", Ty::str(), || PyValue::str("viewer")),
            optional("client", Ty::opt(Ty::str()), none),
            optional("scopes", Ty::list(Ty::str()), empty_list),
        ]
    )
);

// clients.py
body_model!(
    IMPORT_ITEM,
    import_item,
    strict(
        "ImportItem",
        vec![
            required("kind", Ty::str()),
            required("name", Ty::str()),
            optional("on_conflict", Ty::opt(Ty::str()), none),
        ]
    )
);
body_model!(
    IMPORT_IN,
    import_in,
    strict(
        "ImportIn",
        vec![
            required("client", Ty::str()),
            optional("items", Ty::opt(Ty::list(Ty::Model(import_item))), none),
            optional("on_conflict", Ty::opt(Ty::str()), none),
            optional("enable", Ty::Bool, yes),
            optional("override_reason", Ty::opt(Ty::str()), none),
        ]
    )
);
body_model!(
    OBSERVED_ITEM,
    observed_item,
    strict(
        "ObservedItem",
        vec![required("key", Ty::str()), optional("name", Ty::opt(Ty::str()), none),]
    )
);
body_model!(
    OBSERVED_IMPORT_IN,
    observed_import_in,
    strict(
        "ObservedImportIn",
        vec![
            required("items", Ty::list(Ty::Model(observed_item))),
            optional("on_conflict", Ty::Lit(CONFLICT), || PyValue::str("skip")),
            optional("enable", Ty::Bool, no),
            optional("override_reason", Ty::opt(Ty::str()), none),
        ]
    )
);
body_model!(
    ROLLBACK_IN,
    rollback_in,
    strict(
        "RollbackIn",
        vec![
            optional("apply_id", Ty::opt(Ty::str()), none),
            optional("confirm", Ty::Bool, no),
            optional("dry_run", Ty::Bool, no),
            optional("force_paths", Ty::list(Ty::str()), empty_list),
        ]
    )
);

// content.py
body_model!(
    DUPLICATE_IN,
    duplicate_in,
    strict("DuplicateIn", vec![required("new_name", Ty::str())])
);
body_model!(
    INBOX_IN,
    inbox_in,
    strict(
        "InboxIn",
        vec![
            required("client", Ty::str()),
            required("title", Ty::str()),
            required("body", Ty::str()),
            optional("type", Ty::str(), || PyValue::str("reference")),
        ]
    )
);

// organize.py
body_model!(
    MEMBERS_IN,
    members_in,
    strict("MembersIn", vec![required("members", Ty::list(Ty::Model(member)))])
);
body_model!(
    REORDER_IN,
    reorder_in,
    strict("ReorderIn", vec![required("ids", Ty::list(Ty::str()))])
);
body_model!(
    BULK_IN,
    bulk_in,
    strict(
        "BulkIn",
        vec![
            required("items", Ty::list(Ty::Model(member))),
            optional("add_to_collection", Ty::opt(Ty::str()), none),
            optional("remove_from_collection", Ty::opt(Ty::str()), none),
            optional("add_tags", Ty::list(Ty::str()), empty_list),
            optional("remove_tags", Ty::list(Ty::str()), empty_list),
        ]
    )
);
body_model!(
    TOGGLE_IN,
    toggle_in,
    strict(
        "ToggleIn",
        vec![
            required("client", Ty::str()),
            required("kind", Ty::Lit(KIND_LITS)),
            required("name", Ty::str()),
            required("enabled", Ty::Bool),
        ]
    )
);
body_model!(
    BULK_TOGGLE_IN,
    bulk_toggle_in,
    strict(
        "BulkToggleIn",
        vec![
            required("clients", Ty::list(Ty::str())),
            required("items", Ty::list(Ty::Model(member))),
            required("enabled", Ty::Bool),
        ]
    )
);
body_model!(
    ACCEPT_IN,
    accept_in,
    strict(
        "AcceptIn",
        vec![
            required("ids", Ty::list(Ty::str())),
            optional("overrides", Ty::dict(Ty::str(), Ty::dict(Ty::str(), Ty::Any)), || {
                PyValue::Dict(Vec::new())
            }),
        ]
    )
);

// changes.py
body_model!(
    COMMIT_IN,
    commit_in,
    strict("CommitIn", vec![required("message", Ty::str())])
);
body_model!(
    DISCARD_IN,
    discard_in,
    strict(
        "DiscardIn",
        vec![
            optional("paths", Ty::opt(Ty::list(Ty::str())), none),
            optional("confirm", Ty::Bool, no),
            optional("dry_run", Ty::Bool, no),
        ]
    )
);
body_model!(
    REVERT_IN,
    revert_in,
    strict("RevertIn", vec![required("commit", Ty::str())])
);
body_model!(
    PLAN_IN,
    plan_in,
    strict(
        "PlanIn",
        vec![
            optional("clients", Ty::opt(Ty::list(Ty::str())), none),
            optional("source", Ty::str(), || PyValue::str("committed")),
        ]
    )
);
body_model!(
    APPLY_IN,
    apply_in,
    strict(
        "ApplyIn",
        vec![
            required("plan_id", Ty::str()),
            optional("confirm", Ty::Bool, no),
            optional("adopt_paths", Ty::list(Ty::str()), empty_list),
        ]
    )
);

// skilleval.py
body_model!(
    EVALUATE_IN,
    evaluate_in,
    strict(
        "EvaluateIn",
        vec![optional("tiers", Ty::Lit(TIERS), || PyValue::str("static"))]
    )
);
body_model!(
    RUN_IN,
    run_in,
    strict(
        "RunIn",
        vec![
            optional("subjects", Ty::opt(Ty::list(Ty::str())), none),
            optional(
                "select",
                Ty::opt(Ty::Lit(&[Lit::S("none_or_stale"), Lit::S("all")])),
                none
            ),
            optional("source", Ty::str(), || PyValue::str("hub")),
            optional("tiers", Ty::Lit(TIERS), || PyValue::str("static")),
            optional("force", Ty::Bool, no),
        ]
    )
);

/// Body models by pydantic class name (golden tests).
pub fn by_name(name: &str) -> Option<&'static Model> {
    Some(match name {
        "KeyIn" => key_in(),
        "ImportIn" => import_in(),
        "ObservedImportIn" => observed_import_in(),
        "RollbackIn" => rollback_in(),
        "DuplicateIn" => duplicate_in(),
        "InboxIn" => inbox_in(),
        "MembersIn" => members_in(),
        "ReorderIn" => reorder_in(),
        "BulkIn" => bulk_in(),
        "ToggleIn" => toggle_in(),
        "BulkToggleIn" => bulk_toggle_in(),
        "AcceptIn" => accept_in(),
        "CommitIn" => commit_in(),
        "DiscardIn" => discard_in(),
        "RevertIn" => revert_in(),
        "PlanIn" => plan_in(),
        "ApplyIn" => apply_in(),
        "EvaluateIn" => evaluate_in(),
        "RunIn" => run_in(),
        _ => return None,
    })
}

/// Accessors over a validated body.
pub fn s(v: &PyValue, k: &str) -> String {
    v.get(k).and_then(PyValue::as_str).unwrap_or_default().to_string()
}
pub fn os(v: &PyValue, k: &str) -> Option<String> {
    v.get(k).and_then(PyValue::as_str).map(str::to_string)
}
pub fn b(v: &PyValue, k: &str) -> bool {
    matches!(v.get(k), Some(PyValue::Bool(true)))
}
pub fn l(v: &PyValue, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(PyValue::as_list)
        .map(|x| x.iter().map(PyValue::py_str).collect())
        .unwrap_or_default()
}
pub fn ol(v: &PyValue, k: &str) -> Option<Vec<String>> {
    v.get(k)
        .and_then(PyValue::as_list)
        .map(|x| x.iter().map(PyValue::py_str).collect())
}
pub fn members(v: &PyValue, k: &str) -> Vec<hub_core::models::Member> {
    v.get(k)
        .and_then(PyValue::as_list)
        .map(|x| {
            x.iter()
                .map(|m| hub_core::models::Member {
                    kind: s(m, "kind"),
                    name: s(m, "name"),
                })
                .collect()
        })
        .unwrap_or_default()
}
