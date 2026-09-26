// Shared data loading helpers for views. Item lists come from six endpoints (contract §5) and are merged with their kind.
import { api } from './app-context.js';
import { KINDS } from './api.js';

export async function loadAllItems(kinds = KINDS) {
  const lists = await Promise.all(kinds.map((k) => api.items(k)));
  return lists.flatMap((l, i) => l.map((it) => ({ ...it, kind: kinds[i] })));
}
export const byId = (list, key = 'id') => Object.fromEntries(list.map((x) => [x[key], x]));
export const editHref = (kind, name) => `#/edit/${kind}/${encodeURIComponent(name)}`;
// Secrets are shown by the server as "[set]" / "[set, N chars]" and must never be sent back.
export const isRedacted = (v) => typeof v === 'string' && /^\[set(, \d+ chars)?\]$/.test(v);
export const stripRedacted = (obj) => Object.fromEntries(Object.entries(obj).filter(([, v]) => !isRedacted(v)));
export const listOf = (x) => (Array.isArray(x) ? x : x?.items || []);
