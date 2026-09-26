// Reorder maths shared by drag-and-drop and the keyboard alternatives (pure; never mutates its input).
export function moveItem(list, from, to) {
  const a = list.slice();
  if (from < 0 || from >= a.length) return a;
  const [x] = a.splice(from, 1);
  a.splice(Math.max(0, Math.min(a.length, to)), 0, x);
  return a;
}
// Move the element with `id` next to `targetId`. position: 'before' | 'after'. Unknown ids leave the list unchanged.
export function moveRelative(list, id, targetId, position = 'before') {
  if (id === targetId) return list.slice();
  const from = list.indexOf(id);
  if (from < 0 || !list.includes(targetId)) return list.slice();
  const rest = list.filter((x) => x !== id);
  const at = rest.indexOf(targetId) + (position === 'after' ? 1 : 0);
  rest.splice(at, 0, id);
  return rest;
}
// Keyboard move: delta = -1 (up) / +1 (down), clamped.
export const moveBy = (list, id, delta) => {
  const i = list.indexOf(id);
  return i < 0 ? list.slice() : moveItem(list, i, i + delta);
};
// Pointer position within the hovered element decides before/after (top half = before).
export const dropPosition = (top, height, clientY) => (clientY < top + height / 2 ? 'before' : 'after');
// Instruction `order` ints: renumber in steps of 10 so a later manual insert still has gaps.
export const renumber = (ids) => ids.map((id, i) => ({ id, order: (i + 1) * 10 }));
// Adding to a collection must not create duplicates (multi-membership is by (kind,name)).
export const memberKey = (m) => `${m.kind}/${m.name}`;
export function addMembers(members, incoming) {
  const seen = new Set(members.map(memberKey));
  const out = members.slice();
  for (const m of incoming) if (!seen.has(memberKey(m))) { seen.add(memberKey(m)); out.push({ kind: m.kind, name: m.name }); }
  return out;
}
