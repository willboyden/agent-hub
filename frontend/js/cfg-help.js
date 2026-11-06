// Field help for the config editor's Form view and the Settings fields: what a key is, what changing it does, what
// it accepts, its default and when a change takes effect. The text lives in i18n/fields.en.json, one catalog of path
// patterns per config file (`hub-env`, `knowledge`, `client-spec`, `floor`) plus `settings` (Settings > General).
// Pure logic, no DOM, so tests import it. Ported from Nemo Console's cfg-help.js.
//
// Pattern segments: a literal key, `*` = any one mapping key, `#` = any list index, `**` = any number of further levels
// (only as the last segment). The most specific pattern wins: literal segments outweigh wildcards.

export const FIELDS_URL = 'i18n/fields.en.json';
export const ENTRY_KEYS = ['what', 'change', 'accepts', 'default', 'effect', 'choices'];
let pending = null;
/** The catalog document, fetched once per page load (a failed fetch is retried next time). */
export function loadFieldHelp(url = FIELDS_URL) {
  pending ||= fetch(url).then((r) => {
    if (!r.ok) throw new Error(`field help load failed: ${r.status}`);
    return r.json();
  }).catch((e) => { pending = null; throw e; });
  return pending;
}

/** The catalog of one file id, ready for lookupHelp(), or null. */
export function catalogFor(doc, id) {
  const cat = doc?.catalogs?.[id];
  if (!cat) return null;
  return { entries: Object.entries(cat).map(([pat, entry], order) => ({ segs: pat.split('.'), entry, order })), upper: id === 'hub-env' };
}

/** How well a pattern fits a path: -1 = no match; higher = more specific. */
export function matchScore(segs, path) {
  let score = 0;
  for (let i = 0; i < segs.length; i++) {
    const s = segs[i];
    if (s === '**') return i === segs.length - 1 && path.length > i ? score + 1 : -1;
    if (i >= path.length) return -1;
    const p = path[i];
    if (s === '#') { if (typeof p !== 'number') return -1; score += 2; }
    else if (s === '*') { if (typeof p !== 'string') return -1; score += 2; }
    else if (String(p) !== s || typeof p === 'number') return -1;
    else score += 4;
  }
  return segs.length === path.length ? score + 2 : -1;
}

/** The best catalog entry for a path (keys are strings, list indexes numbers), or null. Env names ignore case. */
export function lookupHelp(cat, path) {
  if (!cat) return null;
  const p = cat.upper ? path.map((x) => (typeof x === 'string' ? x.toUpperCase() : x)) : path;
  let best = null, bestScore = -1;
  for (const e of cat.entries) {
    const sc = matchScore(e.segs, p);
    if (sc > bestScore || (sc === bestScore && best && e.order < best.order)) { best = e; bestScore = sc; }
  }
  return bestScore >= 0 ? best.entry : null;
}
