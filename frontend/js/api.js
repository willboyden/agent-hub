// REST client for /api/v1 (ARCHITECTURE §5). RFC 7807 problem+json errors become ApiError; every non-GET sends X-Agent-Hub: 1.
import { normalizeItem, normalizeMatrix, normalizeChanges, normalizeHistory, normalizeClient, normalizeEffective, normalizeDrift, normalizeApply, normalizeDiscover, normalizeImportResult, normalizeSettings, normalizeAudit, normalizeInbox, normalizeSuggestion, normalizeBackends } from './adapt.js';

export class ApiError extends Error {
  constructor(status, problem = {}) {
    super(problem.detail || problem.title || `HTTP ${status}`);
    this.name = 'ApiError';
    this.status = status;
    this.code = problem.code || null;
    this.title = problem.title || null;
    this.problem = problem;
  }
  // True only for a 401 from the hub itself: this is the ONLY condition that may open the API-key dialog.
  get isHubAuth() { return this.status === 401 && !KNOWLEDGE_CODES.has(this.code) && !String(this.code || '').startsWith('knowledge_'); }
  get isKnowledgeSetup() { return KNOWLEDGE_CODES.has(this.code); }
}

// Error codes the hub uses when the KNOWLEDGE service (not the hub) refuses or is unreachable. They must never be mistaken for a hub 401.
export const KNOWLEDGE_CODES = new Set(['knowledge_upstream_unauthorized', 'knowledge_unavailable']);
export const CSRF_HEADER = 'X-Agent-Hub';
export const SAFE_METHODS = new Set(['GET', 'HEAD']);
export const KINDS = ['skill', 'agent', 'instruction', 'mcp', 'rule', 'memory'];
// Path segment per kind (contract §5: /skills /agents /instructions /mcp /rules /memory).
export const KIND_PATH = { skill: 'skills', agent: 'agents', instruction: 'instructions', mcp: 'mcp', rule: 'rules', memory: 'memory' };

export function buildQuery(query) {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(query || {})) {
    if (v == null || v === '') continue;
    p.set(k, String(v));
  }
  const s = p.toString();
  return s ? `?${s}` : '';
}

const enc = encodeURIComponent;
const arr = (x) => (Array.isArray(x) ? x : []);

export function createClient({ base = '/api/v1', getKey = () => null, onUnauthorized = () => {}, fetchImpl } = {}) {
  const f = (...a) => (fetchImpl || globalThis.fetch)(...a);
  const authHeaders = () => { const k = getKey(); return k ? { Authorization: `Bearer ${k}` } : {}; };
  const url = (path, query) => `${base}${path}${buildQuery(query)}`;

  async function raw(method, path, { query, body, signal, headers } = {}) {
    const h = { Accept: 'application/json', ...authHeaders(), ...headers };
    // CSRF / DNS-rebinding guard: a custom header a cross-site form or <img> cannot set.
    if (!SAFE_METHODS.has(method.toUpperCase())) h[CSRF_HEADER] = '1';
    let payload;
    if (body !== undefined) { h['Content-Type'] = 'application/json'; payload = JSON.stringify(body); }
    let res;
    try { res = await f(url(path, query), { method, headers: h, body: payload, signal }); }
    catch (e) { if (e?.name === 'AbortError') throw e; throw new ApiError(0, { title: 'Network error', detail: e?.message || 'Cannot reach the Agent Hub backend', code: 'network' }); }
    if (!res.ok) {
      let problem = {};
      try { problem = await res.json(); } catch { /* not json */ }
      const err = new ApiError(res.status, problem);
      if (err.isHubAuth) onUnauthorized();
      throw err;
    }
    return res;
  }
  async function request(method, path, opts) {
    const res = await raw(method, path, opts);
    if (res.status === 204) return null;
    const ct = res.headers.get?.('content-type') || '';
    return ct.includes('json') ? res.json() : res.text();
  }
  const get = (p, q, o) => request('GET', p, { ...o, query: q });
  const post = (p, body, o) => request('POST', p, { ...o, body: body ?? {} });
  const put = (p, body, o) => request('PUT', p, { ...o, body });
  const del = (p, o) => request('DELETE', p, o);

  // Follow `next_cursor` (every list is {items, next_cursor}); bare arrays pass through unchanged.
  async function listAll(path, query, { maxPages = 10 } = {}) {
    const out = [];
    let cursor;
    for (let i = 0; i < maxPages; i++) {
      const page = await get(path, { limit: 200, ...query, cursor });
      if (Array.isArray(page)) return page;
      out.push(...(page.items || []));
      if (!page.next_cursor) break;
      cursor = page.next_cursor;
    }
    return out;
  }
  const kp = (kind) => KIND_PATH[kind] || kind;

  return {
    url, authHeaders, raw, request, get, post, put, del, listAll,
    health: () => get('/health'),
    adapters: () => listAll('/adapters'),
    adapter: (id) => get(`/adapters/${enc(id)}`),
    clients: async () => (await listAll('/clients')).map(normalizeClient),
    client: async (id) => normalizeClient(await get(`/clients/${enc(id)}`)),
    createClient: (body) => post('/clients', body),
    updateClient: (id, body) => put(`/clients/${enc(id)}`, body),
    deleteClient: (id) => del(`/clients/${enc(id)}`),
    validateSpec: (body) => post('/clients/validate-spec', body),
    discoverClient: async (id) => normalizeDiscover(await post(`/clients/${enc(id)}/discover`)),
    importItems: async (body) => normalizeImportResult(await post('/import', body)),
    rendered: (id, kind, name) => get(`/clients/${enc(id)}/rendered`, { kind, name }),
    effective: async (id, source) => normalizeEffective(await get(`/clients/${enc(id)}/effective`, source ? { source } : undefined)),
    verify: (id) => post(`/clients/${enc(id)}/verify`),
    drift: async (id) => normalizeDrift(await get(`/clients/${enc(id)}/drift`)),
    audit: (id) => get(`/clients/${enc(id)}/audit`),
    items: async (kind, q) => (await listAll(`/${kp(kind)}`, q)).map((x) => normalizeItem(x, kind)),
    item: (kind, name) => get(`/${kp(kind)}/${enc(name)}`),
    saveItem: (kind, name, body) => put(`/${kp(kind)}/${enc(name)}`, body),
    deleteItem: (kind, name) => del(`/${kp(kind)}/${enc(name)}`),
    duplicateSkill: (name, newName) => post(`/skills/${enc(name)}/duplicate`, { new_name: newName }),
    collections: () => listAll('/collections'),
    suggestions: async () => arr((await get('/collections/suggestions')).items).map(normalizeSuggestion),
    acceptSuggestions: (ids, overrides) => post('/collections/suggestions/accept', { ids, ...(overrides && Object.keys(overrides).length ? { overrides } : {}) }),
    createCollection: (body) => post('/collections', body),
    updateCollection: (id, body) => put(`/collections/${enc(id)}`, body),
    deleteCollection: (id) => del(`/collections/${enc(id)}`),
    addMembers: (id, members) => post(`/collections/${enc(id)}/members`, { members }),
    removeMember: (id, kind, name) => del(`/collections/${enc(id)}/members/${enc(kind)}/${enc(name)}`),
    reorderCollections: (ids) => post('/collections/reorder', { ids }),
    bulkItems: (body) => post('/items/bulk', body),
    profile: (client) => get(`/profiles/${enc(client)}`),
    saveProfile: (client, body) => put(`/profiles/${enc(client)}`, body),
    matrix: async () => normalizeMatrix(await get('/matrix')),
    matrixToggle: (body) => post('/matrix/toggle', body),
    matrixBulk: (body) => post('/matrix/bulk', body),
    changes: async () => normalizeChanges(await get('/changes')),
    commit: (message) => post('/changes/commit', { message }),
    discard: (paths) => post('/changes/discard', paths ? { paths } : {}),
    history: async () => normalizeHistory(await listAll('/changes/history')),
    revert: (commit) => post('/changes/revert', { commit }),
    plan: (clients) => post('/plan', clients ? { clients } : {}),
    getPlan: (id) => get(`/plan/${enc(id)}`),
    apply: async (plan_id, adopt_paths) => normalizeApply(await post('/apply', { plan_id, confirm: true, ...(adopt_paths?.length ? { adopt_paths } : {}) })),
    floor: () => get('/policy/floor'),
    checkPolicy: (body) => post('/policy/check', body),
    inbox: async () => (await listAll('/memory/inbox')).filter((i) => i.valid !== false).map(normalizeInbox),
    promote: (id, body) => post(`/memory/inbox/${enc(id)}/promote`, body),
    reject: (id) => post(`/memory/inbox/${enc(id)}/reject`),
    auditLog: async (q) => { const p = await get('/audit', q); return { ...p, items: arr(p.items ?? p).map(normalizeAudit) }; },
    doctor: () => get('/doctor'),
    doctorSummary: () => get('/doctor/summary'),
    settings: async () => normalizeSettings(await get('/settings')),
    saveSettings: (body) => put('/settings', body),
    keys: () => get('/keys'),
    createKey: (body) => post('/keys', body),
    deleteKey: (id) => del(`/keys/${enc(id)}`),
    // knowledge proxy (admin token added server-side; the browser stays same-origin)
    kHealth: () => get('/knowledge/health'),  // {reachable, authenticated, backends}
    kBackends: async () => ({ items: normalizeBackends(await get('/knowledge/backends')) }),
    kNamespaces: () => listAll('/knowledge/namespaces'),
    kCreateNamespace: (body) => post('/knowledge/namespaces', body),
    kDeleteNamespace: (ns) => del(`/knowledge/namespaces/${enc(ns)}`),
    kIngest: (ns, body) => post(`/knowledge/namespaces/${enc(ns)}/ingest`, body),
    kJobs: () => listAll('/knowledge/jobs'),
    kQuery: (ns, body) => post(`/knowledge/namespaces/${enc(ns)}/query`, body),
    kTokens: () => listAll('/knowledge/tokens'),
    kCreateToken: (body) => post('/knowledge/tokens', body),
    kDeleteToken: (id) => del(`/knowledge/tokens/${enc(id)}`),
    kIndexes: () => listAll('/knowledge/indexes'),
    kCreateIndex: (body) => post('/knowledge/indexes', body),
    kDeleteIndex: (id) => del(`/knowledge/indexes/${enc(id)}`),
  };
}
