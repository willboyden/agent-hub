//! `hub_core::py::float` against Python: `repr(float)`, `json.dumps(float)` and pydantic-core's float JSON on 3000
//! values, 1900 of them exact ties between two shortest decimal candidates (both CPython and pydantic-core pick the
//! even one). Golden file: tests/golden/floats.json from backend/parity/gen/gen_float_golden.py.

use hub_core::py::float;

#[test]
fn float_formats_match_python_including_ties() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/floats.json");
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let cases = doc["cases"].as_array().ok_or("cases")?;
    assert!(cases.len() >= 3000);
    let mut bad = Vec::new();
    for c in cases {
        let bits = u64::from_le_bytes(hex::decode(c["bits"].as_str().ok_or("bits")?)?.as_slice().try_into()?);
        let x = f64::from_bits(bits);
        for (got, key) in [
            (float::repr(x), "repr"),
            (float::json(x), "json"),
            (float::pydantic_json(x), "pydantic"),
        ] {
            if got != c[key].as_str().unwrap_or_default() {
                bad.push(format!("{key}({x:e}): {got} != {}", c[key]));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} mismatches, e.g. {:?}",
        bad.len(),
        &bad[..bad.len().min(5)]
    );
    Ok(())
}
