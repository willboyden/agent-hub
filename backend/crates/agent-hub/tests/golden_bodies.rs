//! API body models (api/routes/bodies.rs) against pydantic 2.13.5 (hub-core/tests/golden/models.json).

use agent_hub::api::routes::bodies::by_name;
use hub_core::pyd;
use hub_core::value::PyValue;
use serde_json::Value;

#[test]
fn body_models_match_pydantic() -> Result<(), Box<dyn std::error::Error>> {
    let golden: Vec<Value> = serde_json::from_str(include_str!("../../hub-core/tests/golden/models.json"))?;
    let mut checked = 0;
    let mut failures = Vec::new();
    for case in &golden {
        let name = case["model"].as_str().unwrap_or_default();
        let Some(model) = by_name(name) else { continue };
        checked += 1;
        match (
            case["ok"].as_bool(),
            pyd::validate(model, &PyValue::from_json(&case["input"])),
        ) {
            (Some(true), Ok(v)) => {
                if pyd::dump_json(model, &v, false) != case["dump"] {
                    failures.push(format!(
                        "{name} {}: dump {}",
                        case["input"],
                        pyd::dump_json(model, &v, false)
                    ));
                }
            }
            (Some(false), Err(e)) => {
                if e.errors_json(&[]) != case["errors"] {
                    failures.push(format!("{name} {}: errors {}", case["input"], e.errors_json(&[])));
                }
            }
            (want, got) => failures.push(format!(
                "{name} {}: want ok={want:?} got {:?}",
                case["input"],
                got.map(|_| ())
            )),
        }
    }
    assert!(checked >= 20, "only {checked} body cases");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}
