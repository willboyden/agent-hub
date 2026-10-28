#!/usr/bin/env node
// Verify backend/TEST-MAP.md against the pytest ids (backend/parity/pytest-ids.txt) and the Rust sources:
//   * every pytest id appears exactly once, and no row names an id that does not exist;
//   * a `mapped` row names `<file>::<fn>` where <file> exists and contains `fn <fn>(`;
//   * an `N/A` row carries a reason;
//   * prints mapped / pending / N/A per owner; exit 1 when anything is inconsistent or (with --strict) pending.
//
//   node backend/parity/check-test-map.mjs [--owner WS-A] [--strict]
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const PARITY = dirname(fileURLToPath(import.meta.url));
const REPO = join(PARITY, '..', '..');
const argv = process.argv.slice(2);
const only = argv.includes('--owner') ? argv[argv.indexOf('--owner') + 1] : null;
const strict = argv.includes('--strict');

const ids = new Set(readFileSync(join(PARITY, 'pytest-ids.txt'), 'utf8').split('\n').filter(Boolean));
const md = readFileSync(join(REPO, 'backend', 'TEST-MAP.md'), 'utf8');
const problems = [];
const seen = new Map();
const counts = {};
const sources = new Map();

for (const line of md.split('\n')) {
  const m = line.match(/^\| `(.+)` \| ([^|]+) \| (.*) \| (mapped|pending|N\/A) \|$/);
  if (!m) continue;
  const id = m[1].replace(/\\\|/g, '|');
  const [, , ownerRaw, rustRaw, status] = m;
  const owner = ownerRaw.trim();
  seen.set(id, (seen.get(id) || 0) + 1);
  if (!ids.has(id)) problems.push(`row for an unknown pytest id: ${id}`);
  counts[owner] ??= { mapped: 0, pending: 0, 'N/A': 0 };
  counts[owner][status] += 1;
  const rust = rustRaw.trim().replace(/^`|`$/g, '');
  if (status === 'mapped') {
    const [file, fn] = rust.split('::');
    if (!file || !fn) { problems.push(`${id}: mapped row without <file>::<fn>`); continue; }
    const p = join(REPO, file);
    if (!existsSync(p)) { problems.push(`${id}: ${file} does not exist`); continue; }
    if (!sources.has(p)) sources.set(p, readFileSync(p, 'utf8'));
    if (!new RegExp(`fn ${fn}\\s*\\(`).test(sources.get(p))) problems.push(`${id}: no fn ${fn} in ${file}`);
  } else if (status === 'N/A' && !/^N\/A: .{10,}/.test(rust)) problems.push(`${id}: N/A without a reason`);
}
for (const id of ids) {
  const n = seen.get(id) || 0;
  if (n !== 1) problems.push(`${id}: ${n} rows (want exactly 1)`);
}

let pending = 0; let mapped = 0; let na = 0;
for (const [o, c] of Object.entries(counts).sort()) {
  if (only && o !== only) continue;
  console.log(`${o.padEnd(6)} mapped ${String(c.mapped).padStart(4)}  pending ${String(c.pending).padStart(4)}  N/A ${String(c['N/A']).padStart(3)}`);
  pending += c.pending; mapped += c.mapped; na += c['N/A'];
}
console.log(`total: ${mapped} mapped, ${na} N/A, ${pending} unmapped (pending)${only ? ` for ${only}` : ''}; ${ids.size} pytest ids`);
for (const p of problems.slice(0, 50)) console.log(`PROBLEM ${p}`);
process.exit(problems.length || (strict && pending) ? 1 : 0);
