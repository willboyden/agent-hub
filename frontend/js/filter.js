// Library filtering/sorting/fuzzy search (pure). An item: {kind, name, title, description, tags, groups, source, issues, enabled_for, updated_at}.
import { fuzzyScore } from './fuzzy.js';

export const itemId = (it) => `${it.kind}/${it.name}`;
export const EMPTY_FILTERS = { q: '', kind: '', collection: '', tag: '', client: '', enabled: '', source: '', issues: '' };

const hay = (it) => [it.name, it.title, it.description, (it.tags || []).join(' ')].filter(Boolean).join(' ');
// Name matches count more than description matches; tags in between.
export function searchScore(q, it) {
  const s = q.trim();
  if (!s) return 0;
  const parts = s.split(/\s+/);
  let total = 0;
  for (const p of parts) {
    const a = fuzzyScore(p, it.name || '');
    const b = fuzzyScore(p, it.title || '');
    const c = fuzzyScore(p, (it.tags || []).join(' '));
    const d = fuzzyScore(p, it.description || '');
    const best = Math.max(a >= 0 ? a * 3 : -1, b >= 0 ? b * 2.5 : -1, c >= 0 ? c * 2 : -1, d >= 0 ? d : -1);
    if (best < 0) return -1; // every word must match somewhere
    total += best;
  }
  return total;
}

export function filterItems(items, f = {}) {
  const o = { ...EMPTY_FILTERS, ...f };
  return items.filter((it) => {
    if (o.kind && it.kind !== o.kind) return false;
    if (o.collection && !(it.groups || []).includes(o.collection)) return false;
    if (o.tag && !(it.tags || []).includes(o.tag)) return false;
    if (o.source && (it.source || 'local') !== o.source) return false;
    if (o.issues === '1' && !(it.issues || []).length) return false;
    if (o.issues === '0' && (it.issues || []).length) return false;
    const en = it.enabled_for || [];
    if (o.client && o.enabled === 'on' && !en.includes(o.client)) return false;
    if (o.client && o.enabled === 'off' && en.includes(o.client)) return false;
    if (o.client && !o.enabled && !en.includes(o.client)) return false; // "client" alone = "what this client has"
    if (!o.client && o.enabled === 'on' && !en.length) return false;
    if (!o.client && o.enabled === 'off' && en.length) return false;
    if (o.q && searchScore(o.q, it) < 0) return false;
    return true;
  });
}

const cmp = {
  name: (a, b) => a.name.localeCompare(b.name),
  updated: (a, b) => (b.updated_at || 0) - (a.updated_at || 0) || a.name.localeCompare(b.name),
  kind: (a, b) => a.kind.localeCompare(b.kind) || a.name.localeCompare(b.name),
  issues: (a, b) => (b.issues || []).length - (a.issues || []).length || a.name.localeCompare(b.name),
  enabled: (a, b) => (b.enabled_for || []).length - (a.enabled_for || []).length || a.name.localeCompare(b.name),
};
export function sortItems(items, sort = 'name', q = '') {
  const out = items.slice();
  // With a search query, relevance wins unless the user picked an explicit sort other than the default.
  if (q.trim() && sort === 'relevance') return out.map((it) => ({ it, s: searchScore(q, it) })).sort((a, b) => b.s - a.s || a.it.name.localeCompare(b.it.name)).map((x) => x.it);
  return out.sort(cmp[sort] || cmp.name);
}

// Facet values for the filter drop-downs (sorted, with counts).
export function facets(items) {
  const count = (get) => {
    const m = new Map();
    for (const it of items) for (const v of get(it)) m.set(v, (m.get(v) || 0) + 1);
    return [...m.entries()].sort((a, b) => a[0].localeCompare(b[0])).map(([value, n]) => ({ value, n }));
  };
  return { tags: count((i) => i.tags || []), sources: count((i) => [i.source || 'local']), kinds: count((i) => [i.kind]) };
}
export const activeFilterCount = (f) => Object.entries(f).filter(([k, v]) => k !== 'q' && v).length;
