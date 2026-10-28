//! PyYAML 6.0.3 `emitter.py`, ported state by state and writer by writer (`analyze_scalar`, `choose_scalar_style`,
//! `check_simple_key` incl. the tag length, `write_plain/single_quoted/double_quoted/folded/literal` with line
//! folding at `best_width`, indentless block sequences inside mappings, `open_ended` and the `...` end marker).
//! The serializer never emits anchors or aliases (a [`PyValue`](crate::value::PyValue) tree has no shared objects),
//! so the anchor paths of the emitter are not needed; `canonical` and explicit document markers are never used by
//! `safe_dump` with the hub's options and are left out as well.

/// A serializer event (`events.py`), without marks or anchors.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DEv {
    StreamStart,
    StreamEnd,
    DocumentStart,
    DocumentEnd,
    Scalar {
        tag: String,
        implicit: (bool, bool),
        value: String,
        style: Option<char>,
    },
    SequenceStart {
        tag: String,
        implicit: bool,
        flow_style: bool,
    },
    SequenceEnd,
    MappingStart {
        tag: String,
        implicit: bool,
        flow_style: bool,
    },
    MappingEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum St {
    StreamStart,
    FirstDocumentStart,
    DocumentStart,
    DocumentEnd,
    DocumentRoot,
    Nothing,
    FirstFlowSequenceItem,
    FlowSequenceItem,
    FirstFlowMappingKey,
    FlowMappingKey,
    FlowMappingSimpleValue,
    FlowMappingValue,
    FirstBlockSequenceItem,
    BlockSequenceItem,
    FirstBlockMappingKey,
    BlockMappingKey,
    BlockMappingSimpleValue,
    BlockMappingValue,
}

/// The chosen scalar style (`''` = plain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sty {
    Plain,
    Single,
    Double,
    Literal,
    Folded,
}

#[derive(Debug, Clone)]
struct Analysis {
    scalar: Vec<char>,
    empty: bool,
    multiline: bool,
    allow_flow_plain: bool,
    allow_block_plain: bool,
    allow_single_quoted: bool,
    allow_block: bool,
}

fn is_break(c: char) -> bool {
    matches!(c, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// `'\0 \t\r\n\x85  '`
fn z_ws(c: char) -> bool {
    matches!(c, '\0' | ' ' | '\t' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

pub(crate) struct Emitter<'a> {
    out: String,
    events: &'a [DEv],
    pos: usize,
    states: Vec<St>,
    state: St,
    indents: Vec<Option<usize>>,
    indent: Option<usize>,
    flow_level: usize,
    root_context: bool,
    sequence_context: bool,
    mapping_context: bool,
    simple_key_context: bool,
    column: usize,
    whitespace: bool,
    indention: bool,
    open_ended: bool,
    allow_unicode: bool,
    best_indent: usize,
    best_width: usize,
    prepared_tag: Option<String>,
    analysis: Option<Analysis>,
    style: Option<Sty>,
}

impl<'a> Emitter<'a> {
    pub(crate) fn new(events: &'a [DEv], width: usize, allow_unicode: bool) -> Self {
        let best_indent = 2;
        Self {
            out: String::new(),
            events,
            pos: 0,
            states: Vec::new(),
            state: St::StreamStart,
            indents: Vec::new(),
            indent: None,
            flow_level: 0,
            root_context: false,
            sequence_context: false,
            mapping_context: false,
            simple_key_context: false,
            column: 0,
            whitespace: true,
            indention: true,
            open_ended: false,
            allow_unicode,
            best_indent,
            best_width: if width > best_indent * 2 { width } else { 80 },
            prepared_tag: None,
            analysis: None,
            style: None,
        }
    }

    pub(crate) fn run(mut self) -> String {
        while self.pos < self.events.len() {
            self.step();
            self.pos += 1;
        }
        self.out
    }

    fn event(&self) -> &'a DEv {
        &self.events[self.pos]
    }

    fn next_event(&self) -> Option<&'a DEv> {
        self.events.get(self.pos + 1)
    }

    fn pop_state(&mut self) -> St {
        self.states.pop().unwrap_or(St::Nothing)
    }

    fn pop_indent(&mut self) -> Option<usize> {
        self.indents.pop().unwrap_or(None)
    }

    fn step(&mut self) {
        match self.state {
            St::StreamStart => {
                if matches!(self.event(), DEv::StreamStart) {
                    self.state = St::FirstDocumentStart;
                }
            }
            St::FirstDocumentStart => self.expect_document_start(true),
            St::DocumentStart => self.expect_document_start(false),
            St::DocumentEnd => self.expect_document_end(),
            St::DocumentRoot => {
                self.states.push(St::DocumentEnd);
                self.expect_node(true, false, false, false);
            }
            St::Nothing => {}
            St::FirstFlowSequenceItem => self.expect_flow_sequence_item(true),
            St::FlowSequenceItem => self.expect_flow_sequence_item(false),
            St::FirstFlowMappingKey => self.expect_flow_mapping_key(true),
            St::FlowMappingKey => self.expect_flow_mapping_key(false),
            St::FlowMappingSimpleValue => {
                self.write_indicator(":", false, false, false);
                self.states.push(St::FlowMappingKey);
                self.expect_node(false, false, true, false);
            }
            St::FlowMappingValue => {
                if self.column > self.best_width {
                    self.write_indent();
                }
                self.write_indicator(":", true, false, false);
                self.states.push(St::FlowMappingKey);
                self.expect_node(false, false, true, false);
            }
            St::FirstBlockSequenceItem => self.expect_block_sequence_item(true),
            St::BlockSequenceItem => self.expect_block_sequence_item(false),
            St::FirstBlockMappingKey => self.expect_block_mapping_key(true),
            St::BlockMappingKey => self.expect_block_mapping_key(false),
            St::BlockMappingSimpleValue => {
                self.write_indicator(":", false, false, false);
                self.states.push(St::BlockMappingKey);
                self.expect_node(false, false, true, false);
            }
            St::BlockMappingValue => {
                self.write_indent();
                self.write_indicator(":", true, false, true);
                self.states.push(St::BlockMappingKey);
                self.expect_node(false, false, true, false);
            }
        }
    }

    fn increase_indent(&mut self, flow: bool, indentless: bool) {
        self.indents.push(self.indent);
        match self.indent {
            None => self.indent = Some(if flow { self.best_indent } else { 0 }),
            Some(i) if !indentless => self.indent = Some(i + self.best_indent),
            Some(_) => {}
        }
    }

    // ---- documents -------------------------------------------------------------------------------------------

    fn expect_document_start(&mut self, first: bool) {
        match self.event() {
            DEv::DocumentStart => {
                let implicit = first && !self.check_empty_document();
                if !implicit {
                    self.write_indent();
                    self.write_indicator("---", true, false, false);
                }
                self.state = St::DocumentRoot;
            }
            DEv::StreamEnd => {
                if self.open_ended {
                    self.write_indicator("...", true, false, false);
                    self.write_indent();
                }
                self.state = St::Nothing;
            }
            _ => {}
        }
    }

    fn expect_document_end(&mut self) {
        if matches!(self.event(), DEv::DocumentEnd) {
            self.write_indent();
            self.state = St::DocumentStart;
        }
    }

    // ---- nodes -----------------------------------------------------------------------------------------------

    fn expect_node(&mut self, root: bool, sequence: bool, mapping: bool, simple_key: bool) {
        self.root_context = root;
        self.sequence_context = sequence;
        self.mapping_context = mapping;
        self.simple_key_context = simple_key;
        match self.event() {
            DEv::Scalar { .. } => {
                self.process_tag();
                self.increase_indent(true, false);
                self.process_scalar();
                self.indent = self.pop_indent();
                self.state = self.pop_state();
            }
            DEv::SequenceStart { flow_style, .. } => {
                self.process_tag();
                if self.flow_level > 0 || *flow_style || self.check_empty_sequence() {
                    self.write_indicator("[", true, true, false);
                    self.flow_level += 1;
                    self.increase_indent(true, false);
                    self.state = St::FirstFlowSequenceItem;
                } else {
                    let indentless = self.mapping_context && !self.indention;
                    self.increase_indent(false, indentless);
                    self.state = St::FirstBlockSequenceItem;
                }
            }
            DEv::MappingStart { flow_style, .. } => {
                self.process_tag();
                if self.flow_level > 0 || *flow_style || self.check_empty_mapping() {
                    self.write_indicator("{", true, true, false);
                    self.flow_level += 1;
                    self.increase_indent(true, false);
                    self.state = St::FirstFlowMappingKey;
                } else {
                    self.increase_indent(false, false);
                    self.state = St::FirstBlockMappingKey;
                }
            }
            _ => {}
        }
    }

    fn expect_flow_sequence_item(&mut self, first: bool) {
        if matches!(self.event(), DEv::SequenceEnd) {
            self.indent = self.pop_indent();
            self.flow_level -= 1;
            self.write_indicator("]", false, false, false);
            self.state = self.pop_state();
        } else {
            if !first {
                self.write_indicator(",", false, false, false);
            }
            if self.column > self.best_width {
                self.write_indent();
            }
            self.states.push(St::FlowSequenceItem);
            self.expect_node(false, true, false, false);
        }
    }

    fn expect_flow_mapping_key(&mut self, first: bool) {
        if matches!(self.event(), DEv::MappingEnd) {
            self.indent = self.pop_indent();
            self.flow_level -= 1;
            self.write_indicator("}", false, false, false);
            self.state = self.pop_state();
        } else {
            if !first {
                self.write_indicator(",", false, false, false);
            }
            if self.column > self.best_width {
                self.write_indent();
            }
            if self.check_simple_key() {
                self.states.push(St::FlowMappingSimpleValue);
                self.expect_node(false, false, true, true);
            } else {
                self.write_indicator("?", true, false, false);
                self.states.push(St::FlowMappingValue);
                self.expect_node(false, false, true, false);
            }
        }
    }

    fn expect_block_sequence_item(&mut self, first: bool) {
        if !first && matches!(self.event(), DEv::SequenceEnd) {
            self.indent = self.pop_indent();
            self.state = self.pop_state();
        } else {
            self.write_indent();
            self.write_indicator("-", true, false, true);
            self.states.push(St::BlockSequenceItem);
            self.expect_node(false, true, false, false);
        }
    }

    fn expect_block_mapping_key(&mut self, first: bool) {
        if !first && matches!(self.event(), DEv::MappingEnd) {
            self.indent = self.pop_indent();
            self.state = self.pop_state();
        } else {
            self.write_indent();
            if self.check_simple_key() {
                self.states.push(St::BlockMappingSimpleValue);
                self.expect_node(false, false, true, true);
            } else {
                self.write_indicator("?", true, false, true);
                self.states.push(St::BlockMappingValue);
                self.expect_node(false, false, true, false);
            }
        }
    }

    // ---- checkers --------------------------------------------------------------------------------------------

    fn check_empty_sequence(&self) -> bool {
        matches!(self.event(), DEv::SequenceStart { .. }) && matches!(self.next_event(), Some(DEv::SequenceEnd))
    }

    fn check_empty_mapping(&self) -> bool {
        matches!(self.event(), DEv::MappingStart { .. }) && matches!(self.next_event(), Some(DEv::MappingEnd))
    }

    /// The serializer always tags its scalars, so a document is never "empty" in PyYAML's sense.
    fn check_empty_document(&self) -> bool {
        false
    }

    fn check_simple_key(&mut self) -> bool {
        let mut length = 0;
        let ev = self.event();
        let tag = match ev {
            DEv::Scalar { tag, .. } | DEv::SequenceStart { tag, .. } | DEv::MappingStart { tag, .. } => Some(tag),
            _ => None,
        };
        if let Some(t) = tag {
            if self.prepared_tag.is_none() {
                self.prepared_tag = Some(self.prepare_tag(t));
            }
            length += self.prepared_tag.as_ref().map_or(0, |p| p.chars().count());
        }
        if let DEv::Scalar { value, .. } = ev {
            if self.analysis.is_none() {
                self.analysis = Some(self.analyze_scalar(value));
            }
            length += self.analysis.as_ref().map_or(0, |a| a.scalar.len());
        }
        length < 128
            && (match ev {
                DEv::Scalar { .. } => self.analysis.as_ref().is_some_and(|a| !a.empty && !a.multiline),
                _ => false,
            } || self.check_empty_sequence()
                || self.check_empty_mapping())
    }

    // ---- tags and scalars ------------------------------------------------------------------------------------

    fn process_tag(&mut self) {
        let ev = self.event();
        let tag = match ev {
            DEv::Scalar { tag, implicit, .. } => {
                if self.style.is_none() {
                    self.style = Some(self.choose_scalar_style());
                }
                let plain = self.style == Some(Sty::Plain);
                if (plain && implicit.0) || (!plain && implicit.1) {
                    self.prepared_tag = None;
                    return;
                }
                tag
            }
            DEv::SequenceStart { tag, implicit, .. } | DEv::MappingStart { tag, implicit, .. } => {
                if *implicit {
                    self.prepared_tag = None;
                    return;
                }
                tag
            }
            _ => return,
        };
        if self.prepared_tag.is_none() {
            self.prepared_tag = Some(self.prepare_tag(tag));
        }
        if let Some(p) = self.prepared_tag.take()
            && !p.is_empty()
        {
            self.write_indicator(&p, true, false, false);
        }
        self.prepared_tag = None;
    }

    fn choose_scalar_style(&mut self) -> Sty {
        let DEv::Scalar {
            value, implicit, style, ..
        } = self.event()
        else {
            return Sty::Double;
        };
        if self.analysis.is_none() {
            self.analysis = Some(self.analyze_scalar(value));
        }
        let Some(a) = self.analysis.as_ref() else {
            return Sty::Double;
        };
        if *style == Some('"') {
            return Sty::Double;
        }
        if style.is_none() && implicit.0 {
            let ok_plain = if self.flow_level > 0 {
                a.allow_flow_plain
            } else {
                a.allow_block_plain
            };
            if !(self.simple_key_context && (a.empty || a.multiline)) && ok_plain {
                return Sty::Plain;
            }
        }
        if let Some(s @ ('|' | '>')) = style
            && self.flow_level == 0
            && !self.simple_key_context
            && a.allow_block
        {
            return if *s == '|' { Sty::Literal } else { Sty::Folded };
        }
        if (style.is_none() || *style == Some('\''))
            && a.allow_single_quoted
            && !(self.simple_key_context && a.multiline)
        {
            return Sty::Single;
        }
        Sty::Double
    }

    fn process_scalar(&mut self) {
        let DEv::Scalar { value, .. } = self.event() else {
            return;
        };
        if self.analysis.is_none() {
            self.analysis = Some(self.analyze_scalar(value));
        }
        if self.style.is_none() {
            self.style = Some(self.choose_scalar_style());
        }
        let split = !self.simple_key_context;
        let text = self.analysis.take().map(|a| a.scalar).unwrap_or_default();
        match self.style.take().unwrap_or(Sty::Double) {
            Sty::Double => self.write_double_quoted(&text, split),
            Sty::Single => self.write_single_quoted(&text, split),
            Sty::Folded => self.write_folded(&text),
            Sty::Literal => self.write_literal(&text),
            Sty::Plain => self.write_plain(&text, split),
        }
    }

    /// `prepare_tag` with the default tag prefixes (`!` and `tag:yaml.org,2002:` -> `!!`).
    fn prepare_tag(&self, tag: &str) -> String {
        if tag == "!" {
            return tag.into();
        }
        let mut handle: Option<&str> = None;
        let mut suffix = tag;
        for (prefix, h) in [("!", "!"), ("tag:yaml.org,2002:", "!!")] {
            if tag.starts_with(prefix) && (prefix == "!" || prefix.len() < tag.len()) {
                handle = Some(h);
                suffix = &tag[prefix.len()..];
            }
        }
        let mut chunks = String::new();
        for ch in suffix.chars() {
            if ch.is_ascii_alphanumeric() || "-;/?:@&=+$,_.~*'()[]".contains(ch) || (ch == '!' && handle != Some("!")) {
                chunks.push(ch);
            } else {
                let mut b = [0u8; 4];
                for byte in ch.encode_utf8(&mut b).bytes() {
                    chunks.push_str(&format!("%{byte:02X}"));
                }
            }
        }
        match handle {
            Some(h) => format!("{h}{chunks}"),
            None => format!("!<{chunks}>"),
        }
    }

    fn analyze_scalar(&self, scalar: &str) -> Analysis {
        let chars: Vec<char> = scalar.chars().collect();
        if chars.is_empty() {
            return Analysis {
                scalar: chars,
                empty: true,
                multiline: false,
                allow_flow_plain: false,
                allow_block_plain: true,
                allow_single_quoted: true,
                allow_block: false,
            };
        }
        let mut block_indicators = false;
        let mut flow_indicators = false;
        let mut line_breaks = false;
        let mut special_characters = false;
        let mut leading_space = false;
        let mut leading_break = false;
        let mut trailing_space = false;
        let mut trailing_break = false;
        let mut break_space = false;
        let mut space_break = false;

        if scalar.starts_with("---") || scalar.starts_with("...") {
            block_indicators = true;
            flow_indicators = true;
        }
        let mut preceded_by_whitespace = true;
        let mut followed_by_whitespace = chars.len() == 1 || z_ws(chars[1]);
        let mut previous_space = false;
        let mut previous_break = false;
        let n = chars.len();
        let mut index = 0;
        while index < n {
            let ch = chars[index];
            if index == 0 {
                if "#,[]{}&*!|>'\"%@`".contains(ch) {
                    flow_indicators = true;
                    block_indicators = true;
                }
                if ch == '?' || ch == ':' {
                    flow_indicators = true;
                    if followed_by_whitespace {
                        block_indicators = true;
                    }
                }
                if ch == '-' && followed_by_whitespace {
                    flow_indicators = true;
                    block_indicators = true;
                }
            } else {
                if ",?[]{}".contains(ch) {
                    flow_indicators = true;
                }
                if ch == ':' {
                    flow_indicators = true;
                    if followed_by_whitespace {
                        block_indicators = true;
                    }
                }
                if ch == '#' && preceded_by_whitespace {
                    flow_indicators = true;
                    block_indicators = true;
                }
            }
            if is_break(ch) {
                line_breaks = true;
            }
            let cp = ch as u32;
            if !(ch == '\n' || (0x20..=0x7e).contains(&cp)) {
                let unicode = (cp == 0x85
                    || (0xa0..=0xd7ff).contains(&cp)
                    || (0xe000..=0xfffd).contains(&cp)
                    || (0x10000..0x10ffff).contains(&cp))
                    && cp != 0xfeff;
                if unicode {
                    if !self.allow_unicode {
                        special_characters = true;
                    }
                } else {
                    special_characters = true;
                }
            }
            if ch == ' ' {
                if index == 0 {
                    leading_space = true;
                }
                if index == n - 1 {
                    trailing_space = true;
                }
                if previous_break {
                    break_space = true;
                }
                previous_space = true;
                previous_break = false;
            } else if is_break(ch) {
                if index == 0 {
                    leading_break = true;
                }
                if index == n - 1 {
                    trailing_break = true;
                }
                if previous_space {
                    space_break = true;
                }
                previous_space = false;
                previous_break = true;
            } else {
                previous_space = false;
                previous_break = false;
            }
            index += 1;
            preceded_by_whitespace = z_ws(ch);
            followed_by_whitespace = index + 1 >= n || z_ws(chars[index + 1]);
        }
        let mut allow_flow_plain = true;
        let mut allow_block_plain = true;
        let mut allow_single_quoted = true;
        let mut allow_block = true;
        if leading_space || leading_break || trailing_space || trailing_break {
            allow_flow_plain = false;
            allow_block_plain = false;
        }
        if trailing_space {
            allow_block = false;
        }
        if break_space {
            allow_flow_plain = false;
            allow_block_plain = false;
            allow_single_quoted = false;
        }
        if space_break || special_characters {
            allow_flow_plain = false;
            allow_block_plain = false;
            allow_single_quoted = false;
            allow_block = false;
        }
        if line_breaks {
            allow_flow_plain = false;
            allow_block_plain = false;
        }
        if flow_indicators {
            allow_flow_plain = false;
        }
        if block_indicators {
            allow_block_plain = false;
        }
        Analysis {
            scalar: chars,
            empty: false,
            multiline: line_breaks,
            allow_flow_plain,
            allow_block_plain,
            allow_single_quoted,
            allow_block,
        }
    }

    // ---- writers ---------------------------------------------------------------------------------------------

    fn write(&mut self, data: &str) {
        self.out.push_str(data);
    }

    fn write_chars(&mut self, data: &[char]) {
        self.out.extend(data.iter());
        self.column += data.len();
    }

    fn write_indicator(&mut self, indicator: &str, need_whitespace: bool, whitespace: bool, indention: bool) {
        let data = if self.whitespace || !need_whitespace {
            indicator.to_string()
        } else {
            format!(" {indicator}")
        };
        self.whitespace = whitespace;
        self.indention = self.indention && indention;
        self.column += data.chars().count();
        self.open_ended = false;
        self.write(&data);
    }

    fn write_indent(&mut self) {
        let indent = self.indent.unwrap_or(0);
        if !self.indention || self.column > indent || (self.column == indent && !self.whitespace) {
            self.write_line_break(None);
        }
        if self.column < indent {
            self.whitespace = true;
            let data = " ".repeat(indent - self.column);
            self.column = indent;
            self.write(&data);
        }
    }

    fn write_line_break(&mut self, data: Option<char>) {
        self.whitespace = true;
        self.indention = true;
        self.column = 0;
        self.out.push(data.unwrap_or('\n'));
    }

    fn write_breaks(&mut self, text: &[char]) {
        for &br in text {
            if br == '\n' {
                self.write_line_break(None);
            } else {
                self.write_line_break(Some(br));
            }
        }
    }

    fn write_single_quoted(&mut self, text: &[char], split: bool) {
        self.write_indicator("'", true, false, false);
        let mut spaces = false;
        let mut breaks = false;
        let mut start = 0;
        let mut end = 0;
        while end <= text.len() {
            let ch = text.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.column > self.best_width && split && start != 0 && end != text.len() {
                        self.write_indent();
                    } else {
                        self.write_chars(&text[start..end]);
                    }
                    start = end;
                }
            } else if breaks {
                if !ch.is_some_and(is_break) {
                    if text[start] == '\n' {
                        self.write_line_break(None);
                    }
                    self.write_breaks(&text[start..end]);
                    self.write_indent();
                    start = end;
                }
            } else if (ch.is_none() || ch.is_some_and(|c| c == ' ' || is_break(c)) || ch == Some('\'')) && start < end {
                self.write_chars(&text[start..end]);
                start = end;
            }
            if ch == Some('\'') {
                self.column += 2;
                self.write("''");
                start = end + 1;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
                breaks = is_break(c);
            }
            end += 1;
        }
        self.write_indicator("'", false, false, false);
    }

    fn write_double_quoted(&mut self, text: &[char], split: bool) {
        self.write_indicator("\"", true, false, false);
        let mut start = 0;
        let mut end = 0;
        while end <= text.len() {
            let ch = text.get(end).copied();
            let escape = match ch {
                None => true,
                Some(c) => {
                    let cp = c as u32;
                    matches!(c, '"' | '\\' | '\u{85}' | '\u{2028}' | '\u{2029}' | '\u{feff}')
                        || !((0x20..=0x7e).contains(&cp)
                            || (self.allow_unicode
                                && ((0xa0..=0xd7ff).contains(&cp) || (0xe000..=0xfffd).contains(&cp))))
                }
            };
            if escape {
                if start < end {
                    self.write_chars(&text[start..end]);
                    start = end;
                }
                if let Some(c) = ch {
                    let data = match double_escape(c) {
                        Some(e) => format!("\\{e}"),
                        None => {
                            let cp = c as u32;
                            if cp <= 0xff {
                                format!("\\x{cp:02X}")
                            } else if cp <= 0xffff {
                                format!("\\u{cp:04X}")
                            } else {
                                format!("\\U{cp:08X}")
                            }
                        }
                    };
                    self.column += data.chars().count();
                    self.write(&data);
                    start = end + 1;
                }
            }
            if 0 < end
                && end + 1 < text.len()
                && (ch == Some(' ') || start >= end)
                // `self.column + (end - start) > best_width`; start may be end + 1 right after an escape
                && self.column + end > self.best_width + start
                && split
            {
                let mut data: String = if start < end {
                    text[start..end].iter().collect()
                } else {
                    String::new()
                };
                data.push('\\');
                if start < end {
                    start = end;
                }
                self.column += data.chars().count();
                self.write(&data);
                self.write_indent();
                self.whitespace = false;
                self.indention = false;
                if text.get(start) == Some(&' ') {
                    self.column += 1;
                    self.write("\\");
                }
            }
            end += 1;
        }
        self.write_indicator("\"", false, false, false);
    }

    fn determine_block_hints(&self, text: &[char]) -> String {
        let mut hints = String::new();
        if let (Some(&first), Some(&last)) = (text.first(), text.last()) {
            if first == ' ' || is_break(first) {
                hints.push_str(&self.best_indent.to_string());
            }
            if !is_break(last) {
                hints.push('-');
            } else if text.len() == 1 || is_break(text[text.len() - 2]) {
                hints.push('+');
            }
        }
        hints
    }

    fn write_folded(&mut self, text: &[char]) {
        let hints = self.determine_block_hints(text);
        self.write_indicator(&format!(">{hints}"), true, false, false);
        if hints.ends_with('+') {
            self.open_ended = true;
        }
        self.write_line_break(None);
        let mut leading_space = true;
        let mut spaces = false;
        let mut breaks = true;
        let mut start = 0;
        let mut end = 0;
        while end <= text.len() {
            let ch = text.get(end).copied();
            if breaks {
                if !ch.is_some_and(is_break) {
                    if !leading_space && ch.is_some() && ch != Some(' ') && text[start] == '\n' {
                        self.write_line_break(None);
                    }
                    leading_space = ch == Some(' ');
                    self.write_breaks(&text[start..end]);
                    if ch.is_some() {
                        self.write_indent();
                    }
                    start = end;
                }
            } else if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.column > self.best_width {
                        self.write_indent();
                    } else {
                        self.write_chars(&text[start..end]);
                    }
                    start = end;
                }
            } else if ch.is_none() || ch.is_some_and(|c| c == ' ' || is_break(c)) {
                self.write_chars(&text[start..end]);
                if ch.is_none() {
                    self.write_line_break(None);
                }
                start = end;
            }
            if let Some(c) = ch {
                breaks = is_break(c);
                spaces = c == ' ';
            }
            end += 1;
        }
    }

    fn write_literal(&mut self, text: &[char]) {
        let hints = self.determine_block_hints(text);
        self.write_indicator(&format!("|{hints}"), true, false, false);
        if hints.ends_with('+') {
            self.open_ended = true;
        }
        self.write_line_break(None);
        let mut breaks = true;
        let mut start = 0;
        let mut end = 0;
        while end <= text.len() {
            let ch = text.get(end).copied();
            if breaks {
                if !ch.is_some_and(is_break) {
                    self.write_breaks(&text[start..end]);
                    if ch.is_some() {
                        self.write_indent();
                    }
                    start = end;
                }
            } else if ch.is_none() || ch.is_some_and(is_break) {
                // PyYAML does not advance the column here (the next write is always a line break)
                let data: String = text[start..end].iter().collect();
                self.write(&data);
                if ch.is_none() {
                    self.write_line_break(None);
                }
                start = end;
            }
            if let Some(c) = ch {
                breaks = is_break(c);
            }
            end += 1;
        }
    }

    fn write_plain(&mut self, text: &[char], split: bool) {
        if self.root_context {
            self.open_ended = true;
        }
        if text.is_empty() {
            return;
        }
        if !self.whitespace {
            self.column += 1;
            self.write(" ");
        }
        self.whitespace = false;
        self.indention = false;
        let mut spaces = false;
        let mut breaks = false;
        let mut start = 0;
        let mut end = 0;
        while end <= text.len() {
            let ch = text.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.column > self.best_width && split {
                        self.write_indent();
                        self.whitespace = false;
                        self.indention = false;
                    } else {
                        self.write_chars(&text[start..end]);
                    }
                    start = end;
                }
            } else if breaks {
                if !ch.is_some_and(is_break) {
                    if text[start] == '\n' {
                        self.write_line_break(None);
                    }
                    self.write_breaks(&text[start..end]);
                    self.write_indent();
                    self.whitespace = false;
                    self.indention = false;
                    start = end;
                }
            } else if ch.is_none() || ch.is_some_and(|c| c == ' ' || is_break(c)) {
                self.write_chars(&text[start..end]);
                start = end;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
                breaks = is_break(c);
            }
            end += 1;
        }
    }
}

/// `Emitter.ESCAPE_REPLACEMENTS`.
fn double_escape(c: char) -> Option<char> {
    Some(match c {
        '\0' => '0',
        '\x07' => 'a',
        '\x08' => 'b',
        '\t' => 't',
        '\n' => 'n',
        '\x0b' => 'v',
        '\x0c' => 'f',
        '\r' => 'r',
        '\x1b' => 'e',
        '"' => '"',
        '\\' => '\\',
        '\u{85}' => 'N',
        '\u{a0}' => '_',
        '\u{2028}' => 'L',
        '\u{2029}' => 'P',
        _ => return None,
    })
}
