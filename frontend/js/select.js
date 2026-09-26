// Multi-select semantics for lists/cards: click = only this, ctrl/meta = toggle, shift = range from the anchor.
// `order` is the ids currently visible (after filtering), so a range never selects hidden items.
export function applySelection(state, order, id, { shift = false, ctrl = false } = {}) {
  const ids = new Set(state.ids || []);
  let anchor = state.anchor ?? null;
  if (shift && anchor != null && order.includes(anchor) && order.includes(id)) {
    const a = order.indexOf(anchor), b = order.indexOf(id);
    const [lo, hi] = a < b ? [a, b] : [b, a];
    // Shift-click extends the selection (does not clear what ctrl-click added), like file managers.
    for (let i = lo; i <= hi; i++) ids.add(order[i]);
    return { ids, anchor };
  }
  if (ctrl) { if (ids.has(id)) ids.delete(id); else ids.add(id); return { ids, anchor: id }; }
  if (ids.size === 1 && ids.has(id)) return { ids: new Set(), anchor: id };
  return { ids: new Set([id]), anchor: id };
}
// Keep only ids still visible/existing (after a filter change or a reload).
export function pruneSelection(state, order) {
  const keep = new Set(order);
  return { ids: new Set([...state.ids].filter((i) => keep.has(i))), anchor: keep.has(state.anchor) ? state.anchor : null };
}
export const selectAll = (order) => ({ ids: new Set(order), anchor: order[0] ?? null });
