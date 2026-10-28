//! Port of chunking.py: fixed-window character chunking with overlap, preferring to break at paragraph / line /
//! sentence / space boundaries. Indices are code points (Python `str` indexing); `strip()` is Python's.

use hub_core::py::text;

/// `chunk_text(text, size, overlap)`; `Err` carries Python's ValueError message.
pub fn chunk_text(text: &str, size: usize, overlap: usize) -> Result<Vec<String>, String> {
    if size < 50 {
        return Err("chunk size must be >= 50".into());
    }
    if overlap >= size {
        return Err("overlap must be >= 0 and smaller than the chunk size".into());
    }
    let t: Vec<char> = text::strip(text).chars().collect();
    if t.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let n = t.len();
    let mut start = 0usize;
    while start < n {
        let mut end = (start + size).min(n);
        if end < n {
            // look for a natural break in the last 20% of the window
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            let floor = start + (size as f64 * 0.8) as usize;
            for sep in ["\n\n", "\n", ". ", " "] {
                if let Some(cut) = rfind(&t, sep, floor, end) {
                    end = cut + sep.chars().count();
                    break;
                }
            }
        }
        let piece: String = t[start..end].iter().collect();
        let piece = text::strip(&piece);
        if !piece.is_empty() {
            out.push(piece.to_string());
        }
        if end >= n {
            break;
        }
        start = end.saturating_sub(overlap).max(start + 1);
    }
    Ok(out)
}

/// `t.rfind(sep, lo, hi)`: the last index `i` with `lo <= i` and `i + len(sep) <= hi`.
fn rfind(t: &[char], sep: &str, lo: usize, hi: usize) -> Option<usize> {
    let s: Vec<char> = sep.chars().collect();
    let hi = hi.min(t.len());
    if lo > hi || hi - lo < s.len() {
        return None;
    }
    (lo..=hi - s.len()).rev().find(|&i| t[i..i + s.len()] == s[..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfind_matches_python_bounds() {
        let t: Vec<char> = "a. b. c".chars().collect();
        assert_eq!(rfind(&t, ". ", 0, 7), Some(4));
        assert_eq!(rfind(&t, ". ", 0, 5), Some(1));
        assert_eq!(rfind(&t, ". ", 5, 5), None);
        assert_eq!(rfind(&t, " ", 6, 3), None);
    }
}
