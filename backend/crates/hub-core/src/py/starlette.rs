//! `starlette.routing.compile_path`: `{name}` -> `[^/]+`, `{name:path}` -> `.*`, everything else literal; the regex is
//! matched against the percent-decoded path. Shared by the hub and the knowledge service routers.

use regex::Regex;

/// (anchored regex, parameter names in order).
pub fn compile_path(path: &str) -> (Regex, Vec<String>) {
    let mut re = String::from("^");
    let mut names = Vec::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        re.push_str(&regex::escape(&rest[..start]));
        let Some(end) = rest[start..].find('}') else { break };
        let inner = &rest[start + 1..start + end];
        let (name, conv) = inner.split_once(':').unwrap_or((inner, "str"));
        let pat = if conv == "path" { ".*" } else { "[^/]+" };
        re.push_str(&format!("(?P<{name}>{pat})"));
        names.push(name.to_string());
        rest = &rest[start + end + 1..];
    }
    re.push_str(&regex::escape(rest));
    re.push('$');
    (crate::re(&re), names)
}

#[cfg(test)]
mod tests {
    use super::compile_path;

    #[test]
    fn params_and_path_convertor() {
        let (re, names) = compile_path("/api/v1/clients/{cid}/observed");
        assert_eq!(names, vec!["cid".to_string()]);
        assert!(re.is_match("/api/v1/clients/a.b/observed"));
        assert!(!re.is_match("/api/v1/clients/a/b/observed"));
        let (re, _) = compile_path("/api/v1/knowledge/{path:path}");
        assert!(re.is_match("/api/v1/knowledge/a/b/c"));
        assert!(re.is_match("/api/v1/knowledge/"));
    }
}
