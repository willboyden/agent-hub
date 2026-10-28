//! PyYAML 6.0.3 `constructor.py` (`BaseConstructor` + `SafeConstructor`) and domain/yamlsafe.py
//! `_StrictLoader.construct_mapping`. Construction order is PyYAML's: collections are created empty and filled later
//! by their "state generators" (breadth-first waves), except while `deep_construct` is on (the strict loader builds
//! every mapping key deeply); so a document with two errors reports the same one Python does. Objects live in an
//! arena so aliases share one object (as in Python); the result is then turned into a [`PyValue`] tree.

use std::collections::{HashMap, HashSet};

use super::composer::{Node, NodeId, NodeVal};
use super::error::{Error, Mark};
use super::pyint::{self, BigUint};
use super::pyobj::{HashKey, unhashable_name};
use super::resolver as tags;
use crate::value::{DateTime, PyValue};

type ObjId = usize;
type R<T> = Result<T, Error>;

#[derive(Debug, Clone)]
enum Obj {
    Leaf(PyValue),
    List(Vec<ObjId>),
    /// Insertion-ordered; keys unique under Python `==`.
    Dict(Vec<(ObjId, ObjId)>),
    Set(Vec<ObjId>),
    /// `!!omap` / `!!pairs` items are 2-tuples (kept as 2-item lists in the PyValue: DEVIATION D-YAML-TUPLE).
    Tuple(ObjId, ObjId),
}

#[derive(Debug, Clone, Copy)]
enum Gen {
    Seq(NodeId, ObjId),
    Map(NodeId, ObjId),
    Set(NodeId, ObjId),
    Omap(NodeId, ObjId, bool),
}

/// Materialised values beyond this many (alias expansion in a non-strict load) are refused instead of exhausting
/// memory: Python shares the objects, a PyValue tree copies them (DEVIATION D-YAML-ALIAS-EXPANSION).
const MAX_MATERIALISED: usize = 1_000_000;

pub(crate) struct Constructor<'a> {
    nodes: &'a mut [Node],
    objs: Vec<Obj>,
    constructed: HashMap<NodeId, ObjId>,
    recursive: HashSet<NodeId>,
    pending: Vec<Gen>,
    deep_construct: bool,
    strict: bool,
}

fn ctor_err(context: Option<&str>, context_mark: Option<Mark>, problem: String, mark: Mark) -> Error {
    Error::marked(context, context_mark, problem, Some(mark))
}

impl<'a> Constructor<'a> {
    pub(crate) fn new(nodes: &'a mut [Node], strict: bool) -> Self {
        Self {
            nodes,
            objs: Vec::new(),
            constructed: HashMap::new(),
            recursive: HashSet::new(),
            pending: Vec::new(),
            deep_construct: false,
            strict,
        }
    }

    /// `construct_document(node)`.
    pub(crate) fn construct_document(&mut self, node: NodeId) -> R<PyValue> {
        let data = self.construct_object(node, false)?;
        while !self.pending.is_empty() {
            let gens = std::mem::take(&mut self.pending);
            for g in gens {
                self.run(g)?;
            }
        }
        self.materialise(data)
    }

    fn new_obj(&mut self, o: Obj) -> ObjId {
        self.objs.push(o);
        self.objs.len() - 1
    }

    fn construct_object(&mut self, node: NodeId, deep: bool) -> R<ObjId> {
        if let Some(&o) = self.constructed.get(&node) {
            return Ok(o);
        }
        let old_deep = self.deep_construct;
        if deep {
            self.deep_construct = true;
        }
        if self.recursive.contains(&node) {
            return Err(Error::marked(
                None,
                None,
                "found unconstructable recursive node",
                Some(self.nodes[node].start),
            ));
        }
        self.recursive.insert(node);
        let tag = self.nodes[node].tag.clone();
        let obj = match tag.as_str() {
            tags::SEQ | tags::MAP | tags::SET | tags::OMAP | tags::PAIRS => {
                let (o, g) = match tag.as_str() {
                    tags::SEQ => {
                        let o = self.new_obj(Obj::List(Vec::new()));
                        (o, Gen::Seq(node, o))
                    }
                    tags::MAP => {
                        let o = self.new_obj(Obj::Dict(Vec::new()));
                        (o, Gen::Map(node, o))
                    }
                    tags::SET => {
                        let o = self.new_obj(Obj::Set(Vec::new()));
                        (o, Gen::Set(node, o))
                    }
                    _ => {
                        let o = self.new_obj(Obj::List(Vec::new()));
                        (o, Gen::Omap(node, o, tag == tags::OMAP))
                    }
                };
                if self.deep_construct {
                    self.run(g)?;
                } else {
                    self.pending.push(g);
                }
                o
            }
            _ => {
                let v = self.construct_leaf(node, &tag)?;
                self.new_obj(Obj::Leaf(v))
            }
        };
        self.constructed.insert(node, obj);
        self.recursive.remove(&node);
        if deep {
            self.deep_construct = old_deep;
        }
        Ok(obj)
    }

    fn run(&mut self, g: Gen) -> R<()> {
        match g {
            Gen::Seq(node, o) => {
                let NodeVal::Seq(children) = &self.nodes[node].val else {
                    return Err(Error::marked(
                        None,
                        None,
                        format!("expected a sequence node, but found {}", self.nodes[node].id()),
                        Some(self.nodes[node].start),
                    ));
                };
                let children = children.clone();
                let mut items = Vec::with_capacity(children.len());
                for c in children {
                    items.push(self.construct_object(c, false)?);
                }
                if let Obj::List(l) = &mut self.objs[o] {
                    l.extend(items);
                }
            }
            Gen::Map(node, o) => {
                let pairs = self.construct_mapping(node)?;
                if let Obj::Dict(d) = &mut self.objs[o] {
                    *d = pairs;
                }
            }
            Gen::Set(node, o) => {
                let pairs = self.construct_mapping(node)?;
                if let Obj::Set(s) = &mut self.objs[o] {
                    *s = pairs.into_iter().map(|(k, _)| k).collect();
                }
            }
            Gen::Omap(node, o, omap) => {
                let ctx = if omap {
                    "while constructing an ordered map"
                } else {
                    "while constructing pairs"
                };
                let start = self.nodes[node].start;
                let NodeVal::Seq(children) = &self.nodes[node].val else {
                    return Err(ctor_err(
                        Some(ctx),
                        Some(start),
                        format!("expected a sequence, but found {}", self.nodes[node].id()),
                        start,
                    ));
                };
                for sub in children.clone() {
                    let NodeVal::Map(sp) = &self.nodes[sub].val else {
                        return Err(ctor_err(
                            Some(ctx),
                            Some(start),
                            format!("expected a mapping of length 1, but found {}", self.nodes[sub].id()),
                            self.nodes[sub].start,
                        ));
                    };
                    if sp.len() != 1 {
                        return Err(ctor_err(
                            Some(ctx),
                            Some(start),
                            format!("expected a single mapping item, but found {} items", sp.len()),
                            self.nodes[sub].start,
                        ));
                    }
                    let (kn, vn) = sp[0];
                    let k = self.construct_object(kn, false)?;
                    let v = self.construct_object(vn, false)?;
                    let t = self.new_obj(Obj::Tuple(k, v));
                    if let Obj::List(l) = &mut self.objs[o] {
                        l.push(t);
                    }
                }
            }
        }
        Ok(())
    }

    /// The PyValue of a constructed hashable object (for keys).
    fn key_value(&self, o: ObjId) -> PyValue {
        match &self.objs[o] {
            Obj::Leaf(v) => v.clone(),
            Obj::List(_) | Obj::Tuple(..) => PyValue::List(Vec::new()),
            Obj::Dict(_) => PyValue::Dict(Vec::new()),
            Obj::Set(_) => PyValue::Set(Vec::new()),
        }
    }

    /// `construct_mapping(node)` as the loader in use defines it.
    fn construct_mapping(&mut self, node: NodeId) -> R<Vec<(ObjId, ObjId)>> {
        if self.strict {
            self.strict_key_check(node)?;
        }
        // SafeConstructor.construct_mapping
        if matches!(self.nodes[node].val, NodeVal::Map(_)) {
            self.flatten_mapping(node, 0)?;
        }
        // BaseConstructor.construct_mapping
        let NodeVal::Map(pairs) = &self.nodes[node].val else {
            return Err(Error::marked(
                None,
                None,
                format!("expected a mapping node, but found {}", self.nodes[node].id()),
                Some(self.nodes[node].start),
            ));
        };
        let pairs = pairs.clone();
        let mut mapping: Vec<(ObjId, ObjId)> = Vec::new();
        let mut index: HashMap<HashKey, usize> = HashMap::new();
        for (kn, vn) in pairs {
            let key = self.construct_object(kn, false)?;
            let hk = HashKey::of(&self.key_value(key)).filter(|_| matches!(self.objs[key], Obj::Leaf(_)));
            let Some(hk) = hk else {
                return Err(ctor_err(
                    Some("while constructing a mapping"),
                    Some(self.nodes[node].start),
                    "found unhashable key".into(),
                    self.nodes[kn].start,
                ));
            };
            let value = self.construct_object(vn, false)?;
            match index.get(&hk) {
                Some(&i) => mapping[i].1 = value,
                None => {
                    index.insert(hk, mapping.len());
                    mapping.push((key, value));
                }
            }
        }
        Ok(mapping)
    }

    /// `_StrictLoader.construct_mapping`'s loop: every key is built deeply, then refused if unhashable (`TypeError`
    /// from `key in seen`) or already seen ("duplicate key: <repr>").
    fn strict_key_check(&mut self, node: NodeId) -> R<()> {
        let key_nodes: Vec<NodeId> = match &self.nodes[node].val {
            NodeVal::Map(pairs) => pairs.iter().map(|(k, _)| *k).collect(),
            NodeVal::Seq(items) => {
                // `for key_node, _ in node.value`: unpacking a Node object
                if let Some(&first) = items.first() {
                    return Err(Error::python(
                        "TypeError",
                        format!("cannot unpack non-iterable {} object", self.nodes[first].class_name()),
                    ));
                }
                Vec::new()
            }
            NodeVal::Scalar { value, .. } => {
                // iterating a str yields 1-character strings
                if !value.is_empty() {
                    return Err(Error::python(
                        "ValueError",
                        "not enough values to unpack (expected 2, got 1)",
                    ));
                }
                Vec::new()
            }
        };
        let mut seen: HashSet<HashKey> = HashSet::new();
        for kn in key_nodes {
            let key = self.construct_object(kn, true)?;
            let v = self.key_value(key);
            let (Obj::Leaf(_), Some(hk)) = (&self.objs[key], HashKey::of(&v)) else {
                let name = match &self.objs[key] {
                    Obj::List(_) | Obj::Tuple(..) => "list",
                    Obj::Dict(_) => "dict",
                    Obj::Set(_) => "set",
                    Obj::Leaf(l) => unhashable_name(l),
                };
                return Err(Error::python("TypeError", format!("unhashable type: '{name}'")));
            };
            if !seen.insert(hk) {
                // `f"duplicate key: {key!r}"` (str keys through the exact CPython printable table)
                let r = match &v {
                    PyValue::Str(s) => super::pyunicode::repr(s),
                    other => other.repr(),
                };
                return Err(Error::Strict(format!("duplicate key: {r}")));
            }
        }
        Ok(())
    }

    /// `SafeConstructor.flatten_mapping` (merge keys `<<`, value keys `=`), mutating the node like PyYAML.
    fn flatten_mapping(&mut self, node: NodeId, depth: usize) -> R<()> {
        if depth > super::composer::MAX_DEPTH_SAFE {
            return Err(Error::python("RecursionError", "maximum recursion depth exceeded"));
        }
        let mut merge: Vec<(NodeId, NodeId)> = Vec::new();
        let mut index = 0;
        loop {
            let NodeVal::Map(pairs) = &self.nodes[node].val else {
                return Ok(());
            };
            if index >= pairs.len() {
                break;
            }
            let (kn, vn) = pairs[index];
            if self.nodes[kn].tag == tags::MERGE {
                if let NodeVal::Map(p) = &mut self.nodes[node].val {
                    p.remove(index);
                }
                match &self.nodes[vn].val {
                    NodeVal::Map(_) => {
                        self.flatten_mapping(vn, depth + 1)?;
                        if let NodeVal::Map(p) = &self.nodes[vn].val {
                            merge.extend(p.iter().copied());
                        }
                    }
                    NodeVal::Seq(subs) => {
                        let subs = subs.clone();
                        let mut submerge: Vec<Vec<(NodeId, NodeId)>> = Vec::new();
                        for sub in subs {
                            if !matches!(self.nodes[sub].val, NodeVal::Map(_)) {
                                return Err(ctor_err(
                                    Some("while constructing a mapping"),
                                    Some(self.nodes[node].start),
                                    format!("expected a mapping for merging, but found {}", self.nodes[sub].id()),
                                    self.nodes[sub].start,
                                ));
                            }
                            self.flatten_mapping(sub, depth + 1)?;
                            if let NodeVal::Map(p) = &self.nodes[sub].val {
                                submerge.push(p.clone());
                            }
                        }
                        for v in submerge.into_iter().rev() {
                            merge.extend(v);
                        }
                    }
                    NodeVal::Scalar { .. } => {
                        return Err(ctor_err(
                            Some("while constructing a mapping"),
                            Some(self.nodes[node].start),
                            format!(
                                "expected a mapping or list of mappings for merging, but found {}",
                                self.nodes[vn].id()
                            ),
                            self.nodes[vn].start,
                        ));
                    }
                }
            } else if self.nodes[kn].tag == tags::VALUE {
                self.nodes[kn].tag = tags::STR.to_string();
                index += 1;
            } else {
                index += 1;
            }
        }
        if !merge.is_empty()
            && let NodeVal::Map(p) = &mut self.nodes[node].val
        {
            merge.append(p);
            *p = merge;
        }
        Ok(())
    }

    // ---- scalars ---------------------------------------------------------------------------------------------

    /// `SafeConstructor.construct_scalar` (a mapping with a `=` value key yields that value's scalar).
    fn construct_scalar(&self, node: NodeId) -> R<String> {
        match &self.nodes[node].val {
            NodeVal::Scalar { value, .. } => Ok(value.clone()),
            NodeVal::Map(pairs) => {
                for (kn, vn) in pairs {
                    if self.nodes[*kn].tag == tags::VALUE {
                        return self.construct_scalar(*vn);
                    }
                }
                Err(self.not_scalar(node))
            }
            NodeVal::Seq(_) => Err(self.not_scalar(node)),
        }
    }

    fn not_scalar(&self, node: NodeId) -> Error {
        Error::marked(
            None,
            None,
            format!("expected a scalar node, but found {}", self.nodes[node].id()),
            Some(self.nodes[node].start),
        )
    }

    fn construct_leaf(&mut self, node: NodeId, tag: &str) -> R<PyValue> {
        let start = self.nodes[node].start;
        match tag {
            tags::NULL => {
                self.construct_scalar(node)?;
                Ok(PyValue::None)
            }
            tags::BOOL => {
                let v = self.construct_scalar(node)?;
                match v.to_lowercase().as_str() {
                    "yes" | "true" | "on" => Ok(PyValue::Bool(true)),
                    "no" | "false" | "off" => Ok(PyValue::Bool(false)),
                    other => Err(Error::python("KeyError", super::pyunicode::repr(other))),
                }
            }
            tags::INT => {
                let v = self.construct_scalar(node)?;
                construct_yaml_int(&v)
            }
            tags::FLOAT => {
                let v = self.construct_scalar(node)?;
                construct_yaml_float(&v)
            }
            tags::BINARY => {
                let v = self.construct_scalar(node)?;
                construct_yaml_binary(&v, start)
            }
            tags::TIMESTAMP => {
                self.construct_scalar(node)?;
                let NodeVal::Scalar { value, .. } = &self.nodes[node].val else {
                    // `timestamp_regexp.match(node.value)` on a mapping node's list
                    return Err(Error::python(
                        "TypeError",
                        "expected string or bytes-like object, got 'list'",
                    ));
                };
                construct_yaml_timestamp(value)
            }
            tags::STR => Ok(PyValue::Str(self.construct_scalar(node)?)),
            other => Err(Error::marked(
                None,
                None,
                format!(
                    "could not determine a constructor for the tag {}",
                    super::pyunicode::repr(other)
                ),
                Some(start),
            )),
        }
    }

    // ---- result ----------------------------------------------------------------------------------------------

    /// The object graph as a PyValue tree, built with an explicit stack (any depth is safe). Shared objects (aliases)
    /// are copied; a container that contains itself cannot be represented (DEVIATION D-YAML-RECURSIVE) and alias
    /// expansion is capped (DEVIATION D-YAML-ALIAS-EXPANSION).
    fn materialise(&self, root: ObjId) -> R<PyValue> {
        struct Frame {
            obj: ObjId,
            children: Vec<ObjId>,
            next: usize,
            built: Vec<PyValue>,
        }
        let mut budget = self.objs.len() + MAX_MATERIALISED;
        let children_of = |o: ObjId| -> Vec<ObjId> {
            match &self.objs[o] {
                Obj::Leaf(_) => Vec::new(),
                Obj::List(items) | Obj::Set(items) => items.clone(),
                Obj::Tuple(k, v) => vec![*k, *v],
                Obj::Dict(pairs) => pairs.iter().flat_map(|(k, v)| [*k, *v]).collect(),
            }
        };
        let finish = |o: ObjId, built: Vec<PyValue>| -> PyValue {
            match &self.objs[o] {
                Obj::Leaf(v) => v.clone(),
                Obj::List(_) | Obj::Tuple(..) => PyValue::List(built),
                Obj::Set(_) => PyValue::Set(built),
                Obj::Dict(_) => {
                    let mut it = built.into_iter();
                    let mut pairs = Vec::new();
                    while let (Some(k), Some(v)) = (it.next(), it.next()) {
                        pairs.push((k, v));
                    }
                    PyValue::Dict(pairs)
                }
            }
        };
        if let Obj::Leaf(v) = &self.objs[root] {
            return Ok(v.clone());
        }
        let mut stack = vec![Frame {
            obj: root,
            children: children_of(root),
            next: 0,
            built: Vec::new(),
        }];
        loop {
            let Some(top) = stack.last_mut() else {
                return Ok(PyValue::None);
            };
            if top.next < top.children.len() {
                let child = top.children[top.next];
                top.next += 1;
                if budget == 0 {
                    return Err(Error::python(
                        "MemoryError",
                        "document expands to too many values through aliases",
                    ));
                }
                budget -= 1;
                if let Obj::Leaf(v) = &self.objs[child] {
                    top.built.push(v.clone());
                    continue;
                }
                if stack.iter().any(|f| f.obj == child) {
                    return Err(Error::python(
                        "RecursionError",
                        "recursive YAML structure cannot be represented",
                    ));
                }
                stack.push(Frame {
                    obj: child,
                    children: children_of(child),
                    next: 0,
                    built: Vec::new(),
                });
                continue;
            }
            let Some(done) = stack.pop() else {
                return Ok(PyValue::None);
            };
            let value = finish(done.obj, done.built);
            match stack.last_mut() {
                Some(parent) => parent.built.push(value),
                None => return Ok(value),
            }
        }
    }
}

/// `SafeConstructor.construct_yaml_int`.
fn construct_yaml_int(value: &str) -> R<PyValue> {
    let mut value: String = value.replace('_', "");
    let Some(first) = value.chars().next() else {
        return Err(Error::python("IndexError", "string index out of range"));
    };
    let negative = first == '-';
    if first == '+' || first == '-' {
        value.remove(0);
    }
    if value == "0" {
        return Ok(PyValue::Int(0));
    }
    if value.is_empty() {
        // `elif value[0] == '0'` on the empty rest of a lone sign
        return Err(Error::python("IndexError", "string index out of range"));
    }
    let (neg2, mag) = if let Some(rest) = value.strip_prefix("0b") {
        pyint::py_int(rest, 2)?
    } else if let Some(rest) = value.strip_prefix("0x") {
        pyint::py_int(rest, 16)?
    } else if value.starts_with('0') {
        pyint::py_int(&value, 8)?
    } else if value.contains(':') {
        let mut digits = Vec::new();
        for part in value.split(':') {
            digits.push(pyint::py_int(part, 10)?);
        }
        // value = sum(digit * 60**k), with Python ints (each part may carry its own sign)
        let mut total_pos = BigUint::zero();
        let mut total_neg = BigUint::zero();
        let mut base = BigUint::zero();
        base.mul_add(1, 1);
        for (neg, d) in digits.iter().rev() {
            let term = mul_big(d, &base);
            if *neg {
                total_neg.add(&term);
            } else {
                total_pos.add(&term);
            }
            base = base.mul_small(60);
            if base.too_long() || total_pos.too_long() || total_neg.too_long() {
                return pyint::int_value(false, &base);
            }
        }
        let (neg, mag) = sub_signed(&total_pos, &total_neg);
        return pyint::int_value(negative != neg && !is_zero(&mag), &mag);
    } else {
        pyint::py_int(&value, 10)?
    };
    pyint::int_value(negative != neg2 && !is_zero(&mag), &mag)
}

fn is_zero(b: &BigUint) -> bool {
    b.to_decimal() == "0"
}

/// Schoolbook product through decimal text (operands here are tiny: a sexagesimal digit times 60^k).
fn mul_big(a: &BigUint, b: &BigUint) -> BigUint {
    let mut r = BigUint::zero();
    for ch in a.to_decimal().chars() {
        r.mul_add(10, 0);
        let d = ch.to_digit(10).unwrap_or(0);
        let mut t = b.clone();
        t.mul_add(d, 0);
        r.add(&t);
    }
    r
}

/// |a - b| with its sign (true = negative).
fn sub_signed(a: &BigUint, b: &BigUint) -> (bool, BigUint) {
    let (da, db) = (a.to_decimal(), b.to_decimal());
    let a_ge = da.len() > db.len() || (da.len() == db.len() && da >= db);
    let (big, small, neg) = if a_ge { (da, db, false) } else { (db, da, true) };
    // decimal subtraction
    let bb: Vec<u8> = big.bytes().rev().map(|c| c - b'0').collect();
    let sb: Vec<u8> = small.bytes().rev().map(|c| c - b'0').collect();
    let mut out = Vec::with_capacity(bb.len());
    let mut borrow = 0i8;
    for (i, d) in bb.iter().enumerate() {
        let mut v = i8::try_from(*d).unwrap_or(0) - borrow - i8::try_from(sb.get(i).copied().unwrap_or(0)).unwrap_or(0);
        if v < 0 {
            v += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(u32::try_from(v).unwrap_or(0));
    }
    let mut r = BigUint::zero();
    for d in out.iter().rev() {
        r.mul_add(10, *d);
    }
    (neg, r)
}

/// `SafeConstructor.construct_yaml_float`.
fn construct_yaml_float(value: &str) -> R<PyValue> {
    let mut value = value.replace('_', "").to_lowercase();
    let Some(first) = value.chars().next() else {
        return Err(Error::python("IndexError", "string index out of range"));
    };
    let sign = if first == '-' { -1.0 } else { 1.0 };
    if first == '+' || first == '-' {
        value.remove(0);
    }
    if value == ".inf" {
        return Ok(PyValue::Float(sign * f64::INFINITY));
    }
    if value == ".nan" {
        return Ok(PyValue::Float(f64::NAN));
    }
    if value.contains(':') {
        let mut digits = Vec::new();
        for part in value.split(':') {
            digits.push(pyint::py_float(part)?);
        }
        let mut base = BigUint::zero();
        base.mul_add(1, 1);
        let mut total = 0.0f64;
        for d in digits.iter().rev() {
            let b = base.to_f64();
            if b.is_infinite() {
                // `digit*base` converts the int base to float first
                return Err(Error::python("OverflowError", "int too large to convert to float"));
            }
            total += d * b;
            base = base.mul_small(60);
        }
        return Ok(PyValue::Float(sign * total));
    }
    Ok(PyValue::Float(sign * pyint::py_float(&value)?))
}

/// `SafeConstructor.construct_yaml_binary`: `base64.decodebytes(value.encode('ascii'))`.
fn construct_yaml_binary(value: &str, start: Mark) -> R<PyValue> {
    if let Some(pos) = value.chars().position(|c| !c.is_ascii()) {
        let chars: Vec<char> = value.chars().collect();
        let end = pos + chars[pos..].iter().take_while(|c| !c.is_ascii()).count();
        let what = if end - pos == 1 {
            let cp = chars[pos] as u32;
            let esc = if cp < 0x100 {
                format!("\\x{cp:02x}")
            } else if cp < 0x10000 {
                format!("\\u{cp:04x}")
            } else {
                format!("\\U{cp:08x}")
            };
            format!("character '{esc}' in position {pos}")
        } else {
            format!("characters in position {pos}-{}", end - 1)
        };
        return Err(Error::marked(
            None,
            None,
            format!(
                "failed to convert base64 data into ascii: 'ascii' codec can't encode {what}: ordinal not in range(128)"
            ),
            Some(start),
        ));
    }
    match a2b_base64(value.as_bytes()) {
        Ok(b) => Ok(PyValue::Bytes(b)),
        Err(msg) => Err(Error::marked(
            None,
            None,
            format!("failed to decode base64 data: {msg}"),
            Some(start),
        )),
    }
}

/// CPython `binascii.a2b_base64(data, strict_mode=False)`: non-alphabet bytes are skipped, a pad after two or three
/// data characters of a quad ends the input.
fn a2b_base64(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(data.len() / 4 * 3 + 3);
    let mut quad_pos = 0;
    let mut left: u32 = 0;
    let mut pads = 0;
    for &c in data {
        if c == b'=' {
            if quad_pos >= 2 {
                pads += 1;
                if quad_pos + pads >= 4 {
                    return Ok(out);
                }
            }
            continue;
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        let v = u32::from(v);
        pads = 0;
        match quad_pos {
            0 => {
                quad_pos = 1;
                left = v;
            }
            1 => {
                quad_pos = 2;
                out.push(u8::try_from(((left << 2) | (v >> 4)) & 0xff).unwrap_or(0));
                left = v & 0x0f;
            }
            2 => {
                quad_pos = 3;
                out.push(u8::try_from(((left << 4) | (v >> 2)) & 0xff).unwrap_or(0));
                left = v & 0x03;
            }
            _ => {
                quad_pos = 0;
                out.push(u8::try_from(((left << 6) | v) & 0xff).unwrap_or(0));
                left = 0;
            }
        }
    }
    match quad_pos {
        0 => Ok(out),
        1 => Err(format!(
            "Invalid base64-encoded string: number of data characters ({}) cannot be 1 more than a multiple of 4",
            out.len() / 3 * 4 + 1
        )),
        _ => Err("Incorrect padding".into()),
    }
}

/// `SafeConstructor.construct_yaml_timestamp` (with `datetime`'s own range checks and messages).
fn construct_yaml_timestamp(value: &str) -> R<PyValue> {
    let Some(c) = super::resolver::RE_TS_CONSTRUCT.captures(value) else {
        return Err(Error::python(
            "AttributeError",
            "'NoneType' object has no attribute 'groupdict'",
        ));
    };
    let num = |n: &str| c.name(n).map(|m| m.as_str().parse::<i64>().unwrap_or(0));
    let year = num("year").unwrap_or(0);
    let month = num("month").unwrap_or(0);
    let day = num("day").unwrap_or(0);
    let hour_s = c.name("hour").map(|m| m.as_str()).unwrap_or_default();
    if hour_s.is_empty() {
        check_date(year, month, day)?;
        return Ok(PyValue::Date {
            year: i32::try_from(year).unwrap_or(0),
            month: u8::try_from(month).unwrap_or(0),
            day: u8::try_from(day).unwrap_or(0),
        });
    }
    let hour = num("hour").unwrap_or(0);
    let minute = num("minute").unwrap_or(0);
    let second = num("second").unwrap_or(0);
    let mut fraction: u32 = 0;
    if let Some(f) = c.name("fraction").map(|m| m.as_str()).filter(|f| !f.is_empty()) {
        let f6: String = f.chars().take(6).collect();
        fraction = format!("{f6:0<6}").parse::<u32>().unwrap_or(0);
    }
    let mut offset_s: Option<i32> = None;
    if let Some(sign) = c.name("tz_sign").map(|m| m.as_str()).filter(|s| !s.is_empty()) {
        let tz_hour = num("tz_hour").unwrap_or(0);
        let tz_minute = num("tz_minute").unwrap_or(0);
        let mut delta = tz_hour * 3600 + tz_minute * 60;
        if sign == "-" {
            delta = -delta;
        }
        if delta.abs() >= 86_400 {
            return Err(Error::python(
                "ValueError",
                format!(
                    "offset must be a timedelta strictly between -timedelta(hours=24) and timedelta(hours=24), not {}.",
                    timedelta_repr(delta)
                ),
            ));
        }
        offset_s = Some(i32::try_from(delta).unwrap_or(0));
    } else if c.name("tz").is_some_and(|m| !m.as_str().is_empty()) {
        offset_s = Some(0);
    }
    check_date(year, month, day)?;
    if !(0..=23).contains(&hour) {
        return Err(Error::python("ValueError", "hour must be in 0..23"));
    }
    if !(0..=59).contains(&minute) {
        return Err(Error::python("ValueError", "minute must be in 0..59"));
    }
    if !(0..=59).contains(&second) {
        return Err(Error::python("ValueError", "second must be in 0..59"));
    }
    Ok(PyValue::DateTime(DateTime {
        year: i32::try_from(year).unwrap_or(0),
        month: u8::try_from(month).unwrap_or(0),
        day: u8::try_from(day).unwrap_or(0),
        hour: u8::try_from(hour).unwrap_or(0),
        minute: u8::try_from(minute).unwrap_or(0),
        second: u8::try_from(second).unwrap_or(0),
        microsecond: fraction,
        offset_s,
    }))
}

/// `repr(datetime.timedelta(seconds=s))`.
fn timedelta_repr(s: i64) -> String {
    let days = s.div_euclid(86_400);
    let secs = s.rem_euclid(86_400);
    match (days, secs) {
        (0, 0) => "datetime.timedelta(0)".into(),
        (0, s) => format!("datetime.timedelta(seconds={s})"),
        (d, 0) => format!("datetime.timedelta(days={d})"),
        (d, s) => format!("datetime.timedelta(days={d}, seconds={s})"),
    }
}

/// CPython `check_date_args`.
fn check_date(year: i64, month: i64, day: i64) -> R<()> {
    if !(1..=9999).contains(&year) {
        return Err(Error::python("ValueError", format!("year {year} is out of range")));
    }
    if !(1..=12).contains(&month) {
        return Err(Error::python("ValueError", "month must be in 1..12"));
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let dim = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day < 1 || day > dim {
        return Err(Error::python("ValueError", "day is out of range for month"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_like_binascii() {
        assert_eq!(a2b_base64(b"aGVsbG8=").ok(), Some(b"hello".to_vec()));
        assert_eq!(a2b_base64(b"aGVs\nbG8=").ok(), Some(b"hello".to_vec()));
        assert_eq!(a2b_base64(b"abc").err().as_deref(), Some("Incorrect padding"));
        assert!(a2b_base64(b"abcde").is_err());
        assert_eq!(a2b_base64(b"").ok(), Some(Vec::new()));
    }

    #[test]
    fn ints_like_python() {
        let i = |s: &str| construct_yaml_int(s).ok();
        assert_eq!(i("0x1F"), Some(PyValue::Int(31)));
        assert_eq!(i("-0b101"), Some(PyValue::Int(-5)));
        assert_eq!(i("017"), Some(PyValue::Int(15)));
        assert_eq!(i("1:30"), Some(PyValue::Int(90)));
        assert_eq!(i("-1:30:00"), Some(PyValue::Int(-5400)));
        assert_eq!(i("1_000"), Some(PyValue::Int(1000)));
        assert_eq!(
            i("99999999999999999999"),
            Some(PyValue::BigInt("99999999999999999999".into()))
        );
        assert!(construct_yaml_int("0x").is_err());
    }
}
