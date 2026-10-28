//! A port of the parts of CPython 3.13 `difflib` the hub uses: `unified_diff(a, b, fromfile, tofile)` over
//! `str.splitlines(keepends=True)` lines (SequenceMatcher with autojunk, grouped opcodes, n=3 context, `---`/`+++`
//! headers without dates, `@@ -a,b +c,d @@` ranges as `_format_range_unified`). Used by gitrepo (added files),
//! planner (`unified`) and rollback dry runs. Plan/changes diffs are compared byte-for-byte.
//! Owner: WS-B2.
//!
//! The algorithm is CPython's line for line (Lib/difflib.py, 3.13.14): `__chain_b` (b2j index and the "popular"
//! purge when `autojunk` and `len(b) >= 200`: elements occurring more than `len(b) // 100 + 1` times leave b2j),
//! `find_longest_match` (no isjunk, so the junk-extension passes are no-ops), the queue-based `get_matching_blocks`
//! with its sort and adjacent-block collapse, `get_opcodes`, `get_grouped_opcodes(n)` and `unified_diff`. Golden
//! outputs from Python: `tests/golden/difflib.json` (generator `backend/parity/gen/gen_difflib_golden.py`).

use std::collections::HashMap;
use std::hash::Hash;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Replace,
    Delete,
    Insert,
    Equal,
}

/// One opcode `(tag, i1, i2, j1, j2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opcode {
    pub tag: Tag,
    pub i1: usize,
    pub i2: usize,
    pub j1: usize,
    pub j2: usize,
}

/// `SequenceMatcher(None, a, b, autojunk=True)`.
pub struct SequenceMatcher<'s, T: Eq + Hash> {
    a: &'s [T],
    b: &'s [T],
    /// element of b -> its indices in b (ascending), popular elements purged
    b2j: HashMap<&'s T, Vec<usize>>,
}

impl<'s, T: Eq + Hash> SequenceMatcher<'s, T> {
    pub fn new(a: &'s [T], b: &'s [T]) -> Self {
        Self::with_autojunk(a, b, true)
    }

    pub fn with_autojunk(a: &'s [T], b: &'s [T], autojunk: bool) -> Self {
        let mut b2j: HashMap<&'s T, Vec<usize>> = HashMap::new();
        for (i, elt) in b.iter().enumerate() {
            b2j.entry(elt).or_default().push(i);
        }
        let n = b.len();
        if autojunk && n >= 200 {
            let ntest = n / 100 + 1;
            b2j.retain(|_, idxs| idxs.len() <= ntest);
        }
        Self { a, b, b2j }
    }

    /// `find_longest_match(alo, ahi, blo, bhi)` -> (i, j, k).
    pub fn find_longest_match(&self, alo: usize, ahi: usize, blo: usize, bhi: usize) -> (usize, usize, usize) {
        let (a, b) = (self.a, self.b);
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for (i, ai) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            if let Some(idxs) = self.b2j.get(ai) {
                for &j in idxs {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let prev = if j == 0 {
                        0
                    } else {
                        j2len.get(&(j - 1)).copied().unwrap_or(0)
                    };
                    let k = prev + 1;
                    newj2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = newj2len;
        }
        // Extend the best match by equal elements on each end (popular elements are not in b2j but still match here;
        // there is no junk without isjunk, so CPython's two junk-extension loops never run).
        while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi && bestj + bestsize < bhi && a[besti + bestsize] == b[bestj + bestsize] {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    }

    /// `get_matching_blocks()`, ending with the `(len(a), len(b), 0)` sentinel.
    pub fn get_matching_blocks(&self) -> Vec<(usize, usize, usize)> {
        let (la, lb) = (self.a.len(), self.b.len());
        let mut queue = vec![(0usize, la, 0usize, lb)];
        let mut blocks: Vec<(usize, usize, usize)> = Vec::new();
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (i, j, k) = self.find_longest_match(alo, ahi, blo, bhi);
            if k > 0 {
                blocks.push((i, j, k));
                if alo < i && blo < j {
                    queue.push((alo, i, blo, j));
                }
                if i + k < ahi && j + k < bhi {
                    queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        blocks.sort_unstable();
        let (mut i1, mut j1, mut k1) = (0usize, 0usize, 0usize);
        let mut out = Vec::new();
        for (i2, j2, k2) in blocks {
            if i1 + k1 == i2 && j1 + k1 == j2 {
                k1 += k2;
            } else {
                if k1 > 0 {
                    out.push((i1, j1, k1));
                }
                (i1, j1, k1) = (i2, j2, k2);
            }
        }
        if k1 > 0 {
            out.push((i1, j1, k1));
        }
        out.push((la, lb, 0));
        out
    }

    /// `get_opcodes()`.
    pub fn get_opcodes(&self) -> Vec<Opcode> {
        let (mut i, mut j) = (0usize, 0usize);
        let mut answer = Vec::new();
        for (ai, bj, size) in self.get_matching_blocks() {
            let tag = if i < ai && j < bj {
                Some(Tag::Replace)
            } else if i < ai {
                Some(Tag::Delete)
            } else if j < bj {
                Some(Tag::Insert)
            } else {
                None
            };
            if let Some(tag) = tag {
                answer.push(Opcode {
                    tag,
                    i1: i,
                    i2: ai,
                    j1: j,
                    j2: bj,
                });
            }
            i = ai + size;
            j = bj + size;
            if size > 0 {
                answer.push(Opcode {
                    tag: Tag::Equal,
                    i1: ai,
                    i2: i,
                    j1: bj,
                    j2: j,
                });
            }
        }
        answer
    }

    /// `get_grouped_opcodes(n)`.
    pub fn get_grouped_opcodes(&self, n: usize) -> Vec<Vec<Opcode>> {
        let mut codes = self.get_opcodes();
        if codes.is_empty() {
            codes.push(Opcode {
                tag: Tag::Equal,
                i1: 0,
                i2: 1,
                j1: 0,
                j2: 1,
            });
        }
        if let Some(first) = codes.first_mut()
            && first.tag == Tag::Equal
        {
            first.i1 = first.i1.max(first.i2.saturating_sub(n));
            first.j1 = first.j1.max(first.j2.saturating_sub(n));
        }
        if let Some(last) = codes.last_mut()
            && last.tag == Tag::Equal
        {
            last.i2 = last.i2.min(last.i1 + n);
            last.j2 = last.j2.min(last.j1 + n);
        }
        let nn = n + n;
        let mut groups = Vec::new();
        let mut group: Vec<Opcode> = Vec::new();
        for c in codes {
            let (mut i1, mut j1) = (c.i1, c.j1);
            if c.tag == Tag::Equal && c.i2 - c.i1 > nn {
                group.push(Opcode {
                    tag: c.tag,
                    i1,
                    i2: c.i2.min(i1 + n),
                    j1,
                    j2: c.j2.min(j1 + n),
                });
                groups.push(std::mem::take(&mut group));
                i1 = i1.max(c.i2.saturating_sub(n));
                j1 = j1.max(c.j2.saturating_sub(n));
            }
            group.push(Opcode {
                tag: c.tag,
                i1,
                i2: c.i2,
                j1,
                j2: c.j2,
            });
        }
        if !group.is_empty() && !(group.len() == 1 && group[0].tag == Tag::Equal) {
            groups.push(group);
        }
        groups
    }
}

/// `_format_range_unified(start, stop)`.
fn format_range_unified(start: usize, stop: usize) -> String {
    let mut beginning = start + 1;
    let length = stop - start;
    if length == 1 {
        return beginning.to_string();
    }
    if length == 0 {
        beginning -= 1;
    }
    format!("{beginning},{length}")
}

/// `"".join(difflib.unified_diff(a, b, fromfile, tofile))` (n=3, lineterm="\n", no dates).
pub fn unified_diff(a: &[&str], b: &[&str], fromfile: &str, tofile: &str) -> String {
    let sm = SequenceMatcher::new(a, b);
    let mut out = String::new();
    let mut started = false;
    for group in sm.get_grouped_opcodes(3) {
        if !started {
            started = true;
            out.push_str(&format!("--- {fromfile}\n+++ {tofile}\n"));
        }
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            format_range_unified(first.i1, last.i2),
            format_range_unified(first.j1, last.j2)
        ));
        for c in &group {
            if c.tag == Tag::Equal {
                for line in &a[c.i1..c.i2] {
                    out.push(' ');
                    out.push_str(line);
                }
                continue;
            }
            if matches!(c.tag, Tag::Replace | Tag::Delete) {
                for line in &a[c.i1..c.i2] {
                    out.push('-');
                    out.push_str(line);
                }
            }
            if matches!(c.tag, Tag::Replace | Tag::Insert) {
                for line in &b[c.j1..c.j2] {
                    out.push('+');
                    out.push_str(line);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docstring_examples() {
        let a: Vec<char> = " abcd".chars().collect();
        let b: Vec<char> = "abcd abcd".chars().collect();
        assert_eq!(SequenceMatcher::new(&a, &b).find_longest_match(0, 5, 0, 9), (0, 4, 5));
        let a: Vec<char> = "abxcd".chars().collect();
        let b: Vec<char> = "abcd".chars().collect();
        assert_eq!(
            SequenceMatcher::new(&a, &b).get_matching_blocks(),
            vec![(0, 0, 2), (3, 2, 2), (5, 4, 0)]
        );
        let a: Vec<char> = "qabxcd".chars().collect();
        let b: Vec<char> = "abycdf".chars().collect();
        let tags: Vec<Tag> = SequenceMatcher::new(&a, &b)
            .get_opcodes()
            .iter()
            .map(|o| o.tag)
            .collect();
        assert_eq!(
            tags,
            vec![Tag::Delete, Tag::Equal, Tag::Replace, Tag::Equal, Tag::Insert]
        );
    }

    #[test]
    fn unified_basic() {
        assert_eq!(unified_diff(&[], &[], "a", "b"), "");
        assert_eq!(unified_diff(&["x\n"], &["x\n"], "a", "b"), "");
        assert_eq!(
            unified_diff(&[], &["first\n"], "/dev/null", "b/x"),
            "--- /dev/null\n+++ b/x\n@@ -0,0 +1 @@\n+first\n"
        );
        assert_eq!(
            unified_diff(&["a\n", "b\n"], &["a\n", "c\n"], "a/f", "b/f"),
            "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n"
        );
    }

    /// Byte-identical to CPython's `difflib.unified_diff` on 412 generated cases (incl. autojunk-sized inputs).
    #[test]
    fn unified_diff_matches_python_goldens() -> Result<(), Box<dyn std::error::Error>> {
        let doc: serde_json::Value = serde_json::from_str(include_str!("../../tests/golden/difflib.json"))?;
        let cases = doc["cases"].as_array().ok_or("no cases")?;
        assert!(cases.len() > 400);
        for (n, c) in cases.iter().enumerate() {
            let s = |k: &str| c[k].as_str().unwrap_or_default().to_string();
            let (a, b) = (s("a"), s("b"));
            let al = hub_core::py::text::splitlines(&a, true);
            let bl = hub_core::py::text::splitlines(&b, true);
            assert_eq!(unified_diff(&al, &bl, &s("from"), &s("to")), s("out"), "case {n}");
        }
        Ok(())
    }

    proptest::proptest! {
        /// The opcodes always rebuild `b` from `a` (equal/replace/insert/delete cover both sequences in order), and the
        /// matching blocks are strictly increasing with the sentinel last: the invariants CPython documents.
        #[test]
        fn opcodes_rebuild_b(a in proptest::collection::vec(0u8..6, 0..300), b in proptest::collection::vec(0u8..6, 0..300)) {
            let sm = SequenceMatcher::new(&a, &b);
            let mut rebuilt = Vec::new();
            let (mut ai, mut bj) = (0, 0);
            for o in sm.get_opcodes() {
                proptest::prop_assert_eq!((o.i1, o.j1), (ai, bj));
                match o.tag {
                    Tag::Equal => {
                        proptest::prop_assert_eq!(&a[o.i1..o.i2], &b[o.j1..o.j2]);
                        rebuilt.extend_from_slice(&a[o.i1..o.i2]);
                    }
                    Tag::Delete => proptest::prop_assert_eq!(o.j1, o.j2),
                    Tag::Insert | Tag::Replace => rebuilt.extend_from_slice(&b[o.j1..o.j2]),
                }
                (ai, bj) = (o.i2, o.j2);
            }
            proptest::prop_assert_eq!((ai, bj), (a.len(), b.len()));
            proptest::prop_assert_eq!(rebuilt, b.clone());
            let blocks = sm.get_matching_blocks();
            proptest::prop_assert_eq!(blocks.last().copied(), Some((a.len(), b.len(), 0)));
            for w in blocks.windows(2) {
                proptest::prop_assert!(w[0].0 + w[0].2 <= w[1].0 && w[0].1 + w[0].2 <= w[1].1);
            }
        }
    }
}
