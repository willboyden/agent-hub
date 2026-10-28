//! `fnmatch.fnmatchcase(name, pat)` with CPython 3.13 `fnmatch._translate` semantics: `*` matches any run INCLUDING
//! `/`, `?` one character, `[...]`/`[!...]` sets with ranges (empty ranges dropped, a `-` only makes a range at the
//! positions CPython's chunking picks), an unterminated `[` is a literal. Case-sensitive, whole-string match.
//! The policy floor (deny globs, command/egress probes) depends on these exact semantics.

#[derive(Debug, Clone)]
enum Tok {
    Star,
    Any,
    Lit(char),
    /// `negated`, literal chars, inclusive ranges
    Set(bool, Vec<char>, Vec<(char, char)>),
    /// an empty set: never matches
    Never,
}

fn compile(pat: &[char]) -> Vec<Tok> {
    let n = pat.len();
    let mut res: Vec<Tok> = Vec::new();
    let mut i = 0;
    while i < n {
        let c = pat[i];
        i += 1;
        match c {
            '*' => {
                if !matches!(res.last(), Some(Tok::Star)) {
                    res.push(Tok::Star);
                }
            }
            '?' => res.push(Tok::Any),
            '[' => {
                let mut j = i;
                if j < n && pat[j] == '!' {
                    j += 1;
                }
                if j < n && pat[j] == ']' {
                    j += 1;
                }
                while j < n && pat[j] != ']' {
                    j += 1;
                }
                if j >= n {
                    res.push(Tok::Lit('['));
                    continue;
                }
                let stuff: Vec<char> = pat[i..j].to_vec();
                let chunks: Vec<Vec<char>> = if !stuff.contains(&'-') {
                    vec![stuff]
                } else {
                    let mut chunks: Vec<Vec<char>> = Vec::new();
                    let mut ii = i;
                    let mut k = if pat[ii] == '!' { ii + 2 } else { ii + 1 };
                    loop {
                        let found = (k..j).find(|&p| pat[p] == '-');
                        let Some(kk) = found else { break };
                        chunks.push(pat[ii..kk].to_vec());
                        ii = kk + 1;
                        k = kk + 3;
                    }
                    let chunk = pat[ii..j].to_vec();
                    if chunk.is_empty() {
                        if let Some(last) = chunks.last_mut() {
                            last.push('-');
                        }
                    } else {
                        chunks.push(chunk);
                    }
                    // Remove empty ranges (CPython: `if chunks[k-1][-1] > chunks[k][0]`).
                    let mut k = chunks.len().saturating_sub(1);
                    while k > 0 {
                        let prev_last = chunks[k - 1].last().copied();
                        let next_first = chunks[k].first().copied();
                        if let (Some(a), Some(b)) = (prev_last, next_first)
                            && a > b
                        {
                            let mut merged = chunks[k - 1].clone();
                            merged.pop();
                            merged.extend_from_slice(&chunks[k][1..]);
                            chunks[k - 1] = merged;
                            chunks.remove(k);
                        }
                        k -= 1;
                    }
                    chunks
                };
                i = j + 1;
                // Rebuild the set: chunks joined by range hyphens; hyphens inside a chunk are literal.
                let flat: usize = chunks.iter().map(Vec::len).sum::<usize>() + chunks.len().saturating_sub(1);
                if flat == 0 {
                    res.push(Tok::Never);
                    continue;
                }
                let first_char = chunks.iter().find_map(|c| c.first().copied());
                if flat == 1 && first_char == Some('!') {
                    res.push(Tok::Any); // `[!]` == negated empty set == any character
                    continue;
                }
                let negated = first_char == Some('!');
                let mut lits = Vec::new();
                let mut ranges = Vec::new();
                for (ci, chunk) in chunks.iter().enumerate() {
                    let mut chars: &[char] = chunk;
                    if ci == 0 && negated && !chars.is_empty() {
                        chars = &chars[1..];
                    }
                    let start = usize::from(ci > 0); // first char is the upper end of the previous range
                    let has_next = ci + 1 < chunks.len();
                    let end = if has_next {
                        chars.len().saturating_sub(1)
                    } else {
                        chars.len()
                    };
                    if ci > 0
                        && let (Some(&lo), Some(&hi)) = (chunks[ci - 1].last(), chunk.first())
                    {
                        ranges.push((lo, hi));
                    }
                    if start < end {
                        lits.extend_from_slice(&chars[start..end]);
                    }
                }
                res.push(Tok::Set(negated, lits, ranges));
            }
            c => res.push(Tok::Lit(c)),
        }
    }
    res
}

fn tok_matches(t: &Tok, c: char) -> bool {
    match t {
        Tok::Any => true,
        Tok::Lit(l) => *l == c,
        Tok::Set(neg, lits, ranges) => {
            let hit = lits.contains(&c) || ranges.iter().any(|&(a, b)| a <= c && c <= b);
            hit != *neg
        }
        Tok::Never | Tok::Star => false,
    }
}

/// `fnmatch.fnmatchcase(name, pat)`.
pub fn fnmatchcase(name: &str, pat: &str) -> bool {
    let pat: Vec<char> = pat.chars().collect();
    let toks = compile(&pat);
    let name: Vec<char> = name.chars().collect();
    // Classic iterative wildcard matching with backtracking to the last star.
    let (mut ti, mut ni) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ni < name.len() {
        if ti < toks.len() && !matches!(toks[ti], Tok::Star) && tok_matches(&toks[ti], name[ni]) {
            ti += 1;
            ni += 1;
        } else if ti < toks.len() && matches!(toks[ti], Tok::Star) {
            star = Some((ti, ni));
            ti += 1;
        } else if let Some((st, sn)) = star {
            ti = st + 1;
            ni = sn + 1;
            star = Some((st, sn + 1));
        } else {
            return false;
        }
    }
    while ti < toks.len() && matches!(toks[ti], Tok::Star) {
        ti += 1;
    }
    ti == toks.len()
}

#[cfg(test)]
mod tests {
    use super::fnmatchcase;

    #[test]
    fn basics() {
        assert!(fnmatchcase("/home/u/.ssh/id_rsa", "/home/u/.ssh/**"));
        assert!(fnmatchcase("a/b/c", "*"));
        assert!(fnmatchcase("x.pem", "*.pem"));
        assert!(!fnmatchcase("X.PEM", "*.pem"));
        assert!(fnmatchcase("abc", "a?c"));
        assert!(fnmatchcase("b", "[a-c]"));
        assert!(!fnmatchcase("d", "[a-c]"));
        assert!(fnmatchcase("d", "[!a-c]"));
        assert!(fnmatchcase("[", "["));
        assert!(fnmatchcase("-", "[a-]"));
        assert!(!fnmatchcase("b", "[c-a]"));
        assert!(!fnmatchcase("x", "[!]"));
        assert!(fnmatchcase("[!]", "[!]"));
        assert!(fnmatchcase("x", "[!b-a]"));
        assert!(fnmatchcase("rm -rf /", "rm *"));
        assert!(fnmatchcase("a b c", "* * *"));
        assert!(!fnmatchcase("ab", "a"));
    }
}
