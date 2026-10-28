//! PyYAML 6.0.3 `reader.py` for a `str` stream: the whole text is checked for non-printable characters up front
//! (`check_printable`), a `'\0'` sentinel is appended, and positions are counted in code points (Python `str`
//! indices). `index == pointer` for a `str` stream, so marks only need `index/line/column`.

use super::error::{Error, Mark};

pub(crate) struct Reader {
    pub(crate) buffer: Vec<char>,
    pub(crate) pointer: usize,
    pub(crate) line: usize,
    pub(crate) column: usize,
}

/// `Reader.NON_PRINTABLE` complement: `[\x09\x0A\x0D\x20-\x7E\x85\xA0-퟿-�\U00010000-\U0010ffff]`.
fn printable(c: char) -> bool {
    matches!(c as u32, 0x09 | 0x0a | 0x0d | 0x20..=0x7e | 0x85 | 0xa0..=0xd7ff | 0xe000..=0xfffd | 0x10000..=0x10ffff)
}

impl Reader {
    pub(crate) fn new(text: &str) -> Result<Self, Error> {
        let mut buffer: Vec<char> = text.chars().collect();
        if let Some(pos) = buffer.iter().position(|c| !printable(*c)) {
            return Err(Error::Reader {
                position: pos,
                character: buffer[pos] as u32,
            });
        }
        buffer.push('\0');
        Ok(Self {
            buffer,
            pointer: 0,
            line: 0,
            column: 0,
        })
    }

    /// `peek(index)`. Python raises IndexError past the sentinel; no scanner path reads past it, so `'\0'` is safe.
    pub(crate) fn peek(&self, index: usize) -> char {
        self.buffer.get(self.pointer + index).copied().unwrap_or('\0')
    }

    /// `prefix(length)` (a slice: shorter at the end of the buffer).
    pub(crate) fn prefix(&self, length: usize) -> String {
        let end = (self.pointer + length).min(self.buffer.len());
        self.buffer[self.pointer.min(end)..end].iter().collect()
    }

    /// `forward(length)`: line/column bookkeeping (`\r\n` counts once; a BOM does not advance the column).
    pub(crate) fn forward(&mut self, length: usize) {
        for _ in 0..length {
            let Some(&ch) = self.buffer.get(self.pointer) else {
                return;
            };
            self.pointer += 1;
            if matches!(ch, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}') || (ch == '\r' && self.peek(0) != '\n') {
                self.line += 1;
                self.column = 0;
            } else if ch != '\u{feff}' {
                self.column += 1;
            }
        }
    }

    pub(crate) fn get_mark(&self) -> Mark {
        Mark {
            index: self.pointer,
            line: self.line,
            column: self.column,
        }
    }
}
