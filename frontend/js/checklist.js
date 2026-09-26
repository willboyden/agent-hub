// First-run checklist state (pure), derived from server truth: import = library not empty; organise = a collection exists;
// review = a plan was viewed for the CURRENT content hash; apply = some client has a real last_applied_at.
export function checklist({ itemCount = 0, collectionCount = 0, viewedHash = '', contentHash = '', appliedCount = 0 } = {}) {
  const steps = [
    { id: 'import', done: itemCount > 0, href: '#/import' },
    { id: 'organise', done: collectionCount > 0, href: '#/collections' },
    { id: 'plan', done: !!viewedHash && viewedHash === contentHash, href: '#/changes?tab=plan' },
    { id: 'apply', done: appliedCount > 0, href: '#/changes?tab=plan' },
  ];
  const next = steps.findIndex((s) => !s.done);
  return steps.map((s, i) => ({ ...s, next: i === next }));
}
// The hash lives in localStorage (may throw in private mode).
const KEY = 'ah.plan.viewed';
export const rememberPlanViewed = (hash, store = globalThis.localStorage) => { try { store.setItem(KEY, String(hash)); return true; } catch { return false; } };
export const readPlanViewed = (store = globalThis.localStorage) => { try { return store.getItem(KEY) || ''; } catch { return ''; } };
