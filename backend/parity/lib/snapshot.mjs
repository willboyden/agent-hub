// Snapshot of the files an implementation wrote: client trees under HOME (what apply delivers), the content repo
// working tree (what PUT/import/commit write) and the hub's state files (locks, apply logs, plans, backups).
// Client files and content files are compared BYTE-FOR-BYTE (sha256 + mode); JSON state files as normalised values.
import { createHash } from 'node:crypto';
import { lstatSync, readdirSync, readFileSync, readlinkSync } from 'node:fs';
import { join, relative } from 'node:path';

import { normString, normValue } from './normalize.mjs';

function walk(dir, base, out, skip) {
  let names;
  try { names = readdirSync(dir).sort(); } catch { return; }
  for (const n of names) {
    const p = join(dir, n);
    const rel = relative(base, p);
    if (skip(rel)) continue;
    const st = lstatSync(p);
    if (st.isSymbolicLink()) { out[normString(rel)] = { link: readlinkSync(p) }; continue; }
    if (st.isDirectory()) { out[`${normString(rel)}/`] = { mode: (st.mode & 0o777).toString(8) }; walk(p, base, out, skip); continue; }
    const data = readFileSync(p);
    const entry = { mode: (st.mode & 0o777).toString(8), size: data.length, sha256: createHash('sha256').update(data).digest('hex') };
    if (/\.json$/.test(n) && /^(locks|applies|plans)\//.test(rel)) {
      try { entry.json = normValue(JSON.parse(data.toString('utf8'))); delete entry.sha256; delete entry.size; } catch { /* keep bytes */ }
    } else if (data.length < 64 * 1024 && !data.includes(0)) {
      entry.text = data.toString('utf8');
      if (/^created: [0-9.]+$/m.test(entry.text)) {
        entry.text = normString(entry.text.replace(/^created: [0-9.]+$/m, 'created: <TS>'));
        delete entry.sha256; delete entry.size;
      }
    }
    const key = normString(rel);
    if (key in out) {
      // two names normalise to the same key (e.g. two plan files): keep both, in a stable order
      const all = [...(Array.isArray(out[key]) ? out[key] : [out[key]]), entry];
      out[key] = all.sort((x, y) => JSON.stringify(x).localeCompare(JSON.stringify(y)));
    } else out[key] = entry;
  }
}

/** { home: {...}, content: {...}, state: {...} } */
export function snapshot(tmp) {
  const home = {}; const content = {}; const state = {};
  walk(join(tmp, 'home'), join(tmp, 'home'), home, () => false);
  walk(join(tmp, 'data', 'content'), join(tmp, 'data', 'content'), content, (rel) => rel === '.git' || rel.startsWith('.git/'));
  walk(join(tmp, 'data'), join(tmp, 'data'), state, (rel) => /^(content|snapshots|skilleval|observed)(\/|$)/.test(rel)
    || /^hub\.db/.test(rel) || /\.key$/.test(rel) || rel === 'hub.log');
  return { home, content, state };
}
