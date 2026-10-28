//! PyYAML 6.0.3 `composer.py` + `resolver.py` (implicit resolvers only; SafeLoader has no path resolvers), plus
//! domain/yamlsafe.py `_StrictLoader.compose_node` (an alias or an anchor refuses the document). Nodes live in an
//! arena so aliases share a node exactly like PyYAML's object references (and may form cycles).

use std::collections::HashMap;

use super::error::{Error, Mark};
use super::parser::{E, Ev, Parser};
use super::resolver;

pub(crate) type NodeId = usize;

#[derive(Debug, Clone)]
pub(crate) enum NodeVal {
    Scalar { value: String },
    Seq(Vec<NodeId>),
    Map(Vec<(NodeId, NodeId)>),
}

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub(crate) tag: String,
    pub(crate) val: NodeVal,
    pub(crate) start: Mark,
}

impl Node {
    /// `node.id` / the class name used in `TypeError: cannot unpack non-iterable <X> object`.
    pub(crate) fn id(&self) -> &'static str {
        match self.val {
            NodeVal::Scalar { .. } => "scalar",
            NodeVal::Seq(_) => "sequence",
            NodeVal::Map(_) => "mapping",
        }
    }

    pub(crate) fn class_name(&self) -> &'static str {
        match self.val {
            NodeVal::Scalar { .. } => "ScalarNode",
            NodeVal::Seq(_) => "SequenceNode",
            NodeVal::Map(_) => "MappingNode",
        }
    }
}

/// How many nested nodes Python composes before `RecursionError` (`compose_node` recursion: 2 Python frames per
/// level for SafeLoader, 3 for the strict loader, out of the default limit of 1000). Measured with PyYAML 6.0.3 on
/// CPython 3.13 from a direct call (493/492/491 for flow, mapping and block nesting; 329/328/327 strict); a call from
/// deeper in a server stack fails a few levels earlier in Python (DEVIATION D-YAML-RECURSION).
pub(crate) const MAX_DEPTH_SAFE: usize = 492;
pub(crate) const MAX_DEPTH_STRICT: usize = 328;

pub(crate) struct Composer {
    pub(crate) p: Parser,
    pub(crate) nodes: Vec<Node>,
    anchors: HashMap<String, NodeId>,
    strict: bool,
}

enum Entered {
    Done(NodeId),
    Open(NodeId, bool),
}

type R<T> = Result<T, Error>;

fn recursion_error() -> Error {
    Error::python("RecursionError", "maximum recursion depth exceeded")
}

impl Composer {
    pub(crate) fn new(p: Parser, strict: bool) -> Self {
        Self {
            p,
            nodes: Vec::new(),
            anchors: HashMap::new(),
            strict,
        }
    }

    /// `get_single_node()`.
    pub(crate) fn get_single_node(&mut self) -> R<Option<NodeId>> {
        self.p.get_event()?;
        let mut document = None;
        if !self.p.check_event(&[E::StreamEnd])? {
            document = Some(self.compose_document()?);
        }
        if !self.p.check_event(&[E::StreamEnd])? {
            let event = self.p.get_event()?;
            return Err(Error::marked(
                Some("expected a single document in the stream"),
                document.map(|d| self.nodes[d].start),
                "but found another document",
                event.map(|e| e.start),
            ));
        }
        self.p.get_event()?;
        Ok(document)
    }

    fn compose_document(&mut self) -> R<NodeId> {
        self.p.get_event()?;
        let node = self.compose_node()?;
        self.p.get_event()?;
        self.anchors.clear();
        Ok(node)
    }

    /// `compose_node` with its recursion (`compose_sequence_node` / `compose_mapping_node` -> `compose_node`) turned
    /// into an explicit stack of open collections, so a deep document cannot overflow a thread's stack. The event
    /// order (and so every error) is PyYAML's: a mapping composes its key, then its value without looking for
    /// MAPPING-END in between.
    fn compose_node(&mut self) -> R<NodeId> {
        let limit = if self.strict { MAX_DEPTH_STRICT } else { MAX_DEPTH_SAFE };
        // (collection, is_mapping, composed key waiting for its value)
        let mut open: Vec<(NodeId, bool, Option<NodeId>)> = Vec::new();
        loop {
            if open.len() >= limit {
                return Err(recursion_error());
            }
            let mut done = match self.enter_node()? {
                Entered::Done(id) => Some(id),
                Entered::Open(id, is_map) => {
                    open.push((id, is_map, None));
                    None
                }
            };
            loop {
                if let Some(child) = done.take() {
                    let Some((parent, is_map, pending)) = open.last_mut() else {
                        return Ok(child);
                    };
                    let parent = *parent;
                    if *is_map {
                        match pending.take() {
                            None => {
                                *pending = Some(child);
                                break; // compose the value next
                            }
                            Some(k) => {
                                if let NodeVal::Map(pairs) = &mut self.nodes[parent].val {
                                    pairs.push((k, child));
                                }
                            }
                        }
                    } else if let NodeVal::Seq(items) = &mut self.nodes[parent].val {
                        items.push(child);
                    }
                }
                let Some(&(id, is_map, _)) = open.last() else {
                    return Err(recursion_error());
                };
                let end = if is_map { E::MappingEnd } else { E::SequenceEnd };
                if !self.p.check_event(&[end])? {
                    break; // compose the next child
                }
                self.p.get_event()?;
                open.pop();
                done = Some(id);
            }
        }
    }

    /// The non-recursive part of `compose_node`: the strict loader's checks, aliases, the duplicate-anchor check,
    /// then a scalar (finished) or the start of a collection (to be filled by the caller).
    fn enter_node(&mut self) -> R<Entered> {
        if self.strict {
            if self.p.check_event(&[E::Alias])? {
                return Err(Error::Strict("YAML aliases are not allowed".into()));
            }
            let has_anchor = self.p.peek_event()?.and_then(|e| e.ev.anchor()).is_some();
            if has_anchor {
                return Err(Error::Strict("YAML anchors are not allowed".into()));
            }
        }
        if self.p.check_event(&[E::Alias])? {
            let Some(event) = self.p.get_event()? else {
                return Err(recursion_error());
            };
            let Ev::Alias(anchor) = &event.ev else {
                return Err(recursion_error());
            };
            return match self.anchors.get(anchor) {
                Some(&n) => Ok(Entered::Done(n)),
                None => Err(Error::marked(
                    None,
                    None,
                    format!("found undefined alias {}", super::pyunicode::repr(anchor)),
                    Some(event.start),
                )),
            };
        }
        let (anchor, start) = match self.p.peek_event()? {
            Some(e) => (e.ev.anchor().map(str::to_string), e.start),
            None => (None, Mark::default()),
        };
        if let Some(a) = &anchor
            && let Some(&first) = self.anchors.get(a)
        {
            return Err(Error::marked(
                Some(&format!(
                    "found duplicate anchor {}; first occurrence",
                    super::pyunicode::repr(a)
                )),
                Some(self.nodes[first].start),
                "second occurrence",
                Some(start),
            ));
        }
        let unbound = || {
            Error::python(
                "UnboundLocalError",
                "cannot access local variable 'node' where it is not associated with a value",
            )
        };
        let Some(event) = self.p.get_event()? else {
            return Err(unbound());
        };
        let (entered, node) = match event.ev {
            Ev::Scalar {
                tag, implicit, value, ..
            } => {
                let tag = match tag {
                    Some(t) if t != "!" => t,
                    _ => resolver::resolve_scalar(&value, implicit.0),
                };
                (
                    None,
                    Node {
                        tag,
                        val: NodeVal::Scalar { value },
                        start: event.start,
                    },
                )
            }
            Ev::SequenceStart { tag, .. } => {
                let tag = match tag {
                    Some(t) if t != "!" => t,
                    _ => resolver::SEQ.to_string(),
                };
                (
                    Some(false),
                    Node {
                        tag,
                        val: NodeVal::Seq(Vec::new()),
                        start: event.start,
                    },
                )
            }
            Ev::MappingStart { tag, .. } => {
                let tag = match tag {
                    Some(t) if t != "!" => t,
                    _ => resolver::MAP.to_string(),
                };
                (
                    Some(true),
                    Node {
                        tag,
                        val: NodeVal::Map(Vec::new()),
                        start: event.start,
                    },
                )
            }
            _ => return Err(unbound()),
        };
        let id = self.push(node);
        if let Some(a) = anchor {
            self.anchors.insert(a, id);
        }
        Ok(match entered {
            None => Entered::Done(id),
            Some(is_map) => Entered::Open(id, is_map),
        })
    }

    fn push(&mut self, n: Node) -> NodeId {
        self.nodes.push(n);
        self.nodes.len() - 1
    }
}
