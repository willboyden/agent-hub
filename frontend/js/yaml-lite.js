// A deliberately small YAML subset for pasting a client spec: nested maps (2+ space indent), `key: value` scalars (string, number, true/false,
// quoted), inline lists `[a, b]`, and block lists of scalars (`- item`). Comments (# ...) are ignored. Anything else (anchors, tags, multi-line
// scalars, flow maps, several documents) is REJECTED with a line number instead of being guessed at. The server re-validates the resulting object
// with its own safe loader and schema; this parser only exists because the browser must send `spec` as an object.
export class YamlLiteError extends Error { constructor(message, line) { super(message); this.name = 'YamlLiteError'; this.line = line; } }

const FORBIDDEN = new Set(['__proto__', 'constructor', 'prototype']);
export function parseYamlLite(text) {
  const lines = String(text).replace(/\t/g, '  ').split('\n').map((l, i) => ({ raw: l.replace(/\s+#.*$/, '').replace(/^#.*$/, ''), n: i + 1 })).filter((l) => l.raw.trim());
  const scalar = (v) => { v = v.trim(); if (/^(true|false)$/.test(v)) return v === 'true'; if (/^-?\d+$/.test(v)) return Number(v); const q = /^(["'])(.*)\1$/.exec(v); return q ? q[2] : v; };
  if (lines.some((l) => /^---|^\.\.\./.test(l.raw))) throw new YamlLiteError('only a single YAML document is supported', lines.find((l) => /^---|^\.\.\./.test(l.raw)).n);
  let idx = 0;
  function block(indent) {
    const first = lines[idx];
    if (!first) return {};
    if (/^\s*-\s/.test(first.raw) || first.raw.trim() === '-') {
      const arr = [];
      while (idx < lines.length && lines[idx].raw.search(/\S/) === indent && /^\s*-(\s|$)/.test(lines[idx].raw)) { arr.push(scalar(lines[idx].raw.trim().slice(1))); idx++; }
      return arr;
    }
    const obj = Object.create(null);   // no prototype: a key can never reach Object.prototype
    while (idx < lines.length) {
      const l = lines[idx]; const ind = l.raw.search(/\S/);
      if (ind < indent) break;
      if (ind > indent) throw new YamlLiteError(`unexpected indentation on line ${l.n}`, l.n);
      const m = /^\s*([A-Za-z0-9_.-]+)\s*:(?:\s+(.*))?$/.exec(l.raw);
      if (!m) throw new YamlLiteError(`cannot parse line ${l.n}: ${l.raw.trim().slice(0, 40)}`, l.n);
      idx++;
      if (FORBIDDEN.has(m[1])) throw new YamlLiteError(`key "${m[1]}" is not allowed (line ${l.n})`, l.n);
      if (m[1] in obj) throw new YamlLiteError(`duplicate key "${m[1]}" on line ${l.n}`, l.n);
      if (m[2] !== undefined && /^[&*!|>{]/.test(m[2])) throw new YamlLiteError(`unsupported YAML syntax on line ${l.n} (anchors, tags, block scalars and flow maps are not supported)`, l.n);
      if (m[2] !== undefined && m[2] !== '') { obj[m[1]] = m[2].startsWith('[') ? m[2].replace(/^\[|\]$/g, '').split(',').map(scalar).filter((x) => x !== '') : scalar(m[2]); }
      else if (idx < lines.length && lines[idx].raw.search(/\S/) > indent) obj[m[1]] = block(lines[idx].raw.search(/\S/));
      else obj[m[1]] = null;
    }
    return obj;
  }
  const out = block(lines.length ? lines[0].raw.search(/\S/) : 0);
  return out;
}
