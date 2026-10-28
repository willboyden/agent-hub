//! PyYAML 6.0.3 `parser.py` (+ `events.py`): the LL(1) recursive-descent parser written as a state machine with an
//! explicit state stack, exactly as PyYAML does (`self.state`, `self.states`, `self.marks`), so events, marks and
//! error messages are the same.

use super::error::{Error, Mark};
use super::scanner::{DirectiveValue, Kind, Scanner, Style, T, Token};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Ev {
    StreamStart,
    StreamEnd,
    DocumentStart,
    DocumentEnd,
    Alias(String),
    Scalar {
        anchor: Option<String>,
        tag: Option<String>,
        implicit: (bool, bool),
        value: String,
        style: Style,
    },
    SequenceStart {
        anchor: Option<String>,
        tag: Option<String>,
        implicit: bool,
        flow_style: Option<bool>,
    },
    SequenceEnd,
    MappingStart {
        anchor: Option<String>,
        tag: Option<String>,
        implicit: bool,
        flow_style: Option<bool>,
    },
    MappingEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum E {
    StreamStart,
    StreamEnd,
    DocumentStart,
    DocumentEnd,
    Alias,
    Scalar,
    SequenceStart,
    SequenceEnd,
    MappingStart,
    MappingEnd,
}

impl Ev {
    pub(crate) fn e(&self) -> E {
        match self {
            Ev::StreamStart => E::StreamStart,
            Ev::StreamEnd => E::StreamEnd,
            Ev::DocumentStart => E::DocumentStart,
            Ev::DocumentEnd => E::DocumentEnd,
            Ev::Alias(_) => E::Alias,
            Ev::Scalar { .. } => E::Scalar,
            Ev::SequenceStart { .. } => E::SequenceStart,
            Ev::SequenceEnd => E::SequenceEnd,
            Ev::MappingStart { .. } => E::MappingStart,
            Ev::MappingEnd => E::MappingEnd,
        }
    }

    /// `getattr(event, "anchor", None)`.
    pub(crate) fn anchor(&self) -> Option<&str> {
        match self {
            Ev::Alias(a) => Some(a),
            Ev::Scalar { anchor, .. } | Ev::SequenceStart { anchor, .. } | Ev::MappingStart { anchor, .. } => {
                anchor.as_deref()
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Event {
    pub(crate) ev: Ev,
    pub(crate) start: Mark,
    pub(crate) end: Mark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    ImplicitDocumentStart,
    DocumentStart,
    DocumentEnd,
    DocumentContent,
    BlockNode,
    BlockSequenceFirstEntry,
    BlockSequenceEntry,
    IndentlessSequenceEntry,
    BlockMappingFirstKey,
    BlockMappingKey,
    BlockMappingValue,
    FlowSequenceFirstEntry,
    FlowSequenceEntry,
    FlowSequenceEntryMappingKey,
    FlowSequenceEntryMappingValue,
    FlowSequenceEntryMappingEnd,
    FlowMappingFirstKey,
    FlowMappingKey,
    FlowMappingValue,
    FlowMappingEmptyValue,
}

type R<T> = Result<T, Error>;

pub(crate) struct Parser {
    pub(crate) s: Scanner,
    current_event: Option<Event>,
    /// Ordered like the Python dict (`tag_handles`).
    tag_handles: Vec<(String, String)>,
    states: Vec<State>,
    marks: Vec<Mark>,
    /// None = `self.state is None` (after STREAM-END); `started` = parse_stream_start has run.
    state: Option<State>,
    started: bool,
}

fn default_tags() -> Vec<(String, String)> {
    vec![
        ("!".to_string(), "!".to_string()),
        ("!!".to_string(), "tag:yaml.org,2002:".to_string()),
    ]
}

fn id_repr(t: &Token) -> String {
    super::pyunicode::repr(t.kind.id())
}

impl Parser {
    pub(crate) fn new(s: Scanner) -> Self {
        Self {
            s,
            current_event: None,
            tag_handles: Vec::new(),
            states: Vec::new(),
            marks: Vec::new(),
            state: None,
            started: false,
        }
    }

    fn run_state(&mut self) -> R<Option<Event>> {
        if !self.started {
            self.started = true;
            return self.parse_stream_start().map(Some);
        }
        let Some(state) = self.state else {
            return Ok(None);
        };
        let ev = match state {
            State::ImplicitDocumentStart => self.parse_implicit_document_start(),
            State::DocumentStart => self.parse_document_start(),
            State::DocumentEnd => self.parse_document_end(),
            State::DocumentContent => self.parse_document_content(),
            State::BlockNode => self.parse_node(true, false),
            State::BlockSequenceFirstEntry => self.parse_block_sequence_first_entry(),
            State::BlockSequenceEntry => self.parse_block_sequence_entry(),
            State::IndentlessSequenceEntry => self.parse_indentless_sequence_entry(),
            State::BlockMappingFirstKey => self.parse_block_mapping_first_key(),
            State::BlockMappingKey => self.parse_block_mapping_key(),
            State::BlockMappingValue => self.parse_block_mapping_value(),
            State::FlowSequenceFirstEntry => self.parse_flow_sequence_first_entry(),
            State::FlowSequenceEntry => self.parse_flow_sequence_entry(false),
            State::FlowSequenceEntryMappingKey => self.parse_flow_sequence_entry_mapping_key(),
            State::FlowSequenceEntryMappingValue => self.parse_flow_sequence_entry_mapping_value(),
            State::FlowSequenceEntryMappingEnd => self.parse_flow_sequence_entry_mapping_end(),
            State::FlowMappingFirstKey => self.parse_flow_mapping_first_key(),
            State::FlowMappingKey => self.parse_flow_mapping_key(false),
            State::FlowMappingValue => self.parse_flow_mapping_value(),
            State::FlowMappingEmptyValue => self.parse_flow_mapping_empty_value(),
        }?;
        Ok(Some(ev))
    }

    fn fill(&mut self) -> R<()> {
        if self.current_event.is_none() {
            self.current_event = self.run_state()?;
        }
        Ok(())
    }

    pub(crate) fn check_event(&mut self, choices: &[E]) -> R<bool> {
        self.fill()?;
        Ok(match &self.current_event {
            Some(e) => choices.is_empty() || choices.contains(&e.ev.e()),
            None => false,
        })
    }

    pub(crate) fn peek_event(&mut self) -> R<Option<&Event>> {
        self.fill()?;
        Ok(self.current_event.as_ref())
    }

    pub(crate) fn get_event(&mut self) -> R<Option<Event>> {
        self.fill()?;
        Ok(self.current_event.take())
    }

    // ---- token helpers -----------------------------------------------------------------------------------

    fn check(&mut self, choices: &[T]) -> R<bool> {
        self.s.check_token(choices)
    }

    fn peek_tok(&mut self) -> R<Token> {
        match self.s.peek_token()? {
            Some(t) => Ok(t.clone()),
            None => Err(Error::python(
                "AttributeError",
                "'NoneType' object has no attribute 'start_mark'",
            )),
        }
    }

    fn get_tok(&mut self) -> R<Token> {
        match self.s.get_token()? {
            Some(t) => Ok(t),
            None => Err(Error::python(
                "AttributeError",
                "'NoneType' object has no attribute 'start_mark'",
            )),
        }
    }

    fn pop_state(&mut self) -> R<State> {
        self.states
            .pop()
            .ok_or_else(|| Error::python("IndexError", "pop from empty list"))
    }

    // ---- stream / documents --------------------------------------------------------------------------------

    fn parse_stream_start(&mut self) -> R<Event> {
        let token = self.get_tok()?;
        self.state = Some(State::ImplicitDocumentStart);
        Ok(Event {
            ev: Ev::StreamStart,
            start: token.start,
            end: token.end,
        })
    }

    fn parse_implicit_document_start(&mut self) -> R<Event> {
        if !self.check(&[T::Directive, T::DocumentStart, T::StreamEnd])? {
            self.tag_handles = default_tags();
            let token = self.peek_tok()?;
            self.states.push(State::DocumentEnd);
            self.state = Some(State::BlockNode);
            Ok(Event {
                ev: Ev::DocumentStart,
                start: token.start,
                end: token.start,
            })
        } else {
            self.parse_document_start()
        }
    }

    fn parse_document_start(&mut self) -> R<Event> {
        while self.check(&[T::DocumentEnd])? {
            self.get_tok()?;
        }
        if !self.check(&[T::StreamEnd])? {
            let token = self.peek_tok()?;
            let start = token.start;
            self.process_directives()?;
            if !self.check(&[T::DocumentStart])? {
                let t = self.peek_tok()?;
                return Err(Error::marked(
                    None,
                    None,
                    format!("expected '<document start>', but found {}", id_repr(&t)),
                    Some(t.start),
                ));
            }
            let token = self.get_tok()?;
            self.states.push(State::DocumentEnd);
            self.state = Some(State::DocumentContent);
            Ok(Event {
                ev: Ev::DocumentStart,
                start,
                end: token.end,
            })
        } else {
            let token = self.get_tok()?;
            self.state = None;
            Ok(Event {
                ev: Ev::StreamEnd,
                start: token.start,
                end: token.end,
            })
        }
    }

    fn parse_document_end(&mut self) -> R<Event> {
        let token = self.peek_tok()?;
        let start = token.start;
        let mut end = token.start;
        if self.check(&[T::DocumentEnd])? {
            let token = self.get_tok()?;
            end = token.end;
        }
        self.state = Some(State::DocumentStart);
        Ok(Event {
            ev: Ev::DocumentEnd,
            start,
            end,
        })
    }

    fn parse_document_content(&mut self) -> R<Event> {
        if self.check(&[T::Directive, T::DocumentStart, T::DocumentEnd, T::StreamEnd])? {
            let t = self.peek_tok()?;
            let ev = process_empty_scalar(t.start);
            self.state = Some(self.pop_state()?);
            Ok(ev)
        } else {
            self.parse_node(true, false)
        }
    }

    fn process_directives(&mut self) -> R<()> {
        let mut yaml_version: Option<(String, String)> = None;
        self.tag_handles = Vec::new();
        while self.check(&[T::Directive])? {
            let token = self.get_tok()?;
            if let Kind::Directive { name, value } = &token.kind {
                match (name.as_str(), value) {
                    ("YAML", DirectiveValue::Yaml(major, minor)) => {
                        if yaml_version.is_some() {
                            return Err(Error::marked(
                                None,
                                None,
                                "found duplicate YAML directive",
                                Some(token.start),
                            ));
                        }
                        if major.trim_start_matches('0') != "1" {
                            return Err(Error::marked(
                                None,
                                None,
                                "found incompatible YAML document (version 1.* is required)",
                                Some(token.start),
                            ));
                        }
                        yaml_version = Some((major.clone(), minor.clone()));
                    }
                    ("TAG", DirectiveValue::Tag(handle, prefix)) => {
                        if self.tag_handles.iter().any(|(h, _)| h == handle) {
                            return Err(Error::marked(
                                None,
                                None,
                                format!("duplicate tag handle {}", super::pyunicode::repr(handle)),
                                Some(token.start),
                            ));
                        }
                        self.tag_handles.push((handle.clone(), prefix.clone()));
                    }
                    _ => {}
                }
            }
        }
        for (k, v) in default_tags() {
            if !self.tag_handles.iter().any(|(h, _)| *h == k) {
                self.tag_handles.push((k, v));
            }
        }
        Ok(())
    }

    // ---- nodes ---------------------------------------------------------------------------------------------

    fn parse_node(&mut self, block: bool, indentless_sequence: bool) -> R<Event> {
        if self.check(&[T::Alias])? {
            let token = self.get_tok()?;
            let Kind::Alias(name) = token.kind else {
                return Err(Error::python("AttributeError", "alias"));
            };
            self.state = Some(self.pop_state()?);
            return Ok(Event {
                ev: Ev::Alias(name),
                start: token.start,
                end: token.end,
            });
        }
        let mut anchor: Option<String> = None;
        let mut tag_pair: Option<(Option<String>, String)> = None;
        let mut start_mark: Option<Mark> = None;
        let mut end_mark: Option<Mark> = None;
        let mut tag_mark: Option<Mark> = None;
        if self.check(&[T::Anchor])? {
            let token = self.get_tok()?;
            start_mark = Some(token.start);
            end_mark = Some(token.end);
            if let Kind::Anchor(a) = token.kind {
                anchor = Some(a);
            }
            if self.check(&[T::Tag])? {
                let token = self.get_tok()?;
                tag_mark = Some(token.start);
                end_mark = Some(token.end);
                if let Kind::Tag(h, s) = token.kind {
                    tag_pair = Some((h, s));
                }
            }
        } else if self.check(&[T::Tag])? {
            let token = self.get_tok()?;
            start_mark = Some(token.start);
            tag_mark = Some(token.start);
            end_mark = Some(token.end);
            if let Kind::Tag(h, s) = token.kind {
                tag_pair = Some((h, s));
            }
            if self.check(&[T::Anchor])? {
                let token = self.get_tok()?;
                end_mark = Some(token.end);
                if let Kind::Anchor(a) = token.kind {
                    anchor = Some(a);
                }
            }
        }
        let mut tag: Option<String> = None;
        if let Some((handle, suffix)) = tag_pair {
            match handle {
                Some(h) => match self.tag_handles.iter().find(|(k, _)| *k == h) {
                    Some((_, prefix)) => tag = Some(format!("{prefix}{suffix}")),
                    None => {
                        return Err(Error::marked(
                            Some("while parsing a node"),
                            start_mark,
                            format!("found undefined tag handle {}", super::pyunicode::repr(&h)),
                            tag_mark,
                        ));
                    }
                },
                None => tag = Some(suffix),
            }
        }
        let (start_mark, mut end_mark) = match start_mark {
            Some(s) => (s, end_mark.unwrap_or(s)),
            None => {
                let m = self.peek_tok()?.start;
                (m, m)
            }
        };
        let implicit = tag.is_none() || tag.as_deref() == Some("!");
        if indentless_sequence && self.check(&[T::BlockEntry])? {
            end_mark = self.peek_tok()?.end;
            self.state = Some(State::IndentlessSequenceEntry);
            return Ok(Event {
                ev: Ev::SequenceStart {
                    anchor,
                    tag,
                    implicit,
                    flow_style: None,
                },
                start: start_mark,
                end: end_mark,
            });
        }
        if self.check(&[T::Scalar])? {
            let token = self.get_tok()?;
            end_mark = token.end;
            let Kind::Scalar { value, plain, style } = token.kind else {
                return Err(Error::python("AttributeError", "scalar"));
            };
            let implicit = if (plain && tag.is_none()) || tag.as_deref() == Some("!") {
                (true, false)
            } else if tag.is_none() {
                (false, true)
            } else {
                (false, false)
            };
            self.state = Some(self.pop_state()?);
            return Ok(Event {
                ev: Ev::Scalar {
                    anchor,
                    tag,
                    implicit,
                    value,
                    style,
                },
                start: start_mark,
                end: end_mark,
            });
        }
        if self.check(&[T::FlowSequenceStart])? {
            end_mark = self.peek_tok()?.end;
            self.state = Some(State::FlowSequenceFirstEntry);
            return Ok(Event {
                ev: Ev::SequenceStart {
                    anchor,
                    tag,
                    implicit,
                    flow_style: Some(true),
                },
                start: start_mark,
                end: end_mark,
            });
        }
        if self.check(&[T::FlowMappingStart])? {
            end_mark = self.peek_tok()?.end;
            self.state = Some(State::FlowMappingFirstKey);
            return Ok(Event {
                ev: Ev::MappingStart {
                    anchor,
                    tag,
                    implicit,
                    flow_style: Some(true),
                },
                start: start_mark,
                end: end_mark,
            });
        }
        if block && self.check(&[T::BlockSequenceStart])? {
            end_mark = self.peek_tok()?.start;
            self.state = Some(State::BlockSequenceFirstEntry);
            return Ok(Event {
                ev: Ev::SequenceStart {
                    anchor,
                    tag,
                    implicit,
                    flow_style: Some(false),
                },
                start: start_mark,
                end: end_mark,
            });
        }
        if block && self.check(&[T::BlockMappingStart])? {
            end_mark = self.peek_tok()?.start;
            self.state = Some(State::BlockMappingFirstKey);
            return Ok(Event {
                ev: Ev::MappingStart {
                    anchor,
                    tag,
                    implicit,
                    flow_style: Some(false),
                },
                start: start_mark,
                end: end_mark,
            });
        }
        if anchor.is_some() || tag.is_some() {
            self.state = Some(self.pop_state()?);
            return Ok(Event {
                ev: Ev::Scalar {
                    anchor,
                    tag,
                    implicit: (implicit, false),
                    value: String::new(),
                    style: None,
                },
                start: start_mark,
                end: end_mark,
            });
        }
        let node = if block { "block" } else { "flow" };
        let token = self.peek_tok()?;
        Err(Error::marked(
            Some(&format!("while parsing a {node} node")),
            Some(start_mark),
            format!("expected the node content, but found {}", id_repr(&token)),
            Some(token.start),
        ))
    }

    // ---- block sequences / mappings ------------------------------------------------------------------------

    fn parse_block_sequence_first_entry(&mut self) -> R<Event> {
        let token = self.get_tok()?;
        self.marks.push(token.start);
        self.parse_block_sequence_entry()
    }

    fn last_mark(&self) -> Option<Mark> {
        self.marks.last().copied()
    }

    fn parse_block_sequence_entry(&mut self) -> R<Event> {
        if self.check(&[T::BlockEntry])? {
            let token = self.get_tok()?;
            if !self.check(&[T::BlockEntry, T::BlockEnd])? {
                self.states.push(State::BlockSequenceEntry);
                return self.parse_node(true, false);
            }
            self.state = Some(State::BlockSequenceEntry);
            return Ok(process_empty_scalar(token.end));
        }
        if !self.check(&[T::BlockEnd])? {
            let token = self.peek_tok()?;
            return Err(Error::marked(
                Some("while parsing a block collection"),
                self.last_mark(),
                format!("expected <block end>, but found {}", id_repr(&token)),
                Some(token.start),
            ));
        }
        let token = self.get_tok()?;
        self.state = Some(self.pop_state()?);
        self.marks.pop();
        Ok(Event {
            ev: Ev::SequenceEnd,
            start: token.start,
            end: token.end,
        })
    }

    fn parse_indentless_sequence_entry(&mut self) -> R<Event> {
        if self.check(&[T::BlockEntry])? {
            let token = self.get_tok()?;
            if !self.check(&[T::BlockEntry, T::Key, T::Value, T::BlockEnd])? {
                self.states.push(State::IndentlessSequenceEntry);
                return self.parse_node(true, false);
            }
            self.state = Some(State::IndentlessSequenceEntry);
            return Ok(process_empty_scalar(token.end));
        }
        let token = self.peek_tok()?;
        self.state = Some(self.pop_state()?);
        Ok(Event {
            ev: Ev::SequenceEnd,
            start: token.start,
            end: token.start,
        })
    }

    fn parse_block_mapping_first_key(&mut self) -> R<Event> {
        let token = self.get_tok()?;
        self.marks.push(token.start);
        self.parse_block_mapping_key()
    }

    fn parse_block_mapping_key(&mut self) -> R<Event> {
        if self.check(&[T::Key])? {
            let token = self.get_tok()?;
            if !self.check(&[T::Key, T::Value, T::BlockEnd])? {
                self.states.push(State::BlockMappingValue);
                return self.parse_node(true, true);
            }
            self.state = Some(State::BlockMappingValue);
            return Ok(process_empty_scalar(token.end));
        }
        if !self.check(&[T::BlockEnd])? {
            let token = self.peek_tok()?;
            return Err(Error::marked(
                Some("while parsing a block mapping"),
                self.last_mark(),
                format!("expected <block end>, but found {}", id_repr(&token)),
                Some(token.start),
            ));
        }
        let token = self.get_tok()?;
        self.state = Some(self.pop_state()?);
        self.marks.pop();
        Ok(Event {
            ev: Ev::MappingEnd,
            start: token.start,
            end: token.end,
        })
    }

    fn parse_block_mapping_value(&mut self) -> R<Event> {
        if self.check(&[T::Value])? {
            let token = self.get_tok()?;
            if !self.check(&[T::Key, T::Value, T::BlockEnd])? {
                self.states.push(State::BlockMappingKey);
                return self.parse_node(true, true);
            }
            self.state = Some(State::BlockMappingKey);
            return Ok(process_empty_scalar(token.end));
        }
        self.state = Some(State::BlockMappingKey);
        let token = self.peek_tok()?;
        Ok(process_empty_scalar(token.start))
    }

    // ---- flow sequences ------------------------------------------------------------------------------------

    fn parse_flow_sequence_first_entry(&mut self) -> R<Event> {
        let token = self.get_tok()?;
        self.marks.push(token.start);
        self.parse_flow_sequence_entry(true)
    }

    fn parse_flow_sequence_entry(&mut self, first: bool) -> R<Event> {
        if !self.check(&[T::FlowSequenceEnd])? {
            if !first {
                if self.check(&[T::FlowEntry])? {
                    self.get_tok()?;
                } else {
                    let token = self.peek_tok()?;
                    return Err(Error::marked(
                        Some("while parsing a flow sequence"),
                        self.last_mark(),
                        format!("expected ',' or ']', but got {}", id_repr(&token)),
                        Some(token.start),
                    ));
                }
            }
            if self.check(&[T::Key])? {
                let token = self.peek_tok()?;
                self.state = Some(State::FlowSequenceEntryMappingKey);
                return Ok(Event {
                    ev: Ev::MappingStart {
                        anchor: None,
                        tag: None,
                        implicit: true,
                        flow_style: Some(true),
                    },
                    start: token.start,
                    end: token.end,
                });
            } else if !self.check(&[T::FlowSequenceEnd])? {
                self.states.push(State::FlowSequenceEntry);
                return self.parse_node(false, false);
            }
        }
        let token = self.get_tok()?;
        self.state = Some(self.pop_state()?);
        self.marks.pop();
        Ok(Event {
            ev: Ev::SequenceEnd,
            start: token.start,
            end: token.end,
        })
    }

    fn parse_flow_sequence_entry_mapping_key(&mut self) -> R<Event> {
        let token = self.get_tok()?;
        if !self.check(&[T::Value, T::FlowEntry, T::FlowSequenceEnd])? {
            self.states.push(State::FlowSequenceEntryMappingValue);
            return self.parse_node(false, false);
        }
        self.state = Some(State::FlowSequenceEntryMappingValue);
        Ok(process_empty_scalar(token.end))
    }

    fn parse_flow_sequence_entry_mapping_value(&mut self) -> R<Event> {
        if self.check(&[T::Value])? {
            let token = self.get_tok()?;
            if !self.check(&[T::FlowEntry, T::FlowSequenceEnd])? {
                self.states.push(State::FlowSequenceEntryMappingEnd);
                return self.parse_node(false, false);
            }
            self.state = Some(State::FlowSequenceEntryMappingEnd);
            return Ok(process_empty_scalar(token.end));
        }
        self.state = Some(State::FlowSequenceEntryMappingEnd);
        let token = self.peek_tok()?;
        Ok(process_empty_scalar(token.start))
    }

    fn parse_flow_sequence_entry_mapping_end(&mut self) -> R<Event> {
        self.state = Some(State::FlowSequenceEntry);
        let token = self.peek_tok()?;
        Ok(Event {
            ev: Ev::MappingEnd,
            start: token.start,
            end: token.start,
        })
    }

    // ---- flow mappings -------------------------------------------------------------------------------------

    fn parse_flow_mapping_first_key(&mut self) -> R<Event> {
        let token = self.get_tok()?;
        self.marks.push(token.start);
        self.parse_flow_mapping_key(true)
    }

    fn parse_flow_mapping_key(&mut self, first: bool) -> R<Event> {
        if !self.check(&[T::FlowMappingEnd])? {
            if !first {
                if self.check(&[T::FlowEntry])? {
                    self.get_tok()?;
                } else {
                    let token = self.peek_tok()?;
                    return Err(Error::marked(
                        Some("while parsing a flow mapping"),
                        self.last_mark(),
                        format!("expected ',' or '}}', but got {}", id_repr(&token)),
                        Some(token.start),
                    ));
                }
            }
            if self.check(&[T::Key])? {
                let token = self.get_tok()?;
                if !self.check(&[T::Value, T::FlowEntry, T::FlowMappingEnd])? {
                    self.states.push(State::FlowMappingValue);
                    return self.parse_node(false, false);
                }
                self.state = Some(State::FlowMappingValue);
                return Ok(process_empty_scalar(token.end));
            } else if !self.check(&[T::FlowMappingEnd])? {
                self.states.push(State::FlowMappingEmptyValue);
                return self.parse_node(false, false);
            }
        }
        let token = self.get_tok()?;
        self.state = Some(self.pop_state()?);
        self.marks.pop();
        Ok(Event {
            ev: Ev::MappingEnd,
            start: token.start,
            end: token.end,
        })
    }

    fn parse_flow_mapping_value(&mut self) -> R<Event> {
        if self.check(&[T::Value])? {
            let token = self.get_tok()?;
            if !self.check(&[T::FlowEntry, T::FlowMappingEnd])? {
                self.states.push(State::FlowMappingKey);
                return self.parse_node(false, false);
            }
            self.state = Some(State::FlowMappingKey);
            return Ok(process_empty_scalar(token.end));
        }
        self.state = Some(State::FlowMappingKey);
        let token = self.peek_tok()?;
        Ok(process_empty_scalar(token.start))
    }

    fn parse_flow_mapping_empty_value(&mut self) -> R<Event> {
        self.state = Some(State::FlowMappingKey);
        let token = self.peek_tok()?;
        Ok(process_empty_scalar(token.start))
    }
}

fn process_empty_scalar(mark: Mark) -> Event {
    Event {
        ev: Ev::Scalar {
            anchor: None,
            tag: None,
            implicit: (true, false),
            value: String::new(),
            style: None,
        },
        start: mark,
        end: mark,
    }
}
