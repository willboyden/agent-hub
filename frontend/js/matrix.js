// Matrix state logic. A cell from the API is {state: on|off|via_collection|unsupported|blocked, reason?}.
// Edits are STAGED locally (with undo) and only written to the working tree when the user saves.
export const cellKey = (client, kind, name) => `${client}|${kind}|${name}`;
export const isEnabledState = (s) => s === 'on' || s === 'via_collection';
export const isToggleable = (cell) => !!cell && (cell.state === 'on' || cell.state === 'off' || cell.state === 'via_collection');

// What an unstaged click would set. via_collection -> false means an explicit per-client override (profile.disable).
export const nextEnabled = (cell) => (isToggleable(cell) ? !isEnabledState(cell.state) : null);

export const emptyModel = () => ({ staged: {}, undo: [] });

export function displayState(cell, staged, key) {
  const s = staged?.[key];
  if (!s) return { state: cell?.state ?? 'unsupported', reason: cell?.reason, pending: false };
  return { state: s.enabled ? 'on' : 'off', reason: cell?.reason, pending: true };
}

const withEntry = (staged, key, entry, original) => {
  const next = { ...staged };
  // Staging back to what the server already has means "no change": drop the entry rather than keep a no-op.
  if (entry.enabled === original) delete next[key]; else next[key] = entry;
  return next;
};

export function stageToggle(model, target, cell) {
  if (!isToggleable(cell)) return model;
  const key = cellKey(target.client, target.kind, target.name);
  const original = isEnabledState(cell.state);
  const cur = model.staged[key] ? model.staged[key].enabled : original;
  const staged = withEntry(model.staged, key, { ...target, enabled: !cur }, original);
  return { staged, undo: [...model.undo, model.staged] };
}

// targets: [{client, kind, name, cell}] — one undo step for the whole batch. Non-toggleable cells are skipped.
export function stageMany(model, targets, enabled) {
  let staged = model.staged;
  let changed = false;
  for (const t of targets) {
    if (!isToggleable(t.cell)) continue;
    const key = cellKey(t.client, t.kind, t.name);
    const original = isEnabledState(t.cell.state);
    if ((staged[key] ? staged[key].enabled : original) === enabled) continue; // already there: not a change
    const next = withEntry(staged, key, { client: t.client, kind: t.kind, name: t.name, enabled }, original);
    if (next !== staged) { staged = next; changed = true; }
  }
  return changed ? { staged, undo: [...model.undo, model.staged] } : model;
}

export function undo(model) {
  if (!model.undo.length) return model;
  return { staged: model.undo[model.undo.length - 1], undo: model.undo.slice(0, -1) };
}
export const stagedCount = (model) => Object.keys(model.staged).length;

// Header toggles: if any toggleable target is currently OFF (after staging) turn all on, else turn all off.
export function bulkTarget(targets, model) {
  const live = targets.filter((t) => isToggleable(t.cell));
  if (!live.length) return null;
  const anyOff = live.some((t) => {
    const s = model.staged[cellKey(t.client, t.kind, t.name)];
    return !(s ? s.enabled : isEnabledState(t.cell.state));
  });
  return anyOff;
}

// Collapse staged entries into the fewest POST /matrix/bulk calls: same `enabled` and the same client set per item.
export function toBulkCalls(staged) {
  const byItem = new Map();
  for (const e of Object.values(staged)) {
    const k = `${e.enabled}|${e.kind}|${e.name}`;
    if (!byItem.has(k)) byItem.set(k, { enabled: e.enabled, item: { kind: e.kind, name: e.name }, clients: [] });
    byItem.get(k).clients.push(e.client);
  }
  const groups = new Map();
  for (const v of byItem.values()) {
    const k = `${v.enabled}|${[...v.clients].sort().join(',')}`;
    if (!groups.has(k)) groups.set(k, { clients: [...v.clients].sort(), items: [], enabled: v.enabled });
    groups.get(k).items.push(v.item);
  }
  return [...groups.values()];
}

// Human summary counts for the unsaved bar / plan preview.
export function summarize(staged) {
  let on = 0, off = 0;
  for (const e of Object.values(staged)) e.enabled ? on++ : off++;
  return { on, off, total: on + off };
}
