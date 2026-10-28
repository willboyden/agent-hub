//! Port of filters.py: the metadata filter grammar, validated into an AST; backends translate the AST, never the raw
//! input.
//!
//! ```text
//! filter  := {"$and": [filter,..]} | {"$or": [filter,..]} | {"$not": filter} | {field: cond, ...}   (multi-key = AND)
//! cond    := scalar (equality) | {"$eq"|"$ne"|"$gt"|"$gte"|"$lt"|"$lte": scalar} | {"$in": [scalar,..]}
//! scalar  := string (<=512 chars) | int | float | bool
//! ```
//! Field names fullmatch `[A-Za-z_][A-Za-z0-9_]{0,63}`. Depth <= 4, <= `max_nodes` nodes, <= 64 values per `$in`.
//! [`evaluate`] is the reference semantics (memory store, the LanceDB id-resolution step) with Python's comparison
//! rules: `bool` never equals a number, ints and floats compare exactly (also beyond 2^53 and for big ints), a
//! comparison that raises `TypeError` (str vs number, None, containers) or involves NaN is false.

use std::cmp::Ordering;
use std::sync::LazyLock;

use hub_core::PyValue;
use regex::Regex;

use crate::pyjson::obj;

pub static FIELD: LazyLock<Regex> = LazyLock::new(|| hub_core::re(r"^[A-Za-z_][A-Za-z0-9_]{0,63}$"));
pub const MAX_DEPTH: usize = 4;
pub const MAX_NODES: usize = 32;
pub const MAX_IN: usize = 64;
pub const MAX_STR: usize = 512;

/// `FilterError` (a ValueError); the message is shown to the client as the 422 detail.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FilterError(pub String);

fn ferr<T>(msg: &str) -> Result<T, FilterError> {
    Err(FilterError(msg.to_string()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

impl Op {
    pub fn name(self) -> &'static str {
        match self {
            Op::Eq => "eq",
            Op::Ne => "ne",
            Op::Gt => "gt",
            Op::Gte => "gte",
            Op::Lt => "lt",
            Op::Lte => "lte",
        }
    }

    fn from_operator(op: &str) -> Option<Op> {
        Some(match op {
            "$eq" => Op::Eq,
            "$ne" => Op::Ne,
            "$gt" => Op::Gt,
            "$gte" => Op::Gte,
            "$lt" => Op::Lt,
            "$lte" => Op::Lte,
            _ => return None,
        })
    }
}

/// The validated AST (`Cmp | In | And | Or | Not`). Values are scalars: `Bool`, `Int`, `BigInt`, `Float`, `Str`.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Cmp { field: String, op: Op, value: PyValue },
    In { field: String, values: Vec<PyValue> },
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
}

impl Node {
    /// `Cmp(field, "eq", value)` for a string value (the doc_id replace/delete path).
    pub fn eq_str(field: &str, value: &str) -> Node {
        Node::Cmp {
            field: field.into(),
            op: Op::Eq,
            value: PyValue::str(value),
        }
    }
}

/// `_scalar(v)`.
fn scalar(v: &PyValue) -> Result<PyValue, FilterError> {
    match v {
        PyValue::Bool(_) | PyValue::Int(_) | PyValue::BigInt(_) | PyValue::Float(_) => Ok(v.clone()),
        PyValue::Str(s) if s.chars().count() <= MAX_STR => Ok(v.clone()),
        _ => ferr("filter values must be string (<=512 chars), number or boolean"),
    }
}

/// `parse_filter(raw, max_nodes)`.
pub fn parse_filter(raw: &PyValue, max_nodes: usize) -> Result<Node, FilterError> {
    let mut count = 0usize;
    walk(raw, 1, &mut count, max_nodes)
}

fn walk(node: &PyValue, depth: usize, count: &mut usize, max_nodes: usize) -> Result<Node, FilterError> {
    *count += 1;
    if depth > MAX_DEPTH || *count > max_nodes {
        return ferr("filter too deep or too large");
    }
    let Some(items) = node.as_dict().filter(|d| !d.is_empty()) else {
        return ferr("a filter must be a non-empty object");
    };
    let mut parts = Vec::new();
    for (key, val) in items {
        let k = key.as_str();
        if k == Some("$and") || k == Some("$or") {
            let Some(list) = val.as_list().filter(|l| !l.is_empty()) else {
                return Err(FilterError(format!("{} needs a non-empty list", k.unwrap_or_default())));
            };
            let mut kids = Vec::with_capacity(list.len());
            for v in list {
                kids.push(walk(v, depth + 1, count, max_nodes)?);
            }
            parts.push(if k == Some("$and") {
                Node::And(kids)
            } else {
                Node::Or(kids)
            });
        } else if k == Some("$not") {
            parts.push(Node::Not(Box::new(walk(val, depth + 1, count, max_nodes)?)));
        } else if let Some(field) = k.filter(|f| FIELD.is_match(f)) {
            *count += 1;
            if let Some(cond) = val.as_dict() {
                if cond.len() != 1 {
                    return ferr("one operator per field condition");
                }
                let (op, arg) = &cond[0];
                if op.as_str() == Some("$in") {
                    let Some(list) = arg.as_list().filter(|l| !l.is_empty() && l.len() <= MAX_IN) else {
                        return ferr("$in needs a list of 1..64 values");
                    };
                    let values = list.iter().map(scalar).collect::<Result<Vec<_>, _>>()?;
                    parts.push(Node::In {
                        field: field.into(),
                        values,
                    });
                } else if let Some(o) = op.as_str().and_then(Op::from_operator) {
                    parts.push(Node::Cmp {
                        field: field.into(),
                        op: o,
                        value: scalar(arg)?,
                    });
                } else {
                    return ferr("unknown operator");
                }
            } else {
                parts.push(Node::Cmp {
                    field: field.into(),
                    op: Op::Eq,
                    value: scalar(val)?,
                });
            }
        } else {
            return ferr("invalid field name or operator");
        }
    }
    Ok(if parts.len() == 1 {
        parts.remove(0)
    } else {
        Node::And(parts)
    })
}

// ------------------------------------------------------------------------------------------------ Python numbers

/// An exact Python number (bool excluded by the callers where Python excludes it).
enum Num<'a> {
    I(i128),
    /// beyond i128, canonical decimal text (`-?[1-9][0-9]*`)
    Big(&'a str),
    F(f64),
}

fn num(v: &PyValue) -> Option<Num<'_>> {
    match v {
        PyValue::Bool(b) => Some(Num::I(i128::from(*b))),
        PyValue::Int(i) => Some(Num::I(i128::from(*i))),
        PyValue::BigInt(s) => Some(s.parse::<i128>().map_or(Num::Big(s.as_str()), Num::I)),
        PyValue::Float(f) => Some(Num::F(*f)),
        _ => None,
    }
}

/// Ordering of two decimal integers given as text (`-?digits`, no leading zeros).
fn cmp_dec(a: &str, b: &str) -> Ordering {
    let (na, da) = a.strip_prefix('-').map_or((false, a), |d| (true, d));
    let (nb, db) = b.strip_prefix('-').map_or((false, b), |d| (true, d));
    let mag = da.len().cmp(&db.len()).then_with(|| da.cmp(db));
    match (na, nb) {
        (false, false) => mag,
        (true, true) => mag.reverse(),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
    }
}

/// 2^127 as f64: every finite float of at least this magnitude is an integer beyond i128.
const TWO_127: f64 = 170_141_183_460_469_231_731_687_303_715_884_105_728.0;

/// Exact `int <=> float`.
fn cmp_i_f(i: i128, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f >= TWO_127 {
        return Some(Ordering::Less);
    }
    if f < -TWO_127 {
        return Some(Ordering::Greater);
    }
    let t = f.trunc();
    #[allow(clippy::cast_possible_truncation)]
    let ti = t as i128;
    Some(i.cmp(&ti).then_with(|| {
        let frac = f - t;
        if frac > 0.0 {
            Ordering::Less
        } else if frac < 0.0 {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }))
}

/// Exact `big int <=> float` (a big int is beyond i128).
fn cmp_big_f(b: &str, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f.is_infinite() {
        return Some(if f > 0.0 { Ordering::Less } else { Ordering::Greater });
    }
    if f.abs() < TWO_127 {
        return Some(if b.starts_with('-') {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    // |f| >= 2^127 is an integer: compare its exact decimal expansion
    let text = format!("{f:.0}");
    Some(cmp_dec(b, &text))
}

fn cmp_num(a: &Num<'_>, b: &Num<'_>) -> Option<Ordering> {
    match (a, b) {
        (Num::I(x), Num::I(y)) => Some(x.cmp(y)),
        (Num::F(x), Num::F(y)) => x.partial_cmp(y),
        (Num::I(x), Num::F(y)) => cmp_i_f(*x, *y),
        (Num::F(x), Num::I(y)) => cmp_i_f(*y, *x).map(Ordering::reverse),
        (Num::Big(x), Num::Big(y)) => Some(cmp_dec(x, y)),
        (Num::Big(x), Num::I(y)) => Some(cmp_dec(x, &y.to_string())),
        (Num::I(x), Num::Big(y)) => Some(cmp_dec(&x.to_string(), y)),
        (Num::Big(x), Num::F(y)) => cmp_big_f(x, *y),
        (Num::F(x), Num::Big(y)) => cmp_big_f(y, *x).map(Ordering::reverse),
    }
}

/// Python `a == b` for the values a filter can see (scalars on the right; anything JSON on the left).
pub fn py_eq(a: &PyValue, b: &PyValue) -> bool {
    if let (Some(x), Some(y)) = (num(a), num(b)) {
        return cmp_num(&x, &y) == Some(Ordering::Equal);
    }
    match (a, b) {
        (PyValue::Str(x), PyValue::Str(y)) => x == y,
        (PyValue::None, PyValue::None) => true,
        (PyValue::List(x), PyValue::List(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q)),
        (PyValue::Dict(x), PyValue::Dict(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.iter().any(|(k2, v2)| py_eq(k, k2) && py_eq(v, v2)))
        }
        _ => false,
    }
}

/// `_eq(a, b)`: a bool never equals a non-bool.
fn eq_(a: &PyValue, b: &PyValue) -> bool {
    if matches!(a, PyValue::Bool(_)) != matches!(b, PyValue::Bool(_)) {
        return false;
    }
    py_eq(a, b)
}

/// Python `a < b` & co. as an ordering; `None` where Python raises TypeError or NaN is involved.
fn py_order(a: &PyValue, b: &PyValue) -> Option<Ordering> {
    if let (Some(x), Some(y)) = (num(a), num(b)) {
        return cmp_num(&x, &y);
    }
    match (a, b) {
        (PyValue::Str(x), PyValue::Str(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn lookup<'a>(meta: &'a PyValue, field: &str) -> Option<&'a PyValue> {
    meta.as_dict()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(field))
        .map(|(_, v)| v)
}

/// `evaluate(node, meta)`.
pub fn evaluate(node: &Node, meta: &PyValue) -> bool {
    match node {
        Node::And(items) => items.iter().all(|i| evaluate(i, meta)),
        Node::Or(items) => items.iter().any(|i| evaluate(i, meta)),
        Node::Not(item) => !evaluate(item, meta),
        Node::In { field, values } => lookup(meta, field).is_some_and(|have| values.iter().any(|v| eq_(have, v))),
        Node::Cmp { field, op, value } => {
            let Some(have) = lookup(meta, field) else {
                return *op == Op::Ne;
            };
            match op {
                Op::Eq => eq_(have, value),
                Op::Ne => !eq_(have, value),
                _ => {
                    if matches!(have, PyValue::Bool(_)) || matches!(value, PyValue::Bool(_)) {
                        return false;
                    }
                    let Some(o) = py_order(have, value) else { return false };
                    match op {
                        Op::Gt => o == Ordering::Greater,
                        Op::Gte => o != Ordering::Less,
                        Op::Lt => o == Ordering::Less,
                        _ => o != Ordering::Greater,
                    }
                }
            }
        }
    }
}

/// `to_qdrant(node)`: Qdrant's structured JSON filter; field keys are regex-validated, values sit in typed slots.
pub fn to_qdrant(node: &Node) -> Result<PyValue, FilterError> {
    fn key(f: &str) -> PyValue {
        PyValue::Str(format!("meta.{f}"))
    }
    fn matcher(field: &str, v: &PyValue) -> PyValue {
        obj([("key", key(field)), ("match", obj([("value", v.clone())]))])
    }
    fn cmp_(field: &str, op: Op, value: &PyValue) -> Result<PyValue, FilterError> {
        if matches!(op, Op::Eq | Op::Ne) {
            if matches!(value, PyValue::Float(_)) {
                return ferr("qdrant cannot match floats exactly; use a range operator");
            }
            let cond = matcher(field, value);
            return Ok(if op == Op::Eq {
                cond
            } else {
                obj([("must_not", PyValue::List(vec![cond]))])
            });
        }
        if matches!(value, PyValue::Bool(_) | PyValue::Str(_)) {
            return ferr("range operators need a number");
        }
        Ok(obj([("key", key(field)), ("range", obj([(op.name(), value.clone())]))]))
    }
    fn go(n: &Node) -> Result<PyValue, FilterError> {
        Ok(match n {
            Node::Cmp { field, op, value } => obj([("must", PyValue::List(vec![cmp_(field, *op, value)?]))]),
            Node::In { field, values } => {
                if values.iter().any(|v| matches!(v, PyValue::Float(_))) {
                    return ferr("qdrant cannot match floats exactly; use a range operator");
                }
                obj([(
                    "should",
                    PyValue::List(values.iter().map(|v| matcher(field, v)).collect()),
                )])
            }
            Node::And(items) => obj([("must", PyValue::List(items.iter().map(go).collect::<Result<_, _>>()?))]),
            Node::Or(items) => obj([("should", PyValue::List(items.iter().map(go).collect::<Result<_, _>>()?))]),
            Node::Not(item) => obj([("must_not", PyValue::List(vec![go(item)?]))]),
        })
    }
    go(node)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_number_comparisons() {
        let big = PyValue::Int(9_007_199_254_740_993); // 2**53 + 1
        let f = PyValue::Float(9_007_199_254_740_992.0);
        assert!(!py_eq(&big, &f));
        assert_eq!(py_order(&big, &f), Some(Ordering::Greater));
        assert!(py_eq(&PyValue::Int(1), &PyValue::Float(1.0)));
        assert!(!eq_(&PyValue::Bool(true), &PyValue::Int(1)));
        assert_eq!(py_order(&PyValue::Float(f64::NAN), &PyValue::Int(1)), None);
        // 10**30 < 1e30 (1e30 is 1000000000000000019884624838656)
        let p30 = PyValue::BigInt(format!("1{}", "0".repeat(30)));
        assert_eq!(py_order(&p30, &PyValue::Float(1e30)), Some(Ordering::Less));
        let p40 = PyValue::BigInt(format!("1{}", "0".repeat(40)));
        assert_eq!(py_order(&p40, &PyValue::Float(1e30)), Some(Ordering::Greater));
        assert_eq!(py_order(&PyValue::str("a"), &PyValue::Int(1)), None);
        assert_eq!(
            py_order(&PyValue::Float(2.5), &PyValue::Int(2)),
            Some(Ordering::Greater)
        );
        assert_eq!(py_order(&PyValue::Int(-3), &PyValue::Float(-2.5)), Some(Ordering::Less));
    }
}
