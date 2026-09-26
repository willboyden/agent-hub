// Pure helpers for the Changes / Plan / Apply screens.
const WRITE = new Set(['add', 'change', 'remove']);

export const isBlocked = (cp) => !!cp.blocked || (cp.floor_violations || []).length > 0;
export const fileCounts = (cp) => cp.summary || (cp.files || []).reduce((a, f) => { a[f.action] = (a[f.action] || 0) + 1; return a; }, {});
export const writes = (cp) => (cp.files || []).filter((f) => WRITE.has(f.action));
export const conflicts = (cp) => (cp.files || []).filter((f) => f.action === 'conflict');

// What the Apply button will do for the chosen clients: blocked clients are reported, never silently included.
export function applyTotals(plan, selected = null) {
  const sel = (plan?.clients || []).filter((c) => !selected || selected.has(c.client));
  const ok = sel.filter((c) => !isBlocked(c)), blocked = sel.filter(isBlocked);
  return { clients: ok.length, blocked: blocked.length, blockedIds: blocked.map((c) => c.client), files: ok.reduce((n, c) => n + writes(c).length, 0), conflicts: ok.reduce((n, c) => n + conflicts(c).length, 0) };
}
// Apply is allowed only if something can actually be written or adopted, and the user confirmed reading the diffs.
export function canApply(plan, { reviewed, selected = null, adopt = new Set() } = {}) {
  const t = applyTotals(plan, selected);
  if (!reviewed) return { ok: false, reason: 'not_reviewed' };
  if (!t.clients) return { ok: false, reason: t.blocked ? 'all_blocked' : 'no_clients' };
  const adoptable = (plan.clients || []).filter((c) => !isBlocked(c) && (!selected || selected.has(c.client))).reduce((n, c) => n + conflicts(c).filter((f) => adopt.has(f.path)).length, 0);
  if (!t.files && !adoptable) return { ok: false, reason: 'nothing_to_do' };
  return { ok: true, reason: null };
}
export const stale = (plan, currentHash) => !!plan && !!currentHash && plan.content_hash !== currentHash;

// ---- pending changes (git status) ----
const DIRS = { skills: 'skill', agents: 'agent', instructions: 'instruction', mcp: 'MCP server', rules: 'rule', memory: 'memory item', collections: 'collection', profiles: 'profile', clients: 'client' };
export function summarizeChanges(files) {
  const by = {};
  for (const f of files) { const d = f.path.split('/')[0]; by[d] = by[d] || new Set(); by[d].add(d === 'skills' ? f.path.split('/')[1] : f.path); }
  return Object.entries(by).map(([dir, set]) => ({ dir, count: set.size }));
}
// A starting point for the commit message the user is expected to edit.
export function suggestMessage(files) {
  const s = summarizeChanges(files);
  if (!s.length) return '';
  const parts = s.slice(0, 3).map(({ dir, count }) => `${count} ${DIRS[dir] || dir}${count === 1 ? '' : 's'}`);
  return `Update ${parts.join(', ')}${s.length > 3 ? ` and ${s.length - 3} more area(s)` : ''}`;
}
export const validMessage = (m) => { const s = String(m ?? '').trim(); return s.length >= 3 && s.length <= 200; };
