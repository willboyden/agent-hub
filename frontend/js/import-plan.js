// Import wizard logic. A discovered item's `conflict` is null (new) or {existing_source, equal, differences}.
// Per-item choice: import (new) | link (enable the existing canonical item for this client) | rename | replace | skip.
export const itemKey = (i) => `${i.kind}/${i.name}`;
export const CHOICES = ['link', 'rename', 'replace', 'skip'];
// Default: link when the existing item is equal, otherwise skip (the differences are shown so the user can decide).
export const defaultChoice = (i) => (!i.conflict ? 'import' : i.conflict.equal ? 'link' : 'skip');
export const defaultChoices = (items) => Object.fromEntries(items.map((i) => [itemKey(i), defaultChoice(i)]));
export const defaultSelection = (items) => new Set(items.map(itemKey));
export function previewImport(items, selected, choices) {
  return items.filter((i) => selected.has(itemKey(i))).map((i) => {
    const action = i.conflict ? (choices[itemKey(i)] || defaultChoice(i)) : 'import';
    return { kind: i.kind, name: i.name, conflict: i.conflict, action, to: action === 'rename' ? `${i.name}-2` : i.name };
  });
}
// Body items for POST /import: only conflicting items carry an explicit on_conflict.
export const importItems = (preview) => preview.map((p) => ({ kind: p.kind, name: p.name, ...(p.conflict ? { on_conflict: p.action } : {}) }));
export function importSummary(preview) {
  const c = { import: 0, link: 0, skip: 0, rename: 0, replace: 0 };
  for (const p of preview) c[p.action]++;
  return { ...c, total: preview.length, writes: c.import + c.rename + c.replace, links: c.link };
}
export const conflictCount = (items, selected) => items.filter((i) => selected.has(itemKey(i)) && i.conflict).length;
export const setAll = (items, selected, choice) => Object.fromEntries(items.filter((i) => i.conflict && selected.has(itemKey(i))).map((i) => [itemKey(i), choice]));
