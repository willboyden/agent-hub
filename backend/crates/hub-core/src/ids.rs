//! domain/ids.py: identifier rules shared by every content kind. Always a FULL match (Python `fullmatch`).
//! Owner: CORE (LOCKED).

use std::sync::LazyLock;

use regex::Regex;

macro_rules! full {
    ($name:ident, $re:expr) => {
        pub static $name: LazyLock<Regex> = LazyLock::new(|| crate::re(concat!("^(?:", $re, ")$")));
    };
}

full!(ID_RE, r"[a-z0-9][a-z0-9._-]{0,63}");
full!(ENV_NAME_RE, r"[A-Z_][A-Z0-9_]{0,127}");
full!(HOST_RE, r"(\*\.)?[A-Za-z0-9*]([A-Za-z0-9.*-]{0,251}[A-Za-z0-9*])?");
full!(COMMIT_RE, r"[0-9a-f]{7,40}");

pub const KINDS: [&str; 6] = ["skill", "agent", "instruction", "mcp", "rule", "memory"];
pub const CONCERNS: [&str; 6] = ["skills", "agents", "instructions", "mcp", "permissions", "memory"];

/// PLURAL: kind -> its collection name.
pub fn plural(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "skill" => "skills",
        "agent" => "agents",
        "instruction" => "instructions",
        "mcp" => "mcp",
        "rule" => "rules",
        "memory" => "memory",
        _ => return None,
    })
}

/// FROM_PLURAL.
pub fn from_plural(plural: &str) -> Option<&'static str> {
    KINDS.into_iter().find(|k| self::plural(k) == Some(plural))
}

/// CONCERN_OF_KIND: which manage flag governs each item kind.
pub fn concern_of_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "skill" => "skills",
        "agent" => "agents",
        "instruction" => "instructions",
        "mcp" => "mcp",
        "rule" => "permissions",
        "memory" => "memory",
        _ => return None,
    })
}

/// DEFAULT_MANAGE (ARCHITECTURE decision 5), in CONCERNS order.
pub fn default_manage(concern: &str) -> Option<bool> {
    Some(match concern {
        "skills" | "agents" | "instructions" => true,
        "mcp" | "permissions" | "memory" => false,
        _ => return None,
    })
}

pub fn valid_id(value: &str) -> bool {
    ID_RE.is_match(value)
}

/// `check_ident`: the pydantic AfterValidator message is "Value error, " + this.
pub const IDENT_MESSAGE: &str = "must match ^[a-z0-9][a-z0-9._-]{0,63}$";

/// `slugify(text)`.
pub fn slugify(text: &str) -> String {
    static NON: LazyLock<Regex> = LazyLock::new(|| crate::re(r"[^a-z0-9._-]+"));
    let lowered = text.to_lowercase();
    let s = NON.replace_all(&lowered, "-");
    let s = s.trim_matches(|c| matches!(c, '-' | '.' | '_'));
    let s = crate::py::text::take_chars(s, 64);
    s.trim_matches(|c| matches!(c, '-' | '.' | '_')).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_rules() {
        assert!(valid_id("a"));
        assert!(valid_id("a.b_c-d"));
        assert!(!valid_id("A"));
        assert!(!valid_id("-a"));
        assert!(!valid_id("a\n"));
        assert!(!valid_id(&"a".repeat(65)));
        assert!(HOST_RE.is_match("*.example.com"));
        assert!(!HOST_RE.is_match("a b"));
        assert_eq!(slugify("  Hello, World!.."), "hello-world");
    }
}
