// Pure helpers for the suggested-collections panel.
const KEY = 'ah.sugg.dismissed';
export const loadDismissed = (store = globalThis.localStorage) => { try { return new Set(JSON.parse(store.getItem(KEY) || '[]')); } catch { return new Set(); } };
export const saveDismissed = (set, store = globalThis.localStorage) => { try { store.setItem(KEY, JSON.stringify([...set])); return true; } catch { return false; } };
// Pure: the overrides body for accept (only what the user changed: title and/or trimmed members).
export function buildOverrides(edits, suggestions) {
  const out = {};
  for (const s of suggestions) {
    const e = edits[s.id]; if (!e) continue;
    const o = {};
    if (e.title != null && e.title.trim() && e.title.trim() !== s.title) o.title = e.title.trim();
    if (e.removed?.size) o.members = s.members.filter((m) => !e.removed.has(`${m.kind}/${m.name}`));
    if (Object.keys(o).length) out[s.id] = o;
  }
  return out;
}
export const visibleSuggestions = (list, dismissed) => list.filter((s) => !dismissed.has(s.id));

