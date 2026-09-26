// Subsequence fuzzy scorer for the command palette. Higher is better; -1 = no match.
export function fuzzyScore(query, text) {
  const q = query.toLowerCase().trim();
  const s = text.toLowerCase();
  if (!q) return 0;
  let qi = 0, score = 0, last = -2, first = -1;
  for (let i = 0; i < s.length && qi < q.length; i++) {
    if (s[i] === q[qi]) {
      score += 1 + (i === last + 1 ? 2 : 0) + (i === 0 || /[\s\-_/:.]/.test(s[i - 1]) ? 3 : 0);
      if (first < 0) first = i;
      last = i; qi++;
    }
  }
  return qi === q.length ? score + (first === 0 ? 4 : 0) - s.length * 0.01 : -1;
}
export function fuzzyFilter(query, list, key = (x) => x.label) {
  return list
    .map((x) => ({ x, s: fuzzyScore(query, key(x)) }))
    .filter((r) => r.s >= 0)
    .sort((a, b) => b.s - a.s)
    .map((r) => r.x);
}
