//! backend/tests/core/test_yamlsafe.py, test by test (security control S-YAML: no Python tags, no anchors/aliases, no
//! duplicate keys, size/depth/node caps, frontmatter split/join).

use hub_core::PyValue;
use hub_core::yamlsafe::{self, DEFAULT_MAX_BYTES, YamlError};

fn s(x: &str) -> PyValue {
    PyValue::str(x)
}

fn load(text: &str) -> Result<PyValue, YamlError> {
    yamlsafe::load(text, DEFAULT_MAX_BYTES)
}

#[test]
fn test_loads_plain() {
    assert_eq!(
        load("a: 1\nb: [x, y]\n"),
        Ok(PyValue::Dict(vec![
            (s("a"), PyValue::Int(1)),
            (s("b"), PyValue::List(vec![s("x"), s("y")])),
        ]))
    );
}

#[test]
fn test_billion_laughs_rejected() {
    let bomb = "a: &a [x, x]\nb: &b [*a, *a]\nc: &c [*b, *b]\nd: [*c, *c]\n";
    assert_eq!(load(bomb), Err(YamlError("YAML anchors are not allowed".into())));
}

#[test]
fn test_anchor_alone_rejected() {
    assert_eq!(load("a: &x 1\n"), Err(YamlError("YAML anchors are not allowed".into())));
    // an alias without an anchor is refused by the strict loader before PyYAML's own "undefined alias" error
    assert_eq!(load("a: *x\n"), Err(YamlError("YAML aliases are not allowed".into())));
}

#[test]
fn test_python_tags_rejected() {
    assert_eq!(
        load("a: !!python/object/apply:os.system ['echo hi']\n"),
        Err(YamlError(
            "invalid YAML: could not determine a constructor for the tag 'tag:yaml.org,2002:python/object/apply:os.system'"
                .into()
        ))
    );
}

#[test]
fn test_duplicate_keys_rejected() {
    assert_eq!(load("a: 1\na: 2\n"), Err(YamlError("duplicate key: 'a'".into())));
    // Python equality: 1, 1.0 and true are the same key
    assert_eq!(load("1: a\ntrue: b\n"), Err(YamlError("duplicate key: True".into())));
}

#[test]
fn test_too_large_and_too_deep() {
    let big = format!("a: {}", "x".repeat(300_000));
    assert_eq!(load(&big), Err(YamlError("document too large".into())));
    let deep = format!("{}{}", "[".repeat(30), "]".repeat(30));
    assert_eq!(load(&deep), Err(YamlError("document nested too deeply".into())));
}

#[test]
fn test_too_many_nodes() {
    let many = format!("[{}]", vec!["1"; 6000].join(","));
    assert_eq!(load(&many), Err(YamlError("document has too many nodes".into())));
}

#[test]
fn test_frontmatter_roundtrip_and_errors() {
    let meta = vec![(s("name"), s("x")), (s("description"), s("a: b"))];
    let text = yamlsafe::join_frontmatter(&meta, "body\n---\nstill body\n");
    assert_eq!(text, "---\nname: x\ndescription: 'a: b'\n---\nbody\n---\nstill body\n");
    let (got, body) = yamlsafe::split_frontmatter(&text).unwrap_or_default();
    assert_eq!(PyValue::Dict(got), PyValue::Dict(meta));
    assert_eq!(body, "body\n---\nstill body\n");
    for (bad, msg) in [
        ("no frontmatter", "missing YAML frontmatter"),
        ("---\nname: x\n", "unterminated YAML frontmatter"),
        ("---\n- a\n- b\n---\nbody", "frontmatter must be a mapping"),
    ] {
        assert_eq!(yamlsafe::split_frontmatter(bad), Err(YamlError(msg.into())));
    }
    assert_eq!(
        yamlsafe::split_frontmatter("---\n---\nbody"),
        Ok((Vec::new(), "body".into()))
    );
}
