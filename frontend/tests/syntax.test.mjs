import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const walk = (d) => fs.readdirSync(d, { withFileTypes: true }).flatMap((e) => (e.isDirectory() ? walk(path.join(d, e.name)) : e.name.endsWith('.js') || e.name.endsWith('.mjs') ? [path.join(d, e.name)] : []));

test('every browser module and dev script parses (views need a DOM, so they are syntax-checked, not imported)', () => {
  for (const f of [...walk(path.join(root, 'js')), ...walk(path.join(root, 'dev'))]) {
    try { execFileSync(process.execPath, ['--check', f], { stdio: 'pipe' }); } catch (e) { assert.fail(`${path.relative(root, f)}: ${e.stderr}`); }
  }
});

test('every named import between browser modules resolves to a real export', () => {
  const files = walk(path.join(root, 'js')); const bad = [];
  const exportsOf = (f) => { const s = fs.readFileSync(f, 'utf8'); const out = new Set();
    for (const m of s.matchAll(/export\s+(?:async\s+)?(?:function|class|const|let)\s+([\w$]+)/g)) out.add(m[1]);
    for (const m of s.matchAll(/export\s*\{([^}]+)\}/g)) for (const n of m[1].split(',')) out.add(n.trim().split(/\s+as\s+/).pop());
    return out; };
  for (const f of files) {
    const s = fs.readFileSync(f, 'utf8');
    for (const m of s.matchAll(/import\s*\{([^}]+)\}\s*from\s*'(\.[^']+)'/g)) {
      const target = path.resolve(path.dirname(f), m[2]); if (!fs.existsSync(target)) { bad.push(`${path.relative(root, f)}: missing ${m[2]}`); continue; }
      const ex = exportsOf(target);
      for (const n of m[1].split(',').map((x) => x.trim().split(/\s+as\s+/)[0]).filter(Boolean)) if (!ex.has(n)) bad.push(`${path.relative(root, f)}: ${n} is not exported by ${m[2]}`);
    }
  }
  assert.deepEqual(bad, []);
});

test('views never assign to built-in HTMLElement properties (this.id = null becomes the string "null")', () => {
  const bad = [];
  for (const f of walk(path.join(root, 'js/views'))) {
    const s = fs.readFileSync(f, 'utf8');
    for (const m of s.matchAll(/\bthis\.(id|title|hidden|lang|dir|slot|draggable|tabIndex|style|className|children|dataset|part)\s*=[^=]/g)) bad.push(`${path.relative(root, f)}: this.${m[1]} =`);
  }
  assert.deepEqual(bad, []);
});
