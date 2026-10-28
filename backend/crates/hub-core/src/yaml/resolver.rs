//! PyYAML 6.0.3 `resolver.py`: the YAML 1.1 implicit resolvers, tried in registration order for the scalar's first
//! character (`yes`/`on` booleans, `0`-octal and sexagesimal ints, `.inf`, `1:30.5` floats, `<<` merge, `=` value,
//! `~`/`null`/empty null, timestamps). Python's `$` also matches before a final newline; the patterns use `\n?\z`.

use std::sync::LazyLock;

use regex::Regex;

pub(crate) const STR: &str = "tag:yaml.org,2002:str";
pub(crate) const SEQ: &str = "tag:yaml.org,2002:seq";
pub(crate) const MAP: &str = "tag:yaml.org,2002:map";
pub(crate) const NULL: &str = "tag:yaml.org,2002:null";
pub(crate) const BOOL: &str = "tag:yaml.org,2002:bool";
pub(crate) const INT: &str = "tag:yaml.org,2002:int";
pub(crate) const FLOAT: &str = "tag:yaml.org,2002:float";
pub(crate) const BINARY: &str = "tag:yaml.org,2002:binary";
pub(crate) const TIMESTAMP: &str = "tag:yaml.org,2002:timestamp";
pub(crate) const MERGE: &str = "tag:yaml.org,2002:merge";
pub(crate) const VALUE: &str = "tag:yaml.org,2002:value";
pub(crate) const SET: &str = "tag:yaml.org,2002:set";
pub(crate) const OMAP: &str = "tag:yaml.org,2002:omap";
pub(crate) const PAIRS: &str = "tag:yaml.org,2002:pairs";
const YAML: &str = "tag:yaml.org,2002:yaml";

static RE_BOOL: LazyLock<Regex> = LazyLock::new(|| {
    crate::re(r"^(?:yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF)\n?\z")
});
static RE_FLOAT: LazyLock<Regex> = LazyLock::new(|| {
    crate::re(concat!(
        r"^(?:[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?",
        r"|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?",
        r"|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*",
        r"|[-+]?\.(?:inf|Inf|INF)",
        r"|\.(?:nan|NaN|NAN))\n?\z"
    ))
});
static RE_INT: LazyLock<Regex> = LazyLock::new(|| {
    crate::re(concat!(
        r"^(?:[-+]?0b[0-1_]+",
        r"|[-+]?0[0-7_]+",
        r"|[-+]?(?:0|[1-9][0-9_]*)",
        r"|[-+]?0x[0-9a-fA-F_]+",
        r"|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+)\n?\z"
    ))
});
static RE_MERGE: LazyLock<Regex> = LazyLock::new(|| crate::re(r"^(?:<<)\n?\z"));
static RE_NULL: LazyLock<Regex> = LazyLock::new(|| crate::re(r"^(?:~|null|Null|NULL|)\n?\z"));
static RE_TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    crate::re(concat!(
        r"^(?:[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]",
        r"|[0-9][0-9][0-9][0-9]-[0-9][0-9]?-[0-9][0-9]?",
        r"(?:[Tt]|[ \t]+)[0-9][0-9]?",
        r":[0-9][0-9]:[0-9][0-9](?:\.[0-9]*)?",
        r"(?:[ \t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?)\n?\z"
    ))
});
static RE_VALUE: LazyLock<Regex> = LazyLock::new(|| crate::re(r"^(?:=)\n?\z"));
static RE_YAML: LazyLock<Regex> = LazyLock::new(|| crate::re(r"^(?:!|&|\*)\n?\z"));

#[derive(Clone, Copy)]
enum Res {
    Bool,
    Float,
    Int,
    Merge,
    Null,
    Timestamp,
    Value,
    Yaml,
}

impl Res {
    fn tag_and_re(self) -> (&'static str, &'static Regex) {
        match self {
            Res::Bool => (BOOL, &RE_BOOL),
            Res::Float => (FLOAT, &RE_FLOAT),
            Res::Int => (INT, &RE_INT),
            Res::Merge => (MERGE, &RE_MERGE),
            Res::Null => (NULL, &RE_NULL),
            Res::Timestamp => (TIMESTAMP, &RE_TIMESTAMP),
            Res::Value => (VALUE, &RE_VALUE),
            Res::Yaml => (YAML, &RE_YAML),
        }
    }
}

/// `yaml_implicit_resolvers[first]` in registration order.
fn resolvers_for(first: Option<char>) -> &'static [Res] {
    match first {
        None | Some('~') => &[Res::Null],
        Some('y' | 'Y' | 't' | 'T' | 'f' | 'F' | 'o' | 'O') => &[Res::Bool],
        Some('n' | 'N') => &[Res::Bool, Res::Null],
        Some('-' | '+') => &[Res::Float, Res::Int],
        Some('0'..='9') => &[Res::Float, Res::Int, Res::Timestamp],
        Some('.') => &[Res::Float],
        Some('<') => &[Res::Merge],
        Some('=') => &[Res::Value],
        Some('!' | '&' | '*') => &[Res::Yaml],
        Some(_) => &[],
    }
}

/// `resolve(ScalarNode, value, implicit)` (only `implicit[0]` matters for the result).
pub(crate) fn resolve_scalar(value: &str, implicit_plain: bool) -> String {
    if implicit_plain {
        for r in resolvers_for(value.chars().next()) {
            let (tag, re) = r.tag_and_re();
            if re.is_match(value) {
                return tag.to_string();
            }
        }
    }
    STR.to_string()
}

/// The timestamp constructor's own pattern (1-2 digit month/day, optional time).
pub(crate) static RE_TS_CONSTRUCT: LazyLock<Regex> = LazyLock::new(|| {
    crate::re(concat!(
        r"^(?P<year>[0-9][0-9][0-9][0-9])",
        r"-(?P<month>[0-9][0-9]?)",
        r"-(?P<day>[0-9][0-9]?)",
        r"(?:(?:[Tt]|[ \t]+)",
        r"(?P<hour>[0-9][0-9]?)",
        r":(?P<minute>[0-9][0-9])",
        r":(?P<second>[0-9][0-9])",
        r"(?:\.(?P<fraction>[0-9]*))?",
        r"(?:[ \t]*(?P<tz>Z|(?P<tz_sign>[-+])(?P<tz_hour>[0-9][0-9]?)",
        r"(?::(?P<tz_minute>[0-9][0-9]))?))?)?\n?\z"
    ))
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml11_resolution() {
        let r = |v: &str| resolve_scalar(v, true);
        assert_eq!(r("yes"), BOOL);
        assert_eq!(r("No"), BOOL);
        assert_eq!(r("null"), NULL);
        assert_eq!(r(""), NULL);
        assert_eq!(r("~"), NULL);
        assert_eq!(r("012"), INT);
        assert_eq!(r("1:30"), INT);
        assert_eq!(r("1_000"), INT);
        assert_eq!(r("1e5"), STR);
        assert_eq!(r("1.0e+5"), FLOAT);
        assert_eq!(r("1:30.5"), FLOAT);
        assert_eq!(r(".inf"), FLOAT);
        assert_eq!(r("2024-01-02"), TIMESTAMP);
        assert_eq!(r("2024-1-2"), STR);
        assert_eq!(r("2024-1-2 3:04:05"), TIMESTAMP);
        assert_eq!(r("<<"), MERGE);
        assert_eq!(r("="), VALUE);
        assert_eq!(r("12\n"), INT);
        assert_eq!(resolve_scalar("12", false), STR);
    }
}
