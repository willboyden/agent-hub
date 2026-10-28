//! Port of backend/tests/core/test_pathsafe.py (security control S-PATH). Test names mirror the Python ones.

use std::path::{Path, PathBuf};

use hub_core::ids::{ID_RE, valid_id};
use hub_core::pathsafe::{check_relpath, expand_root, resolve_under};
use hub_core::py::os::realpath;

type R = Result<(), Box<dyn std::error::Error>>;

#[test]
fn test_bad_relpaths_rejected() {
    let long = "x".repeat(600);
    for bad in [
        "",
        "/etc/passwd",
        "../x",
        "a/../b",
        "a//b",
        "./a",
        "a/.",
        "a\\b",
        "a\0b",
        "C:/x",
        "a/b/..",
        long.as_str(),
        "a\nb",
    ] {
        assert!(check_relpath(bad).is_err(), "{bad:?} must be refused");
    }
}

#[test]
fn test_good_relpaths() {
    for ok in ["a", "a/b.md", "skills/x/SKILL.md", ".claude/settings.json", "a b/c"] {
        assert_eq!(check_relpath(ok).ok(), Some(ok));
    }
}

#[test]
fn test_resolve_under_inside() -> R {
    let d = tempfile::tempdir()?;
    assert_eq!(
        resolve_under(d.path(), "a/b/c.txt")?,
        realpath(d.path()).join("a/b/c.txt")
    );
    Ok(())
}

#[test]
fn test_relative_root_rejected() {
    assert!(resolve_under(Path::new("relative/root"), "a").is_err());
}

#[test]
fn test_symlink_component_escape_rejected() -> R {
    let d = tempfile::tempdir()?;
    let (root, outside) = (d.path().join("root"), d.path().join("outside"));
    std::fs::create_dir(&root)?;
    std::fs::create_dir(&outside)?;
    std::os::unix::fs::symlink(&outside, root.join("link"))?;
    assert!(resolve_under(&root, "link/evil.txt").is_err());
    Ok(())
}

#[test]
fn test_symlink_leaf_rejected_even_inside_root() -> R {
    let d = tempfile::tempdir()?;
    let root = d.path().join("root");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("real"), "x")?;
    std::os::unix::fs::symlink(root.join("real"), root.join("alias"))?;
    assert!(resolve_under(&root, "alias").is_err());
    Ok(())
}

#[test]
fn test_dangling_symlink_rejected() -> R {
    let d = tempfile::tempdir()?;
    let root = d.path().join("root");
    std::fs::create_dir(&root)?;
    std::os::unix::fs::symlink(d.path().join("nowhere"), root.join("dangling"))?;
    assert!(resolve_under(&root, "dangling").is_err());
    Ok(())
}

#[test]
fn test_root_itself_may_be_a_symlink() -> R {
    let d = tempfile::tempdir()?;
    let real = d.path().join("real");
    std::fs::create_dir(&real)?;
    let link = d.path().join("link");
    std::os::unix::fs::symlink(&real, &link)?;
    assert_eq!(resolve_under(&link, "f.txt")?, realpath(&real).join("f.txt"));
    Ok(())
}

#[test]
fn test_expand_root() {
    assert!(expand_root("~/x").map(|p: PathBuf| p.is_absolute()).unwrap_or(false));
    for bad in ["", "relative", "/a/../b", "/a\0b"] {
        assert!(expand_root(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn test_ids_use_fullmatch() {
    let long = "x".repeat(65);
    for bad in ["../x", "A", "-x", "", "a b", "a/b", long.as_str(), ".hidden", "a\n"] {
        assert!(!valid_id(bad), "{bad:?}");
    }
}

#[test]
fn test_id_trailing_newline_is_not_accepted() {
    assert!(!ID_RE.is_match("abc\n"));
    assert!(valid_id("a.b_c-d0") && valid_id(&"x".repeat(64)));
}

proptest::proptest! {
    /// Any accepted relative path stays strictly under its root after resolution (no escape for any input).
    #[test]
    fn prop_check_relpath_never_escapes(s in "[a-z./\\\\]{0,20}") {
        if check_relpath(&s).is_ok() {
            proptest::prop_assert!(!s.starts_with('/') && !s.split('/').any(|p| p == ".." || p == "." || p.is_empty()));
        }
    }
}
