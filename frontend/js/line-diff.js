// Line diff of two texts (Myers O(ND) shortest edit script, ported from Nemo Console) and its unified projection.
// Pure; no DOM. diff.js parses and renders unified diff TEXT; unifiedText() produces that text for two drafts, so the
// config editor shows its diff with the same renderer as the plan.
// A trailing newline is not a line: "a\n" and "a" split to the same one-element array.
export function splitLines(s) {
  if (s === '' || s == null) return [];
  const lines = String(s).replace(/\r\n/g, '\n').split('\n');
  if (lines[lines.length - 1] === '') lines.pop();
  return lines;
}

/** Returns ops [{type:'eq'|'del'|'add', text, a?:1-based old line, b?:1-based new line}] in document order. */
export function diffLines(oldText, newText) {
  const A = splitLines(oldText),
    B = splitLines(newText);
  // Trim common prefix/suffix first: most edits are local and this keeps Myers' trace tiny.
  let pre = 0;
  while (pre < A.length && pre < B.length && A[pre] === B[pre]) pre++;
  let suf = 0;
  while (suf < A.length - pre && suf < B.length - pre && A[A.length - 1 - suf] === B[B.length - 1 - suf]) suf++;
  const a = A.slice(pre, A.length - suf),
    b = B.slice(pre, B.length - suf);
  const mid = myers(a, b);
  const ops = [];
  for (let i = 0; i < pre; i++) ops.push({ type: 'eq', text: A[i], a: i + 1, b: i + 1 });
  let ai = pre,
    bi = pre;
  for (const m of mid) {
    if (m.type === 'eq') {
      ops.push({ type: 'eq', text: m.text, a: ++ai, b: ++bi });
    } else if (m.type === 'del') ops.push({ type: 'del', text: m.text, a: ++ai });
    else ops.push({ type: 'add', text: m.text, b: ++bi });
  }
  for (let i = 0; i < suf; i++) ops.push({ type: 'eq', text: A[A.length - suf + i], a: ++ai, b: ++bi });
  return ops;
}

/** Past this many changed lines the diff is shown as "replace everything": Myers' trace grows with D^2. */
export const MAX_EDIT_DISTANCE = 3000;
function myers(a, b) {
  const N = a.length,
    M = b.length;
  if (!N) return b.map((text) => ({ type: 'add', text }));
  if (!M) return a.map((text) => ({ type: 'del', text }));
  const max = N + M,
    off = max;
  let v = new Array(2 * max + 2).fill(0);
  const trace = []; // per step d, only the window [off-d-1, off+d+1] the backtrack reads (O(D^2), not O(D*(N+M)))
  let found = false;
  for (let d = 0; d <= max && !found; d++) {
    if (d > MAX_EDIT_DISTANCE)
      return [...a.map((text) => ({ type: 'del', text })), ...b.map((text) => ({ type: 'add', text }))];
    const lo = Math.max(0, off - d - 1);
    trace.push({ lo, w: v.slice(lo, off + d + 2) });
    for (let k = -d; k <= d; k += 2) {
      let x = k === -d || (k !== d && v[off + k - 1] < v[off + k + 1]) ? v[off + k + 1] : v[off + k - 1] + 1;
      let y = x - k;
      while (x < N && y < M && a[x] === b[y]) {
        x++;
        y++;
      }
      v[off + k] = x;
      if (x >= N && y >= M) {
        found = true;
        break;
      }
    }
  }
  // Backtrack.
  const out = [];
  let x = N,
    y = M;
  for (let d = trace.length - 1; d > 0; d--) {
    const t = trace[d],
      k = x - y,
      at = (i) => t.w[i - t.lo] ?? 0;
    const prevK = k === -d || (k !== d && at(off + k - 1) < at(off + k + 1)) ? k + 1 : k - 1;
    const prevX = at(off + prevK),
      prevY = prevX - prevK;
    while (x > prevX && y > prevY) {
      out.push({ type: 'eq', text: a[x - 1] });
      x--;
      y--;
    }
    if (x === prevX) {
      out.push({ type: 'add', text: b[y - 1] });
      y--;
    } else {
      out.push({ type: 'del', text: a[x - 1] });
      x--;
    }
  }
  while (x > 0 && y > 0) {
    out.push({ type: 'eq', text: a[x - 1] });
    x--;
    y--;
  }
  return out.reverse();
}

export function opStats(ops) {
  let add = 0,
    del = 0;
  for (const o of ops) {
    if (o.type === 'add') add++;
    else if (o.type === 'del') del++;
  }
  return { add, del, changed: add + del > 0 };
}

/** Group ops into unified hunks with `context` unchanged lines around each change. Returns [{aStart,aLen,bStart,bLen,lines:[op]}]. */
export function toUnified(ops, context = 3) {
  const keep = new Array(ops.length).fill(false);
  ops.forEach((o, i) => {
    if (o.type !== 'eq')
      for (let j = Math.max(0, i - context); j <= Math.min(ops.length - 1, i + context); j++) keep[j] = true;
  });
  const hunks = [];
  let cur = null;
  ops.forEach((o, i) => {
    if (!keep[i]) {
      cur = null;
      return;
    }
    if (!cur) {
      cur = { lines: [] };
      hunks.push(cur);
    }
    cur.lines.push(o);
  });
  for (const hk of hunks) {
    const as = hk.lines.filter((l) => l.type !== 'add'),
      bs = hk.lines.filter((l) => l.type !== 'del');
    hk.aStart = as.length ? as[0].a : (hk.lines[0].a ?? 0);
    hk.aLen = as.length;
    hk.bStart = bs.length ? bs[0].b : (hk.lines[0].b ?? 0);
    hk.bLen = bs.length;
  }
  return hunks;
}
export const hunkHeader = (h) => `@@ -${h.aStart},${h.aLen} +${h.bStart},${h.bLen} @@`;

/** A unified diff (---/+++ headers, @@ hunks) of two texts, or '' when they are equal. */
export function unifiedText(oldText, newText, { from = 'loaded', to = 'draft', context = 3 } = {}) {
  const hunks = toUnified(diffLines(oldText, newText), context);
  if (!hunks.length) return '';
  const out = [`--- ${from}`, `+++ ${to}`];
  for (const hk of hunks) {
    out.push(hunkHeader(hk));
    for (const l of hk.lines) out.push(`${l.type === 'add' ? '+' : l.type === 'del' ? '-' : ' '}${l.text}`);
  }
  return `${out.join('\n')}\n`;
}
