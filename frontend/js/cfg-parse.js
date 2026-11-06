// Config parsers for the config editor's Form view: TOML 1.0, a strict subset of block YAML, and KEY=value env files
// (ported from Nemo Console; `env` and several-line [ ] lists added for Agent Hub). Pure logic, no DOM, so tests
// import it. Every parser returns a tree whose scalars carry their exact source span, so the Form view can rewrite one
// value in place and leave every comment, blank line and key order untouched.
//
//   parseConfig('toml' | 'yaml' | 'env', text) -> { root, errors: [{line, col, message}], warnings: [...], unsupported }
//
// Nodes:  scalar {kind:'scalar', type: string|int|float|bool|null|date, value, raw, start, end, line, style}
//         map    {kind:'map', entries: [{key, line, node}], line, inline}
//         seq    {kind:'seq', items: [node], line, inline}
// The first syntax error stops the parse (as the real parsers do). The server's validator stays the authority: these
// checks are early warnings. YAML features the subset does not model (anchors, aliases, tags, complex keys, several
// documents, multi-line flow or quoted values) set `unsupported` instead of raising an error, so a valid file is
// never reported as broken here.

class SyntaxErr extends Error { constructor(pos, message) { super(message); this.pos = pos; } }
class Unsupported extends Error { constructor(what) { super(what); this.what = what; } }

export function lineStarts(text) {
  const out = [0];
  for (let i = 0; i < text.length; i++) if (text[i] === '\n') out.push(i + 1);
  return out;
}
/** 1-based {line, col} of a character offset. */
export function posOf(starts, pos) {
  let lo = 0, hi = starts.length - 1;
  while (lo < hi) { const mid = (lo + hi + 1) >> 1; if (starts[mid] <= pos) lo = mid; else hi = mid - 1; }
  return { line: lo + 1, col: pos - starts[lo] + 1 };
}

const mkMap = (line, inline = false) => ({ kind: 'map', entries: [], line, inline });
const mkSeq = (line, inline = false) => ({ kind: 'seq', items: [], line, inline });
const mkScalar = (type, value, text, start, end, line, style) => ({ kind: 'scalar', type, value, raw: text.slice(start, end), start, end, line, style });
/** The entry for a key; with a duplicated YAML key the LAST one wins, as in PyYAML (TOML refuses duplicates). */
export const entryOf = (map, key) => map.entries.findLast((e) => e.key === key);

export const MAX_DEPTH = 64;
export function parseConfig(format, text) {
  if (format === 'env') return parseEnv(text);
  if (format !== 'toml' && format !== 'yaml') return { root: null, errors: [], warnings: [], unsupported: null };
  // A file nested deeper than any real config (e.g. 20,000 `[`) would overflow the recursive parser and every walker
  // after it; the Form view does not model it, the Text view and the server check still work.
  const deep = { root: null, errors: [], warnings: [], unsupported: `nesting deeper than ${MAX_DEPTH} levels` };
  let r;
  try { r = format === 'toml' ? parseToml(text) : parseYaml(text); } catch (e) { if (e instanceof RangeError) return deep; throw e; }
  return r.root && depthOf(r.root) > MAX_DEPTH ? deep : r;
}
/** The nesting depth of a parsed tree, without recursion. */
function depthOf(root) {
  let max = 0;
  const stack = [[root, 1]];
  while (stack.length) {
    const [n, d] = stack.pop();
    if (d > max) max = d;
    if (d > MAX_DEPTH) return d;
    if (n.kind === 'map') for (const e of n.entries) stack.push([e.node, d + 1]);
    else if (n.kind === 'seq') for (const x of n.items) stack.push([x, d + 1]);
  }
  return max;
}

// ------------------------------------------------------------------------------------------------------------ TOML

const BARE = /[A-Za-z0-9_-]/;
const TOML_ESC = { b: '\b', t: '\t', n: '\n', f: '\f', r: '\r', '"': '"', '\\': '\\' };

export function parseToml(text) {
  const starts = lineStarts(text), n = text.length;
  const lineAt = (p) => posOf(starts, p).line;
  const root = mkMap(1); root.defined = true;
  let cur = root, i = 0;
  const fail = (p, m) => { throw new SyntaxErr(p, m); };
  const ws = () => { while (i < n && (text[i] === ' ' || text[i] === '\t')) i++; };
  const comment = () => { if (text[i] === '#') while (i < n && text[i] !== '\n') i++; };
  const eol = () => {
    ws(); comment();
    if (i >= n) return;
    if (text[i] === '\r' && text[i + 1] === '\n') { i += 2; return; }
    if (text[i] === '\n') { i++; return; }
    fail(i, `unexpected text '${text.slice(i, i + 12).split('\n')[0]}' after the value (one key = value per line)`);
  };

  function basicString(multi) {
    let out = '';
    for (;;) {
      if (i >= n) fail(i, 'this string is never closed');
      const c = text[i];
      if (multi && text.startsWith('"""', i)) {
        let q = 3; while (text[i + q] === '"' && q < 5) q++;
        out += '"'.repeat(q - 3); i += q; return out;
      }
      if (!multi && c === '"') { i++; return out; }
      if (!multi && c === '\n') fail(i, 'this string is never closed on its line (use """ for several lines)');
      if (c === '\\') {
        const e = text[i + 1];
        if (multi && /[ \t\r\n]/.test(e ?? '')) {
          let j = i + 1; while (j < n && /[ \t]/.test(text[j])) j++;
          if (text[j] === '\n' || (text[j] === '\r' && text[j + 1] === '\n')) { i = j; while (i < n && /[ \t\r\n]/.test(text[i])) i++; continue; }
        }
        if (e in TOML_ESC) { out += TOML_ESC[e]; i += 2; continue; }
        if (e === 'u' || e === 'U') {
          const len = e === 'u' ? 4 : 8, hex = text.slice(i + 2, i + 2 + len);
          if (!new RegExp(`^[0-9A-Fa-f]{${len}}$`).test(hex)) fail(i, `bad \\${e} escape`);
          const cp = parseInt(hex, 16);
          if (cp > 0x10ffff || (cp >= 0xd800 && cp <= 0xdfff)) fail(i, `\\${e}${hex} is not a valid character`);
          out += String.fromCodePoint(cp); i += 2 + len; continue;
        }
        fail(i, `unknown escape \\${e ?? ''} (write \\\\ for a backslash)`);
      }
      out += c; i++;
    }
  }
  function literalString(multi) {
    const close = multi ? "'''" : "'";
    const j = text.indexOf(close, i);
    const nl = text.indexOf('\n', i);
    if (j < 0 || (!multi && nl >= 0 && nl < j)) fail(i, multi ? 'this string is never closed' : "this string is never closed on its line (use ''' for several lines)");
    let end = j;
    if (multi) while (text[end + 3] === "'" && end - j < 2) end++;
    const s = text.slice(i, end); i = end + close.length; return s;
  }
  function stringValue() {
    const s = i, line = lineAt(i);
    if (text.startsWith('"""', i)) { i += 3; if (text[i] === '\n') i++; else if (text.startsWith('\r\n', i)) i += 2; const v = basicString(true); return mkScalar('string', v, text, s, i, line, 'ml-basic'); }
    if (text.startsWith("'''", i)) { i += 3; if (text[i] === '\n') i++; else if (text.startsWith('\r\n', i)) i += 2; const v = literalString(true); return mkScalar('string', v, text, s, i, line, 'ml-literal'); }
    if (text[i] === '"') { i++; const v = basicString(false); return mkScalar('string', v, text, s, i, line, 'basic'); }
    i++; const v = literalString(false); return mkScalar('string', v, text, s, i, line, 'literal');
  }
  function key() {
    const parts = [];
    for (;;) {
      ws();
      const p = i;
      let name;
      if (text[i] === '"') { i++; name = basicString(false); }
      else if (text[i] === "'") { i++; name = literalString(false); }
      else { while (i < n && BARE.test(text[i])) i++; name = text.slice(p, i); if (!name) fail(p, text[p] === '=' ? 'a key is missing before =' : `'${text[p] ?? ''}' cannot start a key (keys are letters, digits, _ and -, or quoted)`); }
      parts.push({ name, pos: p });
      ws();
      if (text[i] !== '.') return parts;
      i++;
    }
  }
  const DELIM = /[\s,\]}#]/;
  function atomValue() {
    const s = i, line = lineAt(i), rest = text.slice(i, i + 64);
    const done = (type, value, len, style = 'plain') => {
      i = s + len;
      if (i < n && !DELIM.test(text[i])) fail(s, `'${text.slice(s, i + 1)}…' is not a valid value (strings need quotes)`);
      return mkScalar(type, value, text, s, i, line, style);
    };
    let m;
    if ((m = /^(?:true|false)/.exec(rest))) return done('bool', m[0] === 'true', m[0].length);
    if ((m = /^\d{4}-\d{2}-\d{2}(?:[Tt ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:[Zz]|[+-]\d{2}:\d{2})?)?/.exec(rest))) {
      // "1979-05-27 x": the space only joins a time, so cut a date followed by a space and no time back to the date.
      return done('date', m[0], m[0].length);
    }
    if ((m = /^\d{2}:\d{2}:\d{2}(?:\.\d+)?/.exec(rest))) return done('date', m[0], m[0].length);
    if ((m = /^0x[0-9A-Fa-f](?:_?[0-9A-Fa-f])*/.exec(rest))) return done('int', parseInt(m[0].slice(2).replace(/_/g, ''), 16), m[0].length);
    if ((m = /^0o[0-7](?:_?[0-7])*/.exec(rest))) return done('int', parseInt(m[0].slice(2).replace(/_/g, ''), 8), m[0].length);
    if ((m = /^0b[01](?:_?[01])*/.exec(rest))) return done('int', parseInt(m[0].slice(2).replace(/_/g, ''), 2), m[0].length);
    if ((m = /^[+-]?(?:inf|nan)/.exec(rest))) return done('float', m[0].includes('nan') ? NaN : m[0].startsWith('-') ? -Infinity : Infinity, m[0].length);
    if ((m = /^[+-]?(?:0|[1-9](?:_?\d)*)(?:\.\d(?:_?\d)*)?(?:[eE][+-]?\d(?:_?\d)*)?/.exec(rest)) && m[0] && /\d/.test(m[0])) {
      const isFloat = /[.eE]/.test(m[0]);
      if (/^[+-]?0\d/.test(rest)) fail(s, 'leading zeros are not allowed in TOML numbers');
      return done(isFloat ? 'float' : 'int', Number(m[0].replace(/_/g, '')), m[0].length);
    }
    const word = /^[^\s,\]}#]*/.exec(rest)[0];
    fail(s, word ? `'${word}' is not a valid value (strings need quotes: "${word}")` : 'a value is missing after =');
    return null;
  }
  function skipFlowSpace() {
    for (;;) {
      ws();
      if (text[i] === '#') { comment(); continue; }
      if (text[i] === '\n') { i++; continue; }
      if (text[i] === '\r' && text[i + 1] === '\n') { i += 2; continue; }
      return;
    }
  }
  function value() {
    const c = text[i];
    if (c === '"' || c === "'") return stringValue();
    if (c === '[') {
      const arr = mkSeq(lineAt(i), true); arr.start = i; i++;
      for (;;) {
        skipFlowSpace();
        if (i >= n) fail(arr.start, 'this array is never closed with ]');
        if (text[i] === ']') { i++; arr.end = i; return arr; }
        arr.items.push(value());
        skipFlowSpace();
        if (text[i] === ',') { i++; continue; }
        if (text[i] === ']') { i++; arr.end = i; return arr; }
        fail(i, 'expected , or ] in the array');
      }
    }
    if (c === '{') {
      const tbl = mkMap(lineAt(i), true); tbl.start = i; i++; ws();
      if (text[i] === '}') { i++; tbl.end = i; tbl.closed = true; return tbl; }
      for (;;) {
        if (text[i] === '\n' || text[i] === '\r') fail(i, 'an inline table must close with } on the same line');
        keyval(tbl, true); ws();
        if (text[i] === ',') { i++; ws(); continue; }
        if (text[i] === '}') { i++; tbl.end = i; tbl.closed = true; return tbl; }
        fail(i, text[i] === '\n' ? 'an inline table must close with } on the same line' : 'expected , or } in the inline table');
      }
    }
    if (i >= n || c === '\n' || c === '\r' || c === '#') fail(i, 'a value is missing after =');
    return atomValue();
  }
  const dupMsg = (name, e) => `duplicate key '${name}' (already defined on line ${e.line})`;
  function keyval(table, inInline = false) {
    const parts = key();
    if (text[i] !== '=') fail(i, `expected = after the key '${parts.map((p) => p.name).join('.')}'`);
    i++; ws();
    const v = value();
    let t = table;
    for (const [k, p] of parts.entries()) {
      const e = entryOf(t, p.name);
      if (k === parts.length - 1) {
        if (e) fail(p.pos, dupMsg(p.name, e));
        t.entries.push({ key: p.name, line: lineAt(p.pos), node: v });
      } else if (!e) {
        const sub = mkMap(lineAt(p.pos), inInline); sub.dotted = true; t.entries.push({ key: p.name, line: lineAt(p.pos), node: sub }); t = sub;
      } else if (e.node.kind === 'map' && !e.node.closed && !e.node.header) { t = e.node; }
      else fail(p.pos, `'${p.name}' is already defined on line ${e.line} and cannot be extended here`);
    }
  }
  function header() {
    const s = i, aot = text.startsWith('[[', i);
    i += aot ? 2 : 1;
    const parts = key();
    if (!text.startsWith(aot ? ']]' : ']', i)) fail(i, aot ? 'expected ]] to close the [[array]] header' : 'expected ] to close the [table] header');
    i += aot ? 2 : 1;
    const line = lineAt(s);
    let t = root;
    for (const [k, p] of parts.entries()) {
      const last = k === parts.length - 1, e = entryOf(t, p.name);
      if (!last) {
        if (!e) { const sub = mkMap(line); t.entries.push({ key: p.name, line, node: sub }); t = sub; }
        else if (e.node.kind === 'map' && !e.node.closed) t = e.node;
        else if (e.node.kind === 'seq' && e.node.aot) t = e.node.items[e.node.items.length - 1];
        else fail(p.pos, `'${p.name}' is already a value (line ${e.line}), not a table`);
        continue;
      }
      if (aot) {
        const item = mkMap(line); item.defined = true; item.header = true;
        if (!e) { const arr = mkSeq(line); arr.aot = true; arr.items.push(item); t.entries.push({ key: p.name, line, node: arr }); }
        else if (e.node.kind === 'seq' && e.node.aot) e.node.items.push(item);
        else fail(p.pos, `[[${parts.map((x) => x.name).join('.')}]] clashes with '${p.name}' defined on line ${e.line}`);
        cur = item;
      } else if (!e) { const sub = mkMap(line); sub.defined = true; sub.header = true; t.entries.push({ key: p.name, line, node: sub }); cur = sub; }
      else if (e.node.kind === 'map' && !e.node.closed && !e.node.defined && !e.node.dotted) { e.node.defined = true; e.node.header = true; e.node.line = line; cur = e.node; }
      else fail(s, `table [${parts.map((x) => x.name).join('.')}] is defined twice (first on line ${e.line})`);
    }
  }

  try {
    while (i < n) {
      ws();
      if (i >= n) break;
      const c = text[i];
      if (c === '#') { comment(); continue; }
      if (c === '\n') { i++; continue; }
      if (c === '\r' && text[i + 1] === '\n') { i += 2; continue; }
      if (c === '[') header(); else keyval(cur);
      eol();
    }
    return { root, errors: [], warnings: [], unsupported: null };
  } catch (e) {
    if (!(e instanceof SyntaxErr)) throw e;
    const { line, col } = posOf(starts, Math.min(e.pos, n));
    return { root: null, errors: [{ line, col, message: e.message }], warnings: [], unsupported: null };
  }
}

// ------------------------------------------------------------------------------------------------------------ YAML

// PyYAML's (YAML 1.1) implicit types for plain scalars: what `yes`, `0755`, `1e3` and `2026-11-04` really load as.
const Y_NULL = /^(?:~|null|Null|NULL|)$/;
const Y_BOOL = /^(?:yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF)$/;
const Y_INT = /^(?:[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+)$/;
const Y_FLOAT = /^(?:[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9_]+(?:[eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))$/;
const Y_DATE = /^(?:[0-9]{4}-[0-9]{2}-[0-9]{2}|[0-9]{4}-[0-9]{1,2}-[0-9]{1,2}(?:[Tt]|[ \t]+)[0-9]{1,2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9]{1,2}(?::[0-9]{2})?))?)$/;

/** How PyYAML types a plain (unquoted) scalar. */
export function yamlPlainType(s) {
  if (Y_NULL.test(s)) return { type: 'null', value: null };
  if (Y_BOOL.test(s)) return { type: 'bool', value: /^(?:yes|true|on)$/i.test(s) };
  if (Y_INT.test(s)) {
    const t = s.replace(/_/g, ''), sign = t.startsWith('-') ? -1 : 1, u = t.replace(/^[-+]/, '');
    let v;
    if (u.includes(':')) v = u.split(':').reduce((a, d) => a * 60 + Number(d), 0);
    else if (u.startsWith('0x')) v = parseInt(u.slice(2), 16);
    else if (u.startsWith('0b')) v = parseInt(u.slice(2), 2);
    else if (u.length > 1 && u.startsWith('0')) v = parseInt(u, 8);
    else v = Number(u);
    return { type: 'int', value: sign * v };
  }
  if (Y_FLOAT.test(s)) {
    const t = s.replace(/_/g, '').toLowerCase();
    const v = t.includes('inf') ? (t.startsWith('-') ? -Infinity : Infinity) : t.includes('nan') ? NaN
      : t.includes(':') ? (t.startsWith('-') ? -1 : 1) * t.replace(/^[-+]/, '').split(':').reduce((a, d) => a * 60 + Number(d), 0) : Number(t);
    return { type: 'float', value: v };
  }
  if (Y_DATE.test(s)) return { type: 'date', value: s };
  return { type: 'string', value: s };
}

const Y_ESC = { 0: '\0', a: '\x07', b: '\b', t: '\t', '\t': '\t', n: '\n', v: '\v', f: '\f', r: '\r', e: '\x1b', ' ': ' ', '"': '"', '/': '/', '\\': '\\', N: '\x85', _: '\xa0', L: '\u2028', P: '\u2029' };
const PLAIN_BAD_START = /^(?:[,[\]{}#&*!|>'"%@`]|[-?:](?:\s|$))/;

export function parseYaml(text) {
  const starts = lineStarts(text), n = text.length;
  const warnings = [];
  // One record per physical line: indent (spaces), the content start offset, the end offset (before \r?\n).
  const L = starts.map((s, k) => {
    let end = k + 1 < starts.length ? starts[k + 1] - 1 : n;
    if (text[end - 1] === '\r') end--;
    let j = s; while (j < end && text[j] === ' ') j++;
    return { no: k + 1, start: s, end, indent: j - s, cs: j };
  });
  const fail = (pos, m) => { throw new SyntaxErr(pos, m); };
  const lineAt = (p) => posOf(starts, p).line;
  const isBlank = (l) => { const c = text.slice(l.cs, l.end); return /^[ \t]*(?:#.*)?$/.test(c); };
  let li = 0;
  // PyYAML refuses a tab anywhere outside a quoted string, a | or > block and a comment (even in a blank line).
  const checkTabs = (l) => {
    let q = null;
    for (let j = l.start; j < l.end; j++) {
      const c = text[j], prev = j > l.start ? text[j - 1] : ' ';
      if (q === '"') { if (c === '\\') j++; else if (c === '"') q = null; continue; }
      if (q === "'") { if (c === "'") { if (text[j + 1] === "'") j++; else q = null; } continue; }
      if (c === '#' && /[ \t]/.test(prev)) return;
      if ((c === '"' || c === "'") && /[ \t[{,:-]/.test(prev)) { q = c; continue; }
      if (c === '\t') fail(j, j < l.cs || /^[ \t]*$/.test(text.slice(l.start, j)) ? 'a tab is used for indentation (YAML allows only spaces)' : 'a tab character here: PyYAML accepts tabs only inside quotes, | or > blocks and comments');
    }
  };
  const skipBlank = () => { while (li < L.length && isBlank(L[li])) { checkTabs(L[li]); li++; } };

  // Document markers: one leading `---` is fine; a second document, `...` followed by content, or a %directive is not modeled.
  {
    let k = 0; while (k < L.length && isBlank(L[k])) k++;
    if (k < L.length && /^%/.test(text.slice(L[k].start, L[k].end))) throw new Unsupported('YAML directives (%)');
    if (k < L.length && /^---(?:\s|$)/.test(text.slice(L[k].start, L[k].end))) {
      if (text.slice(L[k].start + 3, L[k].end).trim().replace(/^#.*/, '')) throw new Unsupported('content on the --- line');
      li = k + 1;
    }
  }

  /** Where a plain scalar ends on [s, e): before ` #` (a comment) and trailing spaces. */
  const plainEnd = (s, e, flow) => {
    let j = s;
    while (j < e) {
      const c = text[j];
      if (c === '#' && (j === s || /[ \t]/.test(text[j - 1]))) break;
      if (flow && /[,\]}]/.test(c)) break;
      if (flow && c === ':' && (j + 1 >= e || /[\s,\]}]/.test(text[j + 1]))) break;
      j++;
    }
    while (j > s && /[ \t]/.test(text[j - 1])) j--;
    return j;
  };
  const restIsEmpty = (s, e) => /^[ \t]*(?:#.*)?$/.test(text.slice(s, e));

  function quoted(s, e) {
    const q = text[s], line = lineAt(s);
    let j = s + 1, out = '';
    for (;;) {
      if (j >= e) {
        // closed on a later line = a multi-line quoted scalar (valid YAML, not modeled); never closed = an error
        const close = text.indexOf(q, e);
        if (close >= 0) throw new Unsupported('a quoted value that spans several lines');
        fail(s, 'this quoted string is never closed');
      }
      const c = text[j];
      if (q === "'" && c === "'") { if (text[j + 1] === "'") { out += "'"; j += 2; continue; } j++; break; }
      if (q === '"' && c === '"') { j++; break; }
      if (q === '"' && c === '\\') {
        const x = text[j + 1];
        if (x in Y_ESC) { out += Y_ESC[x]; j += 2; continue; }
        const len = { x: 2, u: 4, U: 8 }[x];
        if (len) {
          const hex = text.slice(j + 2, j + 2 + len);
          if (!new RegExp(`^[0-9A-Fa-f]{${len}}$`).test(hex)) fail(j, `bad \\${x} escape`);
          out += String.fromCodePoint(parseInt(hex, 16)); j += 2 + len; continue;
        }
        if (j + 1 >= e) throw new Unsupported('a quoted value that spans several lines');
        fail(j, `unknown escape \\${x} in a double-quoted string (write \\\\ for a backslash)`);
      }
      out += c; j++;
    }
    return { node: mkScalar('string', out, text, s, j, line, q === '"' ? 'double' : 'single'), end: j };
  }
  function plainScalar(s, e, flow) {
    const pe = plainEnd(s, e, flow);
    const raw = text.slice(s, pe);
    if (PLAIN_BAD_START.test(raw)) fail(s, `a value cannot start with '${raw[0]}' unless it is quoted`);
    if (!flow && /:(?:[ \t]|$)/.test(raw)) fail(s + raw.search(/:(?:[ \t]|$)/), "a second ': ' on the line (quote the value if it contains ': ')");
    const t = yamlPlainType(raw);
    return { node: mkScalar(t.type, t.value, text, s, pe, lineAt(s), 'plain'), end: pe };
  }
  // A [ ] or { } collection. On one line by default; `ml` (Agent Hub addition) lets it run over several lines, where a
  // line break is a space and `# comment` ends at the line end, as in YAML. Scalars inside still end on their line.
  function flow(s, e, ml = false) {
    let j = s;
    // a [ or { that never closes anywhere below is an error; one that closes on a later line is retried with `ml`
    const open = () => { const close = text[s] === '[' ? ']' : '}'; return !ml && text.indexOf(close, e) >= 0 ? new Unsupported('a [ ] or { } list that spans several lines') : new SyntaxErr(s, `this ${text[s]} ${close} list is never closed`); };
    const lineEnd = (p) => { const n = text.indexOf('\n', p); return n < 0 ? text.length : (text[n - 1] === '\r' ? n - 1 : n); };
    const sp = () => {
      for (;;) {
        while (j < e && (ml ? /[ \t\r\n]/ : /[ \t]/).test(text[j])) j++;
        if (j < e && text[j] === '#' && (j === s || /\s/.test(text[j - 1]))) { if (!ml) { j = e; return; } j = lineEnd(j); continue; }
        return;
      }
    };
    const end = (p) => (ml ? Math.min(e, lineEnd(p)) : e);
    const failSep = (pos, m) => { if (ml) throw new Unsupported('a value inside a several-line [ ] or { } list that the form does not model'); fail(pos, m); };
    function item() {
      sp();
      const c = text[j];
      if (j >= e) throw open();
      if (c === '[') {
        const seq = mkSeq(lineAt(j), true); seq.start = j; j++;
        for (;;) {
          sp(); if (j >= e) throw open();
          if (text[j] === ']') { j++; seq.end = j; return seq; }
          seq.items.push(item()); sp();
          if (text[j] === ',') { j++; continue; }
          if (text[j] === ']') { j++; seq.end = j; return seq; }
          if (j >= e) throw open();
          failSep(j, 'expected , or ] in the [ ] list');
        }
      }
      if (c === '{') {
        const map = mkMap(lineAt(j), true); map.start = j; j++;
        for (;;) {
          sp(); if (j >= e) throw open();
          if (text[j] === '}') { j++; map.end = j; return map; }
          const kStart = j, k = item();
          if (k.kind !== 'scalar') throw new Unsupported('a list or mapping used as a key');
          sp();
          let v;
          if (text[j] === ':') { j++; v = item(); } else v = mkScalar('null', null, text, j, j, lineAt(j), 'plain');
          if (entryOf(map, String(k.value))) warnings.push({ line: lineAt(kStart), col: 1, message: `duplicate key '${k.value}': the later value wins` });
          map.entries.push({ key: String(k.value), line: lineAt(kStart), node: v });
          sp();
          if (text[j] === ',') { j++; continue; }
          if (text[j] === '}') { j++; map.end = j; return map; }
          if (j >= e) throw open();
          failSep(j, 'expected , or } in the { } mapping');
        }
      }
      if (c === '"' || c === "'") { const r = quoted(j, end(j)); j = r.end; r.node.flow = true; return r.node; }
      if (c === '&' || c === '*' || c === '!') throw new Unsupported('anchors, aliases or tags (&, *, !)');
      const r = plainScalar(j, end(j), true); j = r.end; r.node.flow = true; return r.node;
    }
    const node = item();
    return { node, end: j };
  }

  /** A `key:` at [s, e): { key, valueStart } or null when the line is not a mapping entry. */
  function mapKey(s, e) {
    const c = text[s];
    if (c === '?' && /\s/.test(text[s + 1] ?? ' ')) throw new Unsupported('complex keys (?)');
    if (c === '&' || c === '!' || c === '*') {
      if (/^[&!*][^\s]*(?:\s|$)/.test(text.slice(s, e)) && /:(?:\s|$)/.test(text.slice(s, e))) throw new Unsupported('anchors, aliases or tags (&, *, !)');
    }
    if (c === '"' || c === "'") {
      let r; try { r = quoted(s, e); } catch (err) { if (err instanceof Unsupported) return null; throw err; }
      let j = r.end; while (j < e && /[ \t]/.test(text[j])) j++;
      if (text[j] === ':' && (j + 1 >= e || /[ \t]/.test(text[j + 1]))) return { key: r.node.value, valueStart: j + 1 };
      return null;
    }
    if (c === '[' || c === '{' || c === '#' || (c === '-' && /\s/.test(text[s + 1] ?? ' '))) return null;
    for (let j = s; j < e; j++) {
      if (text[j] === '#' && j > s && /[ \t]/.test(text[j - 1])) return null;
      if (text[j] === ':' && (j + 1 >= e || /[ \t]/.test(text[j + 1]))) {
        let k = j; while (k > s && /[ \t]/.test(text[k - 1])) k--;
        const key = text.slice(s, k);
        if (key === '<<') throw new Unsupported('merge keys (<<)');
        if (!key) fail(s, "a key is missing before ':'");
        if (PLAIN_BAD_START.test(key)) fail(s, `a key cannot start with '${key[0]}' unless it is quoted`);
        const kt = yamlPlainType(key).type;
        if (kt === 'bool' || kt === 'null') warnings.push({ line: lineAt(s), col: s - starts[lineAt(s) - 1] + 1, message: `the key '${key}' is read as ${kt === 'bool' ? `the boolean ${yamlPlainType(key).value ? 'True' : 'False'}` : 'null'} by PyYAML, not as text (quote it: "${key}")` });
        return { key, valueStart: j + 1 };
      }
    }
    return null;
  }

  /** The value that follows `key:` or `- ` on line l from offset s; `indent` is the owning node's indent. */
  function inlineValue(l, s, indent) {
    let j = s; while (j < l.end && /[ \t]/.test(text[j])) j++;
    if (restIsEmpty(j, l.end)) {
      li++;
      skipBlank();
      const nx = L[li];
      if (nx && nx.indent > indent) { checkTabs(nx); return block(nx.indent); }
      if (nx && nx.indent === indent && /^-(?:\s|$)/.test(text.slice(nx.cs, nx.end))) return seq(indent);   // `key:` then `- item` at the key's column
      return mkScalar('null', null, text, s, s, l.no, 'empty');
    }
    const c = text[j];
    if (c === '&' || c === '*' || c === '!') throw new Unsupported('anchors, aliases or tags (&, *, !)');
    if (c === '|' || c === '>') return blockScalar(l, j, indent);
    if (c === '[' || c === '{') {
      let r;
      try { r = flow(j, l.end); } catch (err) {
        if (!(err instanceof Unsupported) || !/spans several lines/.test(err.what)) throw err;
        r = flow(j, n, true);
        const k = lineAt(Math.max(j, r.end - 1)) - 1;
        if (!restIsEmpty(r.end, L[k].end)) fail(r.end, 'unexpected text after the closing bracket');
        li = k + 1; return r.node;
      }
      if (!restIsEmpty(r.end, l.end)) fail(r.end, 'unexpected text after the closing bracket');
      li++; return r.node;
    }
    if (c === '"' || c === "'") {
      const r = quoted(j, l.end);
      if (!restIsEmpty(r.end, l.end)) fail(r.end, 'unexpected text after the quoted string');
      li++; return r.node;
    }
    const r = plainScalar(j, l.end, false);
    li++;
    // A more-indented line after a plain value continues it (a folded multi-line plain scalar): valid, not modeled.
    let k = li; while (k < L.length && isBlank(L[k])) k++;
    if (k < L.length && L[k].indent > indent && !mapKeySafe(L[k])) throw new Unsupported('a plain value that continues on the next line');
    if (k < L.length && L[k].indent > indent) fail(L[k].cs, 'this line is indented more than the key above it, whose value is already given');
    return r.node;
  }
  const mapKeySafe = (l) => { try { return !!mapKey(l.cs, l.end); } catch { return false; } };

  function blockScalar(l, j, indent) {
    const head = /^([|>])([+-]?)([1-9]?)([+-]?)[ \t]*(?:#.*)?$/.exec(text.slice(j, l.end));
    if (!head) fail(j, `bad block scalar header '${text.slice(j, l.end).trim()}' (use | or > with an optional + or - and a digit)`);
    const style = head[1], chomp = head[2] || head[4], explicit = head[3] ? indent + Number(head[3]) : 0;
    li++;
    let k = li, blockIndent = explicit;
    if (!blockIndent) { while (k < L.length && L[k].cs === L[k].end) k++; blockIndent = k < L.length && L[k].indent > indent ? L[k].indent : indent + 1; }
    const lines = []; let lastContent = li - 1;
    k = li;
    while (k < L.length) {
      const x = L[k];
      if (x.cs === x.end) { lines.push(''); k++; continue; }   // a blank (or spaces-only) line belongs to the block
      if (x.indent < blockIndent) break;
      lines.push(text.slice(x.start + blockIndent, x.end)); lastContent = k; k++;
    }
    const used = lastContent - li + 1;
    const content = lines.slice(0, Math.max(used, 0));
    const trailing = lines.length - content.length;
    let value;
    if (style === '|') value = content.join('\n');
    else {
      // folded: lines join with a space, a blank line is a newline, more-indented lines keep their breaks
      value = '';
      content.forEach((ln, idx) => {
        const prev = content[idx - 1];
        if (idx === 0) value = ln;
        else if (ln === '') value += '\n';
        else if (prev === '') value += ln;
        else if (/^\s/.test(ln) || /^\s/.test(prev)) value += `\n${ln}`;
        else value += ` ${ln}`;
      });
    }
    if (content.length) value += chomp === '-' ? '' : chomp === '+' ? '\n'.repeat(1 + trailing) : '\n';
    const cStart = used > 0 ? L[li].start : L[li - 1].end, cEnd = used > 0 ? L[lastContent].end : L[li - 1].end;
    const node = mkScalar('string', value, text, cStart, cEnd, l.no, 'block');
    node.block = { header: text.slice(j, l.end).trim(), indent: blockIndent, lines: content, empty: used <= 0 };
    li = lastContent + 1;
    return node;
  }

  function seq(indent) {
    const first = L[li];
    const node = mkSeq(first.no);
    for (;;) {
      skipBlank();
      const l = L[li];
      if (!l || l.indent < indent) return node;
      checkTabs(l);
      if (l.indent > indent) fail(l.cs, 'this line is indented more than the list item above it');
      const c = text.slice(l.cs, l.end);
      if (!/^-(?:\s|$)/.test(c)) return node;
      let j = l.cs + 1; while (j < l.end && text[j] === ' ') j++;
      if (text[j] === '\t') fail(j, 'a tab is used for indentation (YAML allows only spaces)');
      if (restIsEmpty(j, l.end)) {
        li++; skipBlank();
        const nx = L[li];
        if (nx && nx.indent > indent) { checkTabs(nx); node.items.push(block(nx.indent)); }
        else node.items.push(mkScalar('null', null, text, l.cs + 1, l.cs + 1, l.no, 'empty'));
        continue;
      }
      if (/^-(?:\s|$)/.test(text.slice(j, l.end))) throw new Unsupported('a list item that starts another list on the same line');
      if (mapKey(j, l.end)) {
        // `- key: v` opens a mapping whose keys sit at the column after the dash: parse it from a virtual line.
        const col = j - l.start, at = li, saved = L[at];
        L[at] = { ...saved, indent: col, cs: j };
        node.items.push(map(col));
        L[at] = saved;
        continue;
      }
      node.items.push(inlineValue(l, j, indent));
    }
  }

  function map(indent) {
    const node = mkMap(L[li].no);
    for (;;) {
      skipBlank();
      const l = L[li];
      if (!l || l.indent < indent) return node;
      checkTabs(l);
      if (l.indent > indent) fail(l.cs, "this line's indentation does not line up with the keys above it");
      const k = mapKey(l.cs, l.end);
      if (!k) {
        const c = text.slice(l.cs, l.end);
        if (/^-(?:\s|$)/.test(c)) fail(l.cs, "a list item ('- ') where a 'key: value' line was expected");
        if (/^(?:---|\.\.\.)(?:\s|$)/.test(c) && l.indent === 0) throw new Unsupported('several YAML documents in one file');
        fail(l.cs, `expected 'key: value' here (is a ':' missing after '${c.slice(0, 24)}'?)`);
      }
      const prior = entryOf(node, k.key);
      if (prior) warnings.push({ line: l.no, col: l.indent + 1, message: `duplicate key '${k.key}': the later value wins and line ${prior.line} is ignored` });
      const entry = { key: k.key, line: l.no, node: null };
      node.entries.push(entry);
      entry.node = inlineValue(l, k.valueStart, indent);
    }
  }

  function block(indent) {
    skipBlank();
    const l = L[li];
    if (!l) return mkScalar('null', null, text, n, n, L.length, 'empty');
    checkTabs(l);
    const c = text.slice(l.cs, l.end);
    if (/^-(?:\s|$)/.test(c)) return seq(l.indent);
    if (/^(?:---|\.\.\.)(?:\s|$)/.test(c) && l.indent === 0) throw new Unsupported('several YAML documents in one file');
    if (mapKey(l.cs, l.end)) return map(l.indent);
    // a bare scalar on its own line (the whole document, or a value placed under its key)
    const node = inlineValue(l, l.cs, indent - 1);
    return node;
  }

  try {
    skipBlank();
    if (li >= L.length) return { root: mkMap(1), errors: [], warnings, unsupported: null, empty: true };
    const root = block(L[li].indent);
    skipBlank();
    if (li < L.length) {
      const l = L[li], c = text.slice(l.cs, l.end);
      if (/^(?:---|\.\.\.)(?:\s|$)/.test(c) && l.indent === 0) {
        let k = li + 1; while (k < L.length && isBlank(L[k])) k++;
        if (k < L.length || c.startsWith('---')) throw new Unsupported('several YAML documents in one file');
      } else fail(l.cs, l.indent > 0 ? 'this line is indented less than the block it belongs to, but more than the one above it' : 'unexpected content after the end of the document');
    }
    return { root, errors: [], warnings, unsupported: null };
  } catch (e) {
    if (e instanceof Unsupported) return { root: null, errors: [], warnings: [], unsupported: e.what };
    if (!(e instanceof SyntaxErr)) throw e;
    const { line, col } = posOf(starts, Math.min(e.pos, n));
    return { root: null, errors: [{ line, col, message: e.message }], warnings, unsupported: null };
  }
}

// ------------------------------------------------------------------------------------------------------------- env

const ENV_LINE = /^[ \t]*(?:export[ \t]+)?([^=#\s'"]+)[ \t]*=[ \t]*/;
const ENV_DQ = { '\\': '\\', "'": "'", '"': '"', a: '\x07', b: '\b', f: '\f', n: '\n', r: '\r', t: '\t', v: '\v' };

const isHws = (c) => c === ' ' || c === '\t';

/**
 * Where a bare env value ends: before the first ` #comment` (a `#` after a space or tab), then without the spaces and
 * tabs before that. A plain scan: the regexes /[ \t]+#/ and /[ \t]+$/ take quadratic time on a long run of spaces.
 */
function bareEnd(rest) {
  let end = rest.length;
  for (let j = 1; j < rest.length; j++) if (rest[j] === '#' && isHws(rest[j - 1])) { end = j; break; }
  while (end > 0 && isHws(rest[end - 1])) end--;
  return end;
}

/**
 * Env keys whose values are probably secrets, by name (a heuristic, not a proof): the editor shows them as password
 * fields and never sends their values to the hub. `AUTHOR` (`HUB_GIT_AUTHOR_NAME`) is not `AUTH`, and a `*_FILE` key
 * names a file, not a secret.
 */
export const SECRET_ENV_KEY = /KEY|TOKEN|SECRET|PASSW|PASSPHRASE|AUTH|CRED|BEARER|COOKIE|SESSION|PRIVATE|PWD/i;
export const isSecretEnvKey = (key) => {
  const k = String(key).replace(/AUTHORS?(?=_|$)/gi, '');
  return SECRET_ENV_KEY.test(k) && !/_FILE$/i.test(key);
};
/** What a secret value is replaced with before a draft goes to the hub's check (a plain word every check accepts). */
export const SECRET_PLACEHOLDER = 'value-kept-in-the-browser';

// The hub's reader (a port of python-dotenv) splits lines at \r\n, \n or \r and treats every character of Python's
// str.isspace() as whitespace; `HWS` is that set without CR and LF. A byte order mark is skipped at the start of a line
// (the reader skips it at the start of the file; anywhere else it is part of the key, which the name test still sees).
const WS_SET = '\\t\\n\\x0B\\x0C\\r\\x1C-\\x1F \\x85\\xA0\\u1680\\u2000-\\u200A\\u2028\\u2029\\u202F\\u205F\\u3000';
const HWS_SET = '\\t\\x0B\\x0C\\x1C-\\x1F \\x85\\xA0\\u1680\\u2000-\\u200A\\u2028\\u2029\\u202F\\u205F\\u3000';
const HWS_RE = new RegExp(`[${HWS_SET}]`);
/** A binding's start as the hub reads it: optional `export`, a 'quoted' or bare key, `=`; sticky (matched at a line start). */
const MASK_LINE = new RegExp(`[${HWS_SET}\\uFEFF]*(?:export[${HWS_SET}]+)?(?:'([^']+)'|([^=#'${WS_SET}][^=#${WS_SET}]*))[${HWS_SET}]*=[${HWS_SET}]*`, 'y');

/** Line starts and ends (end = before the line break) with the hub reader's line breaks: \r\n, \n or \r. */
function envLines(text) {
  const starts = [0], ends = [];
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c !== '\n' && c !== '\r') continue;
    ends.push(i);
    if (c === '\r' && text[i + 1] === '\n') i++;
    starts.push(i + 1);
  }
  ends.push(text.length);
  return { starts, ends };
}

/**
 * The draft with the value of every key named like a secret (see {@link isSecretEnvKey}) replaced by
 * {@link SECRET_PLACEHOLDER}, every line where it was: a quoted value that runs over several lines keeps its line
 * breaks after the placeholder, so the hub's line numbers still match the draft. Keys are read with the hub reader's
 * own grammar ('quoted' keys, any whitespace, \r line breaks). Only the check's copy is masked, never the draft itself.
 */
export function maskEnvSecrets(text) {
  let out = '', pos = 0;
  const { starts, ends } = envLines(text);
  for (let k = 0; k < starts.length; k++) {
    const ls = starts[k];
    if (ls < pos) continue;                               // inside a value masked already
    MASK_LINE.lastIndex = ls;
    const m = MASK_LINE.exec(text);
    if (!m || !isSecretEnvKey(m[1] ?? m[2])) continue;
    const vs = ls + m[0].length;
    let le = ends[k];
    if (vs > le) {                                        // a 'quoted' key over several lines: the value's own line
      le = text.length;
      for (let j = vs; j < text.length; j++) if (text[j] === '\n' || text[j] === '\r') { le = j; break; }
    }
    let ve;
    const q = text[vs];
    if (q === "'" || q === '"') {
      let j = vs + 1;
      for (; j < text.length; j++) {
        if (text[j] === '\\') { j++; continue; }
        if (text[j] === q) break;
      }
      // an unclosed quote: the hub skips this line and reads on from the next one, so mask the rest of this line
      ve = j >= text.length ? le : j + 1;
    } else {
      ve = vs + bareEndWs(text.slice(vs, le));
      if (ve === vs) continue;                            // an empty value: nothing to keep back
    }
    const breaks = text.slice(vs, ve).match(/\r\n|\n|\r/g) || [];
    out += text.slice(pos, vs) + SECRET_PLACEHOLDER + breaks.join('');
    pos = ve;
  }
  return out + text.slice(pos);
}

/** {@link bareEnd} with the hub reader's whitespace: before the first `#` that follows whitespace, then trimmed. */
function bareEndWs(rest) {
  let end = rest.length;
  for (let j = 1; j < rest.length; j++) if (rest[j] === '#' && HWS_RE.test(rest[j - 1])) { end = j; break; }
  while (end > 0 && HWS_RE.test(rest[end - 1])) end--;
  return end;
}

/**
 * A KEY=value file the way python-dotenv reads one line (the hub's own reader is a port of it): optional `export `,
 * 'single' quotes (only \\ and \' are escapes), "double" quotes (C-style escapes), or a bare value that ends at
 * ` #comment`. Every binding becomes a string scalar of one map, in file order; a later duplicate wins. Lines that
 * are not bindings (comments, blank, malformed) are left alone: the server check reports the malformed ones. A
 * quoted value that runs over several lines is not modelled (`unsupported`), so it can only be edited as text.
 */
export function parseEnv(text) {
  const starts = lineStarts(text), root = mkMap(1);
  for (let k = 0; k < starts.length; k++) {
    const ls = starts[k];
    let le = k + 1 < starts.length ? starts[k + 1] - 1 : text.length;
    if (text[le - 1] === '\r') le--;
    const line = text.slice(ls, le);
    if (/^[ \t]*(?:#|$)/.test(line)) continue;
    const m = ENV_LINE.exec(line);
    if (!m) continue;
    const key = m[1], vs = ls + m[0].length, rest = text.slice(vs, le);
    let node;
    if (rest.startsWith("'") || rest.startsWith('"')) {
      const q = rest[0];
      let j = 1, value = '';
      for (; j < rest.length; j++) {
        const c = rest[j];
        if (c === '\\' && j + 1 < rest.length) {
          const n = rest[j + 1];
          const rep = q === '"' ? ENV_DQ[n] : (n === '\\' || n === "'" ? n : undefined);
          if (rep !== undefined) { value += rep; j++; continue; }
          value += c; continue;
        }
        if (c === q) break;
        value += c;
      }
      if (j >= rest.length) return { root: null, errors: [], warnings: [], unsupported: `a quoted value that runs over several lines (line ${k + 1})` };
      node = mkScalar('string', value, text, vs, vs + j + 1, k + 1, q === '"' ? 'double' : 'single');
    } else {
      const raw = rest.slice(0, bareEnd(rest));
      node = mkScalar('string', raw, text, vs, vs + raw.length, k + 1, raw === '' ? 'empty' : 'plain');
    }
    node.env = true;
    root.entries.push({ key, line: k + 1, node });
  }
  return { root, errors: [], warnings: [], unsupported: null };
}
