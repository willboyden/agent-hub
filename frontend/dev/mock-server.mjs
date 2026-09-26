#!/usr/bin/env node
// Dependency-free mock of the Agent Hub API plus a static host for the frontend. Response SHAPES mirror the backend source as read from
// hub/backend/src/agent_hub (api/routers/*, services/hub.py, organize.py, importer.py, planner.py ...): where the architecture sketch and the
// code differ, the code wins.
// Usage: node dev/mock-server.mjs [--port 8794] [--host 127.0.0.1] [--empty] [--trust]      (MOCK_KEY=secret requires a bearer key)
//        --proxy http://127.0.0.1:8792  forwards /api and /metrics to a real backend (screenshots only), keeping the static host.
// Reproduced: RFC 7807 errors with `code`, {items,next_cursor} lists, X-Agent-Hub CSRF header (403), JSON-only bodies (415), body cap (413),
// Host allowlist (421), same-origin check, plan_stale (409), conflicts, redaction of secret-looking text in diffs, one-time secrets.
// All data is invented (dev/mock-data.mjs). Nothing here is a verified fact about the real backend.
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';
import * as D from './mock-data.mjs';
import * as C from './mock-core.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const argv = (n, d) => { const i = args.indexOf(`--${n}`); return i >= 0 ? args[i + 1] : d; };
const PORT = Number(argv('port', process.env.PORT ?? 8794));
const HOST = argv('host', '127.0.0.1');
const KEY = process.env.MOCK_KEY || '';
const PROXY = argv('proxy', process.env.MOCK_PROXY || '');
const EMPTY = args.includes('--empty') || process.env.MOCK_EMPTY === '1';    // first-run state: nothing imported yet
const TRUST = args.includes('--trust') || process.env.MOCK_TRUST === '1';    // show the loopback-trust warning banner
const BODY_CAP = 1 << 20;
const uid = (p) => `${p}${crypto.randomBytes(5).toString('hex')}`;
const now = C.now;
const KINDS = C.KINDS;
const PLURAL = C.PK;

// ---------------- state ----------------
let content = C.seedContent();
if (EMPTY) { for (const k of Object.keys(content.items)) content.items[k] = {}; content.collections = []; content.profiles = {}; content.clients = {}; }
let base = structuredClone(content);          // the last commit: plans, effective, drift, verify all read THIS (as the backend does)
const applied = {}, live = {}, locks = new Set();
const plans = new Map();
const sha40 = () => crypto.randomBytes(20).toString('hex');
let history = EMPTY ? [] : [
  { commit: sha40(), message: 'Import skills from hermes', author: 'admin', time: Math.floor(now() - 86400 * 4) },
  { commit: sha40(), message: 'Add Operations collection', author: 'admin', time: Math.floor(now() - 86400 * 3) },
  { commit: sha40(), message: 'Enable security collection for claude-code', author: 'admin', time: Math.floor(now() - 86400 * 1.2) },
  { commit: sha40(), message: 'Pin fetch MCP server to 3c55f80', author: 'admin', time: Math.floor(now() - 3600 * 9) },
];
const auditRows = [];
let auditSeq = 0;
const audit = (method, p, status, params = {}, ts = now()) => { auditRows.unshift({ id: ++auditSeq, ts, actor: 'admin', role: 'admin', method, path: p, status, params }); if (auditRows.length > 500) auditRows.pop(); };
const inbox = EMPTY ? [] : D.INBOX.map((i) => ({ id: i.id, client: i.client, type: i.type, title: i.title, body: i.body, created: i.created_at, valid: true }));
const limits = { plan_retention: 20, inbox_max_per_client: 200, inbox_rate_per_min: 30 };
const keys = [{ id: 'key_1', name: 'bootstrap', role: 'admin', prefix: 'hc_3fa9Xq', client: null, scopes: [], created_at: now() - 86400 * 6, last_used_at: now() - 300 }, { id: 'key_2', name: 'hermes-token', role: 'client', prefix: 'hc_71c2Lm', client: 'hermes', scopes: ['inbox:write', 'knowledge:read:team-docs'], created_at: now() - 86400 * 2, last_used_at: null }];
const K = structuredClone(D.KNOWLEDGE);
if (EMPTY) { K.namespaces = []; K.tokens = []; K.indexes = []; K.docs = []; }
const jobs = [{ id: 'job_1', namespace: 'team-docs', state: 'done', total: 120, done: 120, created_at: now() - 7200, error: null }];
const jobStreams = new Map();

function seedApplied() {
  if (EMPTY) return;
  for (const cl of D.CLIENTS) {
    const id = cl.id;
    if (cl.status === 'never_applied') { applied[id] = {}; live[id] = {}; continue; }
    locks.add(id);
    const r = C.renderClient(content, id);
    const m = Object.fromEntries(r.files.filter((f) => f.managed).map((f) => [`${f.root}:${f.path}`, f.text]));
    const keysAll = Object.keys(m).filter((k) => k.includes('SKILL.md'));
    if (id !== 'opencode') {
      // A few files the hub would add / change / remove, so the plan has something to show.
      delete m[keysAll[1]]; delete m[keysAll[4]];
      m[keysAll[2]] = m[keysAll[2]].replace('## Steps', '## Steps (older wording)');
      m[keysAll[6]] = m[keysAll[6]] + '\n- Legacy note removed in the new version.\n';
      const L = C.layoutFor(content.clients[id]);
      m[`${L.root}:${L.skills}/old-skill/SKILL.md`] = '---\nname: old-skill\ndescription: An old skill that is no longer enabled\n---\n\n# old skill\n';
    }
    applied[id] = { ...m }; live[id] = { ...m };
    if (id === 'claude-code') { const k = Object.keys(m).find((x) => x.endsWith('CLAUDE.md')); if (k) live[id][k] = m[k].replace('<!-- agent-hub:end -->', '\n## Local note (edited by hand)\nDo not remove me.\n<!-- agent-hub:end -->'); }
    if (id === 'hermes') { live[id][keysAll[0]] = (m[keysAll[0]] || '') + '\n<!-- tweaked by the sync script -->\n'; }
  }
}
seedApplied();
if (!EMPTY) for (let i = 0; i < 24; i++) audit(['PUT', 'POST', 'POST', 'POST', 'POST', 'DELETE'][i % 6], ['/api/v1/skills/code-review', '/api/v1/matrix/toggle', '/api/v1/plan', '/api/v1/apply', '/api/v1/changes/commit', '/api/v1/collections/ops/members/skill/a'][i % 6], i % 11 === 5 ? 409 : 200, { keys: ['x'] }, now() - 3600 * (i * 5 + 1));

// ---------------- helpers ----------------
const MIME = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.mjs': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8', '.json': 'application/json', '.svg': 'image/svg+xml', '.ico': 'image/x-icon' };
const CSP = "default-src 'self'; connect-src 'self'; img-src 'self' data: blob:; style-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'";
const json = (res, code, obj) => { res.writeHead(code, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' }); res.end(JSON.stringify(obj)); };
const noContent = (res) => { res.writeHead(204); res.end(); };
const problem = (res, code, title, detail, c, extra = {}) => { const cc = c || title.toLowerCase().replace(/\W+/g, '_'); res.writeHead(code, { 'Content-Type': 'application/problem+json' }); res.end(JSON.stringify({ type: `urn:agent-hub:${cc}`, title, status: code, detail, code: cc, ...extra })); };
// 422 in the backend's shape: errors:[{path, message, fix_hint}]
const invalid = (res, errors, detail) => problem(res, 422, 'Unprocessable', detail || errors.map((e) => `${e.path}: ${e.message}`).join('; '), 'validation_error', { errors: errors.map((e) => ({ path: e.path, message: e.message, fix_hint: e.fix_hint ?? null })) });
class HttpErr extends Error { constructor(status, title, detail, code, extra) { super(detail); Object.assign(this, { status, title, detail, code, extra }); } }
const readBody = (req) => new Promise((ok, no) => {
  const c = []; let n = 0;
  req.on('data', (d) => { n += d.length; if (n > BODY_CAP) { no(new HttpErr(413, 'Payload too large', `Request body exceeds ${BODY_CAP >> 10} KiB`, 'payload_too_large')); req.destroy(); } else c.push(d); });
  req.on('end', () => { const raw = Buffer.concat(c).toString(); if (!raw) return ok(null); try { ok(JSON.parse(raw)); } catch { no(new HttpErr(400, 'Bad JSON', 'Request body is not valid JSON', 'bad_json')); } });
  req.on('error', no);
});
function paginate(items, url, max = 500, dflt = 100) {
  const limit = Math.max(1, Math.min(max, Number(url.searchParams.get('limit') || dflt)));
  const off = Number(url.searchParams.get('cursor') || 0) || 0;
  return { items: items.slice(off, off + limit), next_cursor: off + limit < items.length ? String(off + limit) : null };
}
const arr = (x) => (Array.isArray(x) ? x : []);
const bad = (res, detail, code = 'bad_request') => problem(res, 400, 'Bad request', detail, code);
const nf = (r, what) => r.problem(404, 'Not found', what, 'not_found');
const clientIds = (c = content) => Object.keys(c.clients).sort();
const adapterOf = (id, c = content) => D.ADAPTERS.find((a) => a.id === c.clients[id].adapter);
const capsOf = (id, c = content) => c.clients[id].caps || adapterOf(id, c).caps;
const DEFAULT_MANAGE = { skills: true, agents: true, instructions: true, mcp: false, permissions: false, memory: false };
const treeHash = (c) => C.contentHash(c);

// ---------------- items ----------------
const issuesFor = (kind, it) => {
  const out = [];
  if (kind === 'skill') { if ((it.description || '').length < 20) out.push({ path: 'description', message: 'is too short to tell an agent when to use this skill (min 20 characters)', fix_hint: 'Say when the skill applies.' }); if (it.name === 'csv-wrangler') out.push({ path: 'body', message: 'has no example section', fix_hint: null }); if (it.name === 'docs-alt-text') out.push({ path: 'references/labels.json', message: 'is 480 KiB; large files slow every sync', fix_hint: null }); }
  if (kind === 'mcp' && it.scan_status !== 'clean') out.push({ path: 'scan_status', message: it.scan_status === 'findings' ? 'the scan found problems; it cannot be enabled' : 'not scanned yet', fix_hint: `Run hub scan-mcp ${it.name}` });
  return out;
};
const enabledMap = (c = content) => { const m = {}; for (const id of clientIds(c)) { const res = C.resolveClient(c, id); for (const kind of KINDS) for (const n of res[kind].on) (m[`${kind}/${n}`] ||= []).push(id); } return m; };
function summary(kind, it, emap) {
  const colls = content.collections.filter((k) => k.members.some((m) => m.kind === kind && m.name === it.name)).map((k) => k.id).sort();
  const issues = issuesFor(kind, it);
  return { kind, name: it.name, title: it.title || it.name, description: String(it.description || it.title || '').slice(0, 300), tags: it.tags || [], groups: [], collections: colls, source: it.source || 'local', valid: !(kind === 'skill' && (it.description || '').length < 20), issues, updated: it.updated_at, enabled_for: (emap[`${kind}/${it.name}`] || []).slice().sort() };
}
function metaOf(kind, it) {
  switch (kind) {
    case 'skill': return { name: it.name, description: it.description };
    case 'agent': return { name: it.name, description: it.description, capabilities: it.capabilities, model_tier: it.model_tier, mode: it.mode, read_only: it.read_only, tags: it.tags || [] };
    case 'instruction': return { id: it.name, title: it.title, order: it.order, applies_to: it.applies_to || [], tags: it.tags || [] };
    case 'mcp': return { name: it.name, transport: it.transport, command: it.command, args: it.args, url: it.url, env_names: it.env_names, sandbox_profile: it.sandbox_profile, egress_hosts: it.egress_hosts, scan_status: it.scan_status, pinned_ref: it.pinned_ref, groups: [], tags: it.tags || [], notes: it.notes || '' };
    case 'rule': return { id: it.name, title: it.title, kind: it.kind, match: it.match, decision: it.decision, reason: it.reason || null, applies_to: it.applies_to || [], tags: it.tags || [] };
    default: return { id: it.name, type: it.type, title: it.title, tags: it.tags || [] };
  }
}
function detail(kind, it) {
  const out = { ...summary(kind, it, enabledMap()), meta: metaOf(kind, it), body: it.body || '' };
  if (kind === 'skill') {
    const files = { 'SKILL.md': `---\nname: ${it.name}\ndescription: ${it.description}\n---\n\n${it.body}`, ...Object.fromEntries((it.files || []).map((f) => [f.path, f.content])) };
    out.file_list = Object.entries(files).map(([p, v]) => ({ path: p, size: v.length, binary: false })); out.files = files;
    out.sidecar = { groups: [], tags: it.tags || [], notes: it.notes || '', source: it.source || '', provenance: it.provenance || {} };
  }
  return out;
}
function listItems(kind, url) {
  const q = (url.searchParams.get('q') || '').toLowerCase(), g = url.searchParams.get('group'), tag = url.searchParams.get('tag'), cl = url.searchParams.get('client'), en = url.searchParams.get('enabled'), src = url.searchParams.get('source'), iss = url.searchParams.get('issues');
  const emap = enabledMap();
  let l = Object.values(content.items[kind]).map((it) => summary(kind, it, emap));
  l = l.filter((x) => (!q || `${x.name} ${x.title} ${x.description} ${x.tags.join(' ')}`.toLowerCase().includes(q)) && (!g || x.collections.includes(g)) && (!tag || x.tags.includes(tag)) && (!src || x.source.startsWith(src))
    && (!iss || (iss === 'true') === (x.issues.length > 0)) && (!cl || (en === 'false' ? !x.enabled_for.includes(cl) : x.enabled_for.includes(cl))) && (!(en && !cl) || (en === 'true') === (x.enabled_for.length > 0)));
  const sort = url.searchParams.get('sort') || 'name';
  if (!['name', 'title', 'updated'].includes(sort)) throw new HttpErr(400, 'Bad request', 'sort must be name, title or updated', 'bad_request');
  return l.sort(sort === 'updated' ? (a, b) => b.updated - a.updated || a.name.localeCompare(b.name) : sort === 'title' ? (a, b) => a.title.localeCompare(b.title) : (a, b) => a.name.localeCompare(b.name));
}
// PUT validation: the backend models FORBID unknown keys, so each kind accepts exactly its own fields.
const ID = /^[a-z0-9][a-z0-9._-]{0,63}$/;
const ALLOWED = { skill: ['name', 'description', 'body', 'files', 'files_b64', 'groups', 'tags', 'notes', 'source', 'provenance'], agent: ['name', 'description', 'body', 'capabilities', 'model_tier', 'mode', 'read_only', 'tags'],
  instruction: ['id', 'title', 'body', 'order', 'applies_to', 'tags'], mcp: ['name', 'transport', 'command', 'args', 'url', 'env_names', 'sandbox_profile', 'egress_hosts', 'scan_status', 'pinned_ref', 'groups', 'tags', 'notes'],
  rule: ['id', 'title', 'kind', 'match', 'decision', 'reason', 'applies_to', 'tags'], memory: ['id', 'type', 'title', 'body', 'tags'] };
const ENUM = { model_tier: ['fast', 'standard', 'deep'], mode: ['subagent', 'primary'], decision: ['allow', 'ask', 'deny'], transport: ['stdio', 'http', 'sse'], type: ['user', 'feedback', 'project', 'reference'], kind: ['tool', 'command', 'path_read', 'path_write', 'egress_host', 'mcp_server'] };
function validateItem(kind, name, b) {
  const errs = []; const e = (p, m) => errs.push({ path: p, message: m });
  if (!ID.test(name)) throw new HttpErr(400, 'Bad request', 'invalid name', 'invalid_id');
  for (const k of Object.keys(b)) if (!ALLOWED[kind].includes(k)) e(k, 'Extra inputs are not permitted');
  for (const [k, vals] of Object.entries(ENUM)) if (b[k] != null && ALLOWED[kind].includes(k) && !vals.includes(b[k])) e(k, `Input should be ${vals.map((v) => `'${v}'`).join(', ')}`);
  for (const t of arr(b.tags)) if (!ID.test(t)) e('tags', `invalid identifier ${JSON.stringify(t)}`);
  if (kind === 'skill') { if (b.description !== undefined && !String(b.description).trim()) e('description', 'String should have at least 1 character'); if (String(b.description || '').length > 1024) e('description', 'String should have at most 1024 characters'); for (const p of Object.keys(b.files || {})) if (/^\/|\.\.|\\|\0/.test(p) || p === 'SKILL.md') e(`files.${p}`, 'unsafe or reserved file path'); }
  if (kind === 'agent') for (const c of arr(b.capabilities)) if (!D.CAPS.includes(c)) e('capabilities', `unknown capability ${c}`);
  if (kind === 'mcp') { if ((b.transport || 'stdio') === 'stdio' && !b.command) e('transport', 'stdio transport needs `command`'); if (b.transport && b.transport !== 'stdio' && !b.url) e('transport', `${b.transport} transport needs \`url\``); for (const n of arr(b.env_names)) if (!/^[A-Z_][A-Z0-9_]*$/.test(n)) e('env_names', 'env var NAMES expected, like API_TOKEN (values never live in the hub)'); for (const h of arr(b.egress_hosts)) if (/\s/.test(h)) e('egress_hosts', 'invalid host pattern'); }
  if (kind === 'rule' && !b.match) e('match', 'String should have at least 1 character');
  if ((kind === 'instruction' || kind === 'rule' || kind === 'memory') && b.title !== undefined && !String(b.title).trim()) e('title', 'String should have at least 1 character');
  return errs;
}
function saveItem(kind, name, b, existing) {
  const cur = existing || { name, tags: [], notes: '', source: 'local', provenance: {}, description: '', body: '', files: [], updated_at: now() };
  const next = { ...cur, ...b, name, updated_at: now() };
  delete next.id; delete next.groups; delete next.files_b64;
  if (kind === 'skill' && b.files) next.files = Object.entries(b.files).map(([p, c]) => ({ path: p, content: String(c) }));
  if (kind === 'skill' && !existing) next.description = b.description || '';
  if (kind === 'instruction' && next.applies_to == null) next.applies_to = [];
  content.items[kind][name] = next;
  return next;
}
function removeItem(kind, name) {
  delete content.items[kind][name];
  for (const k of content.collections) k.members = k.members.filter((m) => !(m.kind === kind && m.name === name));
  for (const p of Object.values(content.profiles)) { p.enable[PLURAL[kind]] = arr(p.enable[PLURAL[kind]]).filter((n) => n !== name); p.disable[PLURAL[kind]] = arr(p.disable[PLURAL[kind]]).filter((n) => n !== name); }
}

// ---------------- clients ----------------
// status mirrors planner.drift(): never_applied when no lock and something is out of sync, error when blocked, drift when files differ.
function fileState(f, id) { return f.action === 'advisory' ? 'advisory' : f.action === 'unchanged' ? 'in_sync' : f.action === 'add' ? (locks.has(id) ? 'missing' : 'never_applied') : f.action === 'change' ? 'pending' : f.action === 'remove' ? 'orphaned' : 'changed_live'; }
function driftOf(id) {
  const p = C.planClient(base, id, applied, live);
  const rows = p.files.map((f) => ({ root: f.root, path: f.path, state: fileState(f, id), kind: f.kind, reason: f.reason }));
  const managed = rows.filter((r) => r.state !== 'advisory');
  let status;
  if (!locks.has(id) && managed.some((r) => r.state !== 'in_sync')) status = 'never_applied';
  else if (p.blocked) status = 'error';
  else if (managed.some((r) => r.state !== 'in_sync')) status = 'drift';
  else status = 'in_sync';
  if (!locks.has(id) && !managed.length && !p.blocked) status = 'in_sync';
  return { client: id, status, files: rows };
}
const appliedAt = { 'claude-code': now() - 3600 * 5, 'hermes': now() - 3600 * 50 };
const adoptedAt = { opencode: now() - 3600 * 1, 'claude-code': now() - 3600 * 80, 'hermes': now() - 3600 * 80 };
function clientStatus(id) {
  const d = driftOf(id);
  const edited = d.files.filter((f) => f.state === 'changed_live').map((f) => ({ root: f.root, path: f.path, reason: f.reason || 'live file differs from what the hub last wrote' }));
  const pending = d.files.filter((f) => ['pending', 'missing', 'orphaned', 'never_applied'].includes(f.state)).length;
  const status = d.status === 'error' ? 'error' : d.status === 'never_applied' ? 'never_applied' : edited.length ? 'edited_outside' : pending ? 'pending_changes' : 'in_sync';
  return { status, drift: { edited_outside: edited, pending_changes: pending } };
}
function clientView(id) {
  const cfg = content.clients[id]; const committed = !!base.clients[id];
  return { id, adapter: cfg.adapter, display_name: cfg.display_name, description: cfg.description || '', roots: cfg.roots, params: cfg.params || {}, strict: cfg.strict, icon: cfg.icon, color: cfg.color, ...(cfg.spec ? { spec: cfg.spec } : {}),
    manage: cfg.manage || {}, manage_effective: { ...DEFAULT_MANAGE, ...(cfg.manage || {}) }, adapter_installed: true, caps: capsOf(id), ...(committed ? clientStatus(id) : { status: 'never_applied', drift: { edited_outside: [], pending_changes: 0 } }), adopted_at: adoptedAt[id] ?? null, last_applied_at: appliedAt[id] ?? null, committed };
}
const notCommitted = (r, id) => (!base.clients[id] ? r.problem(409, 'Conflict', 'client is not in committed content yet; commit it first', 'client_not_committed') : null);
function effectiveOf(id, source) {
  const c = source === 'working' ? content : base;
  const res = C.resolveClient(c, id);
  const cfg = c.clients[id]; const caps = capsOf(id, c);
  const via = (kind, n) => { const out = []; if (res[kind].explicit.has(n)) out.push('direct'); for (const k of c.collections) if ((c.profiles[id]?.collections || []).includes(k.id) && k.members.some((m) => m.kind === kind && m.name === n)) out.push(`collection:${k.id}`); return out; };
  const items = Object.fromEntries(KINDS.map((k) => [PLURAL[k], [...res[k].on].sort().map((n) => ({ name: n, via: via(k, n) }))]));
  const F = D.FLOOR;
  const rules = [...F.deny_path_read.map((m) => ({ kind: 'path_read', match: m, decision: 'deny', reason: 'policy floor: protected path' })), ...F.deny_path_write.map((m) => ({ kind: 'path_write', match: m, decision: 'deny', reason: 'policy floor: protected path' })),
    ...Object.entries(F.tools).map(([t, pol]) => ({ kind: 'tool', match: t, decision: pol.max, reason: `policy floor: at most ${pol.max}` })),
    ...Object.values(c.items.rule).filter((r) => !r.applies_to?.length || r.applies_to.includes(id)).filter((r) => !C.floorViolations(c, id).some((v) => v.item === r.name)).map((r) => ({ kind: r.kind, match: r.match, decision: r.decision, reason: r.reason || null }))];
  const mcp = [...res.mcp.on].map((n) => ({ name: n, transport: c.items.mcp[n].transport, egress_hosts: c.items.mcp[n].egress_hosts, scan_status: c.items.mcp[n].scan_status }));
  const { diags } = C.renderClient(c, id);
  const warnings = [];
  if (!caps.permissions) warnings.push(`${cfg.adapter} cannot express permissions: the floor cannot be enforced natively`);
  for (const k of KINDS) for (const n of res[k].explicit) if (!c.items[k][n]) warnings.push(`${k} '${n}' is enabled but does not exist`);
  return { client: id, source, adapter: cfg.adapter, adapter_installed: true, caps, manage: { ...DEFAULT_MANAGE, ...(cfg.manage || {}) }, items, rules, default_tool_decision: 'ask', network_default: 'deny', mcp,
    egress: { default: 'deny', allow_hosts: [...new Set(mcp.flatMap((m) => m.egress_hosts))].sort() }, findings: C.floorViolations(c, id), diagnostics: diags.map((d) => ({ severity: d.severity, code: d.code, message: d.message, item: d.item ?? null })), warnings };
}

// ---------------- generic-spec validation (spec must already be an object, as in the backend) ----------------
const SPEC_KEYS = ['skills', 'agents', 'instructions', 'mcp', 'permissions', 'model_tiers'];
function validateSpec(body) {
  const spec = body.spec;
  if (!spec || typeof spec !== 'object' || Array.isArray(spec)) throw new HttpErr(422, 'Unprocessable', '`spec` must be a mapping', 'validation_error');
  const d = []; const err = (code, item, message) => d.push({ severity: 'error', code, message, item });
  const roots = Object.keys(body.roots || {});
  for (const k of Object.keys(spec)) if (!SPEC_KEYS.includes(k)) err('unknown_key', k, `Unknown key "${k}". Allowed: ${SPEC_KEYS.join(', ')}.`);
  const chkPath = (p, v) => { if (typeof v === 'string' && (/^\//.test(v) || /(^|\/)\.\.(\/|$)/.test(v) || /\\/.test(v))) err('unsafe_path', p, `${p} must be relative and stay inside the root.`); };
  const chkRoot = (p, v) => { if (!v) err('missing_field', p, `${p} is required.`); else if (roots.length && !roots.includes(v)) err('bad_value', p, `Unknown root "${v}". Known roots: ${roots.join(', ')}.`); };
  if (spec.skills) { chkRoot('skills.root', spec.skills.root); if (!spec.skills.dir) err('missing_field', 'skills.dir', 'skills.dir is required.'); chkPath('skills.dir', spec.skills.dir); if (spec.skills.layout && !['dir/SKILL.md', 'flat.md'].includes(spec.skills.layout)) err('bad_value', 'skills.layout', 'skills.layout must be dir/SKILL.md or flat.md.'); }
  if (spec.instructions) { chkRoot('instructions.root', spec.instructions.root); if (!spec.instructions.target) err('missing_field', 'instructions.target', 'instructions.target is required.'); chkPath('instructions.target', spec.instructions.target); if (spec.instructions.merge && !['own', 'block', 'json_keys', 'yaml_keys'].includes(spec.instructions.merge)) err('bad_value', 'instructions.merge', 'merge must be own, block, json_keys or yaml_keys.'); }
  if (spec.agents) { chkRoot('agents.root', spec.agents.root); if (!spec.agents.dir) err('missing_field', 'agents.dir', 'agents.dir is required.'); if (typeof spec.agents.tools === 'string') err('bad_type', 'agents.tools', 'agents.tools must be a mapping of capability to native tool name.'); }
  if (spec.mcp) { chkRoot('mcp.root', spec.mcp.root); if (!spec.mcp.file) err('missing_field', 'mcp.file', 'mcp.file is required.'); chkPath('mcp.file', spec.mcp.file); }
  if (!spec.skills && !spec.instructions && !spec.agents && !spec.mcp) err('missing_field', '', 'Add at least one of skills, agents, instructions, mcp.');
  if (d.length) return { ok: false, artifacts: [], diagnostics: d };
  const arts = [];
  if (spec.skills) arts.push({ root: spec.skills.root, path: spec.skills.layout === 'flat.md' ? `${spec.skills.dir}/sample-skill.md` : `${spec.skills.dir}/sample-skill/SKILL.md`, kind: 'skill', merge: 'own', size: 120, managed_keys: [], preview: '---\nname: sample-skill\ndescription: A sample skill\n---\nDo the sample thing.\n' });
  if (spec.instructions) arts.push({ root: spec.instructions.root, path: spec.instructions.target, kind: 'instruction', merge: spec.instructions.merge || 'own', size: 40, managed_keys: [], preview: 'Be careful.\n' });
  if (spec.agents) arts.push({ root: spec.agents.root, path: `${spec.agents.dir}/sample-agent.md`, kind: 'agent', merge: 'own', size: 90, managed_keys: [], preview: `---\nname: sample-agent\ntools: ${['read', 'shell'].map((c) => spec.agents.tools?.[c]).filter(Boolean).join(', ')}\n---\nYou are a sample agent.\n` });
  if (spec.mcp) arts.push({ root: spec.mcp.root, path: spec.mcp.file, kind: 'mcp', merge: 'json_keys', size: 80, managed_keys: ['mcpServers'], preview: JSON.stringify({ mcpServers: { 'sample-mcp': { command: 'sample-mcp' } } }, null, 2) });
  const diags = [];
  if (spec.agents) for (const c of ['browser', 'web_search']) if (!spec.agents.tools?.[c]) diags.push({ severity: 'warn', code: 'unsupported_capability', message: `Capability "${c}" has no native tool in this spec.`, item: null });
  return { ok: true, artifacts: arts, diagnostics: diags };
}

// ---------------- routes ----------------
const routes = [];
const route = (method, pattern, fn) => routes.push({ method, re: new RegExp(`^${pattern.replace(/:(\w+)/g, '(?<$1>[^/]+)')}$`), fn });
const KIND_OF = { skills: 'skill', agents: 'agent', instructions: 'instruction', mcp: 'mcp', rules: 'rule', memory: 'memory' };

route('GET', '/health', (r) => r.json(200, { status: 'ok', version: '0.1.0-mock', content_initialised: true, adapters: D.ADAPTERS.map((a) => a.id), trust_loopback: TRUST, warnings: TRUST ? ['HUB_TRUST_LOOPBACK is ON: any local process (including agents with a shell) is admin without a key'] : [] }));
route('GET', '/adapters', (r) => r.json(200, { items: D.ADAPTERS.map((a) => ({ id: a.id, display_name: a.display_name, caps: a.caps, default_config: a.default_config, docs: a.docs })), next_cursor: null }));
route('GET', '/adapters/:id', (r) => { const a = D.ADAPTERS.find((x) => x.id === r.p.id); return a ? r.json(200, { id: a.id, display_name: a.display_name, caps: a.caps, default_config: a.default_config, docs: a.docs }) : nf(r, `no such adapter: ${r.p.id}`); });

route('GET', '/clients', (r) => r.json(200, paginate(clientIds(content).map(clientView), r.url)));
route('POST', '/clients/validate-spec', (r) => r.json(200, validateSpec(r.body || {})));
function clientDoc(r, id, b, create) {
  const errs = []; const e = (p, m) => errs.push({ path: p, message: m });
  const allowed = ['adapter', 'display_name', 'description', 'roots', 'params', 'spec', 'strict', 'icon', 'color', 'manage'];
  for (const k of Object.keys(b)) if (!allowed.includes(k)) e(k, 'Extra inputs are not permitted');
  for (const k of ['adapter', 'display_name']) if (!b[k]) e(k, 'Field required');
  if (b.adapter && !D.ADAPTERS.some((a) => a.id === b.adapter)) return r.problem(422, 'Unprocessable', `adapter '${b.adapter}' is not installed`, 'unknown_adapter');
  if (b.adapter === 'generic' && !b.spec) e('spec', "adapter 'generic' requires `spec`");
  for (const k of Object.keys(b.manage || {})) if (!(k in DEFAULT_MANAGE)) e('manage', `unknown concern name(s); allowed: ${Object.keys(DEFAULT_MANAGE).join(', ')}`);
  if (errs.length) return invalid(r.res, errs, 'client failed validation');
  const ad = D.ADAPTERS.find((a) => a.id === b.adapter);
  const caps = b.adapter === 'generic' ? { skills: !!b.spec.skills, agents: !!b.spec.agents, instructions: !!b.spec.instructions, mcp: !!b.spec.mcp, permissions: false, egress: false, memory: false, tool_map: b.spec.agents?.tools || {}, model_tiers: {}, notes: '' } : ad.caps;
  const prev = content.clients[id];
  content.clients[id] = { id, adapter: b.adapter, display_name: b.display_name, description: b.description || '', icon: b.icon || prev?.icon || 'terminal', color: b.color || prev?.color || '#a6a6a6', strict: b.strict ?? true, roots: b.roots && Object.keys(b.roots).length ? b.roots : ad.default_config.roots, params: b.params || {}, caps, manage: b.manage || {}, ...(b.spec ? { spec: b.spec } : {}) };
  if (create) { content.profiles[id] = { collections: [], enable: {}, disable: {} }; applied[id] = {}; live[id] = {}; }
  return r.json(create ? 201 : 200, clientView(id));
}
route('POST', '/clients', (r) => { const { id, ...b } = r.body || {}; if (typeof id !== 'string') return bad(r.res, '`id` is required', 'invalid_id'); if (!ID.test(id)) return bad(r.res, 'invalid name', 'invalid_id'); if (content.clients[id]) return r.problem(409, 'Conflict', 'client already exists', 'already_exists'); return clientDoc(r, id, b, true); });
route('GET', '/clients/:id', (r) => (content.clients[r.p.id] ? r.json(200, clientView(r.p.id)) : nf(r, `no such client: ${r.p.id}`)));
route('PUT', '/clients/:id', (r) => { if (!content.clients[r.p.id]) return nf(r, `no such client: ${r.p.id}`); const { id, ...b } = r.body || {}; return clientDoc(r, r.p.id, b, false); });
route('DELETE', '/clients/:id', (r) => { if (!content.clients[r.p.id]) return nf(r, `no such client: ${r.p.id}`); delete content.clients[r.p.id]; delete content.profiles[r.p.id]; noContent(r.res); });

const DISCOVER = { 'claude-code': { skill: [['code-review', 'identical'], ['container-basics', 'differs'], ['kubectl-helper', 'none'], ['terraform-plan-review', 'none'], ['docs-verify', 'identical']], agent: [['planner', 'none'], ['code-reviewer', 'differs']], instruction: [['claude-md-local', 'none']], mcp: [['filesystem', 'identical'], ['sentry', 'none']], rule: [['allow-all-bash', 'none']] } };
route('POST', '/clients/:id/discover', (r) => {
  const id = r.p.id; if (!content.clients[id]) return nf(r, `no such client: ${id}`);
  const caps = capsOf(id); const src = DISCOVER[id] || DISCOVER['claude-code'];
  const items = {};
  for (const [kind, rows] of Object.entries(src)) { if (!caps[kind === 'mcp' ? 'mcp' : PLURAL[kind]] && kind !== 'rule') continue; items[kind] = rows.map(([name, conflict]) => ({ kind, name, description: content.items[kind][name]?.description || `Found ${name.replace(/-/g, ' ')} in the client's files.`, conflict: conflict === 'none' ? null : conflict === 'identical' ? { existing_source: content.items[kind][name]?.source || 'local', equal: true, differences: [] } : { existing_source: content.items[kind][name]?.source || 'local', equal: false, differences: ['description differs', 'body differs (7 lines)'] }, ...(kind === 'skill' ? { files: ['SKILL.md'] } : {}), ...(name === 'allow-all-bash' ? { floor_violation: 'tool shell: allow exceeds the floor maximum (ask)' } : {}) })); }
  r.json(200, { client: id, items, notes: [`Read the native content under ${Object.values(content.clients[id].roots)[0]} (read-only).`], suggested_params: {} });
});
route('POST', '/import', (r) => {
  const b = r.body || {}; const allowed = ['client', 'items', 'on_conflict', 'enable'];
  const extra = Object.keys(b).filter((k) => !allowed.includes(k)); if (extra.length || !b.client) return invalid(r.res, [{ path: extra[0] || 'client', message: extra.length ? 'Extra inputs are not permitted' : 'Field required' }]);
  if (!content.clients[b.client]) return nf(r, `no such client: ${b.client}`);
  const oc = b.on_conflict || 'skip'; if (!['skip', 'rename', 'replace'].includes(oc)) return bad(r.res, 'on_conflict must be skip, rename or replace');
  const src = DISCOVER[b.client] || DISCOVER['claude-code'];
  const wanted = b.items ? new Set(b.items.map((i) => `${i.kind}/${i.name}`)) : null;
  const results = [];
  for (const [kind, rows] of Object.entries(src)) for (const [name, conflict] of rows) {
    if (wanted && !wanted.has(`${kind}/${name}`)) continue;
    const res = { kind, name, final_name: name, status: 'created', detail: '' };
    const per = (b.items || []).find((i) => i.kind === kind && i.name === name)?.on_conflict;
    if (per && !['skip', 'rename', 'replace', 'link'].includes(per)) return bad(r.res, 'on_conflict must be skip, rename, replace or link');
    const oc2 = per || oc;
    if (conflict !== 'none' && oc2 === 'link') { res.status = 'linked'; if (b.enable !== false && content.items[kind][name]) { const p = (content.profiles[b.client] ||= { collections: [], enable: {}, disable: {} }); p.enable[PLURAL[kind]] = [...new Set([...arr(p.enable[PLURAL[kind]]), name])]; } }
    else if (conflict === 'identical') res.status = 'identical';
    else if (conflict === 'differs' && oc2 === 'skip') { res.status = 'skipped'; res.detail = 'exists with different content'; }
    else {
      let final = name; if (conflict === 'differs' && oc2 === 'rename') { let n = 2; while (content.items[kind][`${name}-${n}`]) n++; final = `${name}-${n}`; res.status = 'renamed'; } else if (conflict === 'differs') res.status = 'replaced';
      res.final_name = final;
      const base0 = { name: final, description: `Imported ${final.replace(/-/g, ' ')} from ${b.client}.`, body: `# ${final}\n\nImported from ${b.client}.\n`, tags: [], source: `imported:${b.client}`, provenance: { origin: b.client }, updated_at: now(), files: [], notes: '' };
      const x = kind === 'agent' ? { capabilities: ['read'], model_tier: 'standard', mode: 'subagent', read_only: false } : kind === 'instruction' ? { title: final, order: 100, applies_to: [b.client] } : kind === 'mcp' ? { transport: 'stdio', command: 'npx', args: [], env_names: [], egress_hosts: [], scan_status: 'unscanned', sandbox_profile: 'srt' } : kind === 'rule' ? { title: final, kind: 'tool', match: 'shell', decision: 'ask', reason: '', applies_to: [b.client] } : {};
      if (kind === 'rule' && name === 'allow-all-bash') { res.status = 'skipped'; res.detail = 'violates the floor: tool shell: allow exceeds the floor maximum (ask)'; results.push(res); continue; }
      content.items[kind][final] = { ...base0, ...x };
      if (b.enable !== false && kind !== 'rule') { const p = (content.profiles[b.client] ||= { collections: [], enable: {}, disable: {} }); p.enable[PLURAL[kind]] = [...new Set([...arr(p.enable[PLURAL[kind]]), final])]; }
    }
    results.push(res);
  }
  r.json(200, { client: b.client, results });
});
route('GET', '/clients/:id/effective', (r) => {
  const id = r.p.id; const source = r.url.searchParams.get('source') || 'committed';
  if (!['committed', 'working'].includes(source)) return bad(r.res, 'source must be committed or working');
  if (source === 'working' ? !content.clients[id] : false) return nf(r, `no such client: ${id}`);
  if (source === 'committed' && !content.clients[id] && !base.clients[id]) return nf(r, `no such client: ${id}`);
  if (source === 'committed') { const nc = notCommitted(r, id); if (nc) return nc; }
  r.json(200, effectiveOf(id, source));
});
route('POST', '/clients/:id/verify', (r) => {
  const id = r.p.id; if (!content.clients[id]) return nf(r, `no such client: ${id}`); const nc = notCommitted(r, id); if (nc) return nc;
  if (!locks.has(id)) return r.problem(409, 'Conflict', 'nothing has been applied to this client yet', 'never_applied');
  const ap = applied[id] || {}; const ks = Object.keys(ap);
  const checks = [{ name: 'files present', ok: ks.every((k) => live[id][k] != null), detail: `${ks.length} delivered file(s) checked` }, { name: 'hashes match lock', ok: ks.every((k) => live[id][k] === ap[k]), detail: ks.filter((k) => live[id][k] !== ap[k]).slice(0, 3).join(', ') || 'all match' }, { name: 'client lists the skills', ok: true, detail: `${id} skills list reports ${ks.filter((k) => k.includes('SKILL.md')).length} skill(s)` }];
  r.json(200, { client: id, ok: checks.every((c) => c.ok), checks });
});
route('GET', '/clients/:id/drift', (r) => { const id = r.p.id; if (!content.clients[id]) return nf(r, `no such client: ${id}`); const nc = notCommitted(r, id); if (nc) return nc; r.json(200, driftOf(id)); });
route('GET', '/clients/:id/audit', (r) => {
  const id = r.p.id; if (!content.clients[id]) return nf(r, `no such client: ${id}`); const nc = notCommitted(r, id); if (nc) return nc;
  const findings = [{ code: 'permissions_readable', severity: 'info', message: 'The live permissions were read and match the floor for protected paths.', item: null, concern: 'permissions', rule: null }];
  if (id === 'claude-code') findings.push({ code: 'tool_too_permissive', severity: 'error', message: 'The live settings allow every shell command; the floor allows at most "ask" (permissions are advisory for this client, so the hub did not write them).', item: null, concern: 'permissions', rule: 'tools.shell' });
  if (id === 'hermes') findings.push({ code: 'mcp_not_in_hub', severity: 'error', message: "live MCP server 'sentry' is not a scanned item in the hub catalog", item: 'sentry', concern: 'mcp', rule: null });
  r.json(200, { client: id, ok: !findings.some((f) => f.severity === 'error'), findings, manage: { ...DEFAULT_MANAGE, ...(content.clients[id].manage || {}) } });
});

for (const [seg, kind] of Object.entries(KIND_OF)) {
  route('GET', `/${seg}`, (r) => r.json(200, paginate(listItems(kind, r.url), r.url)));
  route('GET', `/${seg}/:name`, (r) => { const it = content.items[kind][r.p.name]; return it ? r.json(200, detail(kind, it)) : nf(r, `no such ${kind}: ${r.p.name}`); });
  route('PUT', `/${seg}/:name`, (r) => {
    const b = r.body; if (!b || typeof b !== 'object') return invalid(r.res, [{ path: '', message: 'JSON object required' }]);
    const errs = validateItem(kind, r.p.name, b); if (errs.length) return invalid(r.res, errs, 'content failed validation');
    const existing = content.items[kind][r.p.name];
    if (!existing && kind === 'skill' && !b.description) return invalid(r.res, [{ path: 'description', message: 'Field required' }], 'content failed validation');
    r.json(200, detail(kind, saveItem(kind, r.p.name, b, existing)));
  });
  route('DELETE', `/${seg}/:name`, (r) => { if (!content.items[kind][r.p.name]) return nf(r, `no such ${kind}: ${r.p.name}`); removeItem(kind, r.p.name); noContent(r.res); });
}
route('POST', '/skills/:name/duplicate', (r) => {
  const b = r.body || {}; if (Object.keys(b).some((k) => k !== 'new_name') || !b.new_name) return invalid(r.res, [{ path: 'new_name', message: 'Field required' }]);
  const s = content.items.skill[r.p.name]; if (!s) return nf(r, `no such skill: ${r.p.name}`);
  if (!ID.test(b.new_name)) return bad(r.res, 'invalid name', 'invalid_id');
  if (content.items.skill[b.new_name]) return r.problem(409, 'Conflict', `skill already exists: ${b.new_name}`, 'already_exists');
  content.items.skill[b.new_name] = { ...structuredClone(s), name: b.new_name, updated_at: now(), source: 'local', provenance: {} };
  r.json(201, detail('skill', content.items.skill[b.new_name]));
});

// collections (CollectionDoc: full documents, unknown keys forbidden)
const collDoc = (k) => ({ id: k.id, title: k.title, description: k.description, icon: k.icon, color: k.color, order: k.order, members: k.members });
const requireItem = (kind, name) => { if (!KINDS.includes(kind)) throw new HttpErr(400, 'Bad request', `unknown kind: ${kind}`, 'bad_request'); if (!content.items[kind]?.[name]) throw new HttpErr(404, 'Not found', `no such ${kind}: ${name}`, 'not_found'); };
function checkColl(b) {
  const errs = []; const allowed = ['id', 'title', 'description', 'icon', 'color', 'order', 'members'];
  for (const k of Object.keys(b)) if (!allowed.includes(k)) errs.push({ path: k, message: 'Extra inputs are not permitted' });
  if (!b.title) errs.push({ path: 'title', message: 'Field required' }); if ((b.icon || '').length > 16) errs.push({ path: 'icon', message: 'String should have at most 16 characters' });
  if (b.color && !/^[#A-Za-z0-9-]*$/.test(b.color)) errs.push({ path: 'color', message: 'String should match pattern' });
  for (const m of arr(b.members)) requireItem(m.kind, m.name);
  return errs;
}
function suggestionList() {
  const inAny = new Set(content.collections.flatMap((k) => k.members.map((m) => `${m.kind}/${m.name}`)));
  const loose = Object.values(content.items.skill).filter((it) => !inAny.has(`skill/${it.name}`));
  const groups = new Map();
  for (const it of loose) { const tag = (it.tags || [])[0]; const key = tag ? `tag:${tag}` : `prefix:${it.name.split('-')[0]}`; (groups.get(key) || groups.set(key, []).get(key)).push(it); }
  const COLORS = ['#0072b2', '#009e73', '#e69f00', '#cc79a7', '#56b4e9', '#d55e00'];
  return [...groups.entries()].filter(([, l]) => l.length >= 3).map(([key, l], i) => { const [kind, v] = key.split(':'); return { id: `sugg-${v}`, title: `${v[0].toUpperCase()}${v.slice(1)} skills`, description: kind === 'tag' ? `Skills tagged "${v}".` : `Skills whose names start with "${v}-".`, icon: 'folder', color: COLORS[i % COLORS.length], reason: kind === 'tag' ? 'tag' : 'common_prefix', members: l.map((x) => ({ kind: 'skill', name: x.name })) }; });
}
route('GET', '/collections/suggestions', (r) => r.json(200, { items: suggestionList() }));
route('POST', '/collections/suggestions/accept', (r) => {
  const b = r.body || {}; if (!Array.isArray(b.ids) || !b.ids.length) return invalid(r.res, [{ path: 'ids', message: 'Field required' }]);
  const all = suggestionList(); const made = [];
  for (const id of b.ids) {
    const s = all.find((x) => x.id === id); if (!s) return nf(r, `no such suggestion: ${id}`);
    const o = b.overrides?.[id] || {}; const cid = id.replace(/^sugg-/, '');
    if (content.collections.some((k) => k.id === cid)) return r.problem(409, 'Conflict', 'collection already exists', 'already_exists');
    const k = { id: cid, title: o.title || s.title, description: s.description, icon: s.icon, color: s.color, order: Math.max(0, ...content.collections.map((x) => x.order)) + 10, members: (o.members || s.members).map((m) => ({ kind: m.kind, name: m.name })) };
    content.collections.push(k); made.push(collDoc(k));
  }
  r.json(200, { items: made, next_cursor: null });
});
route('GET', '/collections', (r) => r.json(200, paginate([...content.collections].sort((a, b) => a.order - b.order || a.id.localeCompare(b.id)).map(collDoc), r.url, 1e9)));
route('POST', '/collections/reorder', (r) => {
  const ids = r.body?.ids; if (!Array.isArray(ids)) return invalid(r.res, [{ path: 'ids', message: 'Field required' }]);
  for (const i of ids) if (!content.collections.some((k) => k.id === i)) return nf(r, `no such collection: ${i}`);
  if (new Set(ids).size !== ids.length) return bad(r.res, 'duplicate ids');
  ids.forEach((id, i) => { content.collections.find((k) => k.id === id).order = (i + 1) * 10; });
  r.json(200, { items: ids.map((id) => collDoc(content.collections.find((k) => k.id === id))), next_cursor: null });
});
route('POST', '/collections', (r) => {
  const b = r.body || {}; if (typeof b.id !== 'string' || !ID.test(b.id)) return bad(r.res, '`id` must match ^[a-z0-9][a-z0-9._-]{0,63}$', 'invalid_id');
  if (content.collections.some((k) => k.id === b.id)) return r.problem(409, 'Conflict', 'collection already exists', 'already_exists');
  const errs = checkColl(b); if (errs.length) return invalid(r.res, errs);
  const k = { id: b.id, title: b.title, description: b.description || '', icon: b.icon || '', color: b.color || '', order: b.order ?? 100, members: arr(b.members).map((m) => ({ kind: m.kind, name: m.name })) };
  content.collections.push(k); r.json(201, collDoc(k));
});
route('PUT', '/collections/:id', (r) => {
  const k = content.collections.find((x) => x.id === r.p.id); if (!k) return nf(r, `no such collection: ${r.p.id}`);
  const b = { ...(r.body || {}), id: r.p.id }; const errs = checkColl(b); if (errs.length) return invalid(r.res, errs);
  Object.assign(k, { title: b.title, description: b.description ?? '', icon: b.icon ?? '', color: b.color ?? '', order: b.order ?? 100, members: arr(b.members).map((m) => ({ kind: m.kind, name: m.name })) });
  r.json(200, collDoc(k));
});
route('DELETE', '/collections/:id', (r) => { const i = content.collections.findIndex((x) => x.id === r.p.id); if (i < 0) return nf(r, `no such collection: ${r.p.id}`); content.collections.splice(i, 1); for (const p of Object.values(content.profiles)) p.collections = arr(p.collections).filter((c) => c !== r.p.id); noContent(r.res); });
route('POST', '/collections/:id/members', (r) => {
  const k = content.collections.find((x) => x.id === r.p.id); if (!k) return nf(r, `no such collection: ${r.p.id}`);
  const ms = r.body?.members; if (!Array.isArray(ms)) return invalid(r.res, [{ path: 'members', message: 'Field required' }]);
  for (const m of ms) requireItem(m.kind, m.name);
  for (const m of ms) if (!k.members.some((x) => x.kind === m.kind && x.name === m.name)) k.members.push({ kind: m.kind, name: m.name });
  r.json(200, collDoc(k));
});
route('DELETE', '/collections/:id/members/:kind/:name', (r) => {
  const k = content.collections.find((x) => x.id === r.p.id); if (!k) return nf(r, `no such collection: ${r.p.id}`);
  const before = k.members.length; k.members = k.members.filter((m) => !(m.kind === r.p.kind && m.name === r.p.name));
  if (k.members.length === before) return nf(r, 'not a member of this collection');
  r.json(200, collDoc(k));
});
route('POST', '/items/bulk', (r) => {
  const b = r.body || {}; const allowed = ['items', 'add_to_collection', 'remove_from_collection', 'add_tags', 'remove_tags'];
  if (!Array.isArray(b.items) || Object.keys(b).some((k) => !allowed.includes(k))) return invalid(r.res, [{ path: 'items', message: 'Field required' }]);
  const coll = (id) => { const k = content.collections.find((x) => x.id === id); if (!k) throw new HttpErr(404, 'Not found', `no such collection: ${id}`, 'not_found'); return k; };
  if (b.add_to_collection) coll(b.add_to_collection); if (b.remove_from_collection) coll(b.remove_from_collection);
  const results = b.items.map((it) => {
    const out = { kind: it.kind, name: it.name, ok: true, detail: [] };
    try {
      requireItem(it.kind, it.name);
      if (b.add_to_collection) { const k = coll(b.add_to_collection); if (!k.members.some((m) => m.kind === it.kind && m.name === it.name)) k.members.push({ kind: it.kind, name: it.name }); out.detail.push(`added to ${b.add_to_collection}`); }
      if (b.remove_from_collection) { const k = coll(b.remove_from_collection); k.members = k.members.filter((m) => !(m.kind === it.kind && m.name === it.name)); out.detail.push(`removed from ${b.remove_from_collection}`); }
      if (arr(b.add_tags).length || arr(b.remove_tags).length) { for (const t of arr(b.add_tags)) if (!ID.test(t)) throw new HttpErr(400, 'Bad request', `invalid tag ${JSON.stringify(t)}`, 'invalid_id'); const x = content.items[it.kind][it.name]; x.tags = [...new Set([...(x.tags || []).filter((t) => !arr(b.remove_tags).includes(t)), ...arr(b.add_tags)])]; out.detail.push('tags updated'); }
    } catch (e) { if (!(e instanceof HttpErr)) throw e; out.ok = false; out.error = e.detail; }
    return out;
  });
  r.json(200, { results, ok: results.every((x) => x.ok) });
});

// profiles + matrix (plural selection keys, as ProfileDoc)
route('GET', '/profiles/:client', (r) => { if (!content.clients[r.p.client]) return nf(r, `no such client: ${r.p.client}`); const p = content.profiles[r.p.client] || { collections: [], enable: {}, disable: {} }; const full = (o) => Object.fromEntries(Object.values(PLURAL).map((k) => [k, arr(o?.[k])])); r.json(200, { collections: arr(p.collections), enable: full(p.enable), disable: full(p.disable) }); });
route('PUT', '/profiles/:client', (r) => {
  if (!content.clients[r.p.client]) return nf(r, `no such client: ${r.p.client}`);
  const b = r.body || {}; const errs = [];
  for (const k of Object.keys(b)) if (!['collections', 'enable', 'disable'].includes(k)) errs.push({ path: k, message: 'Extra inputs are not permitted' });
  for (const sel of ['enable', 'disable']) for (const k of Object.keys(b[sel] || {})) if (!Object.values(PLURAL).includes(k)) errs.push({ path: `${sel}.${k}`, message: 'Extra inputs are not permitted' });
  if (errs.length) return invalid(r.res, errs);
  content.profiles[r.p.client] = { collections: arr(b.collections), enable: b.enable || {}, disable: b.disable || {} };
  r.json(200, content.profiles[r.p.client]);
});
function cellsFor(id, kind, name, resBy) {
  const c = C.cellFor(content, capsOf(id), id, kind, name, resBy[id]);
  const via = []; if (resBy[id][kind].explicit.has(name)) via.push('direct'); for (const k of content.collections) if (arr(content.profiles[id]?.collections).includes(k.id) && k.members.some((m) => m.kind === kind && m.name === name)) via.push(`collection:${k.id}`);
  return { state: c.state, ...(c.reason ? { reason: c.reason } : {}), via };
}
route('GET', '/matrix', (r) => {
  const cols = clientIds(); const resBy = Object.fromEntries(cols.map((c) => [c, C.resolveClient(content, c)]));
  const row = (kind, name) => { const it = content.items[kind][name]; return { kind, name, title: it.title || it.name, valid: true, cells: Object.fromEntries(cols.map((c) => [c, cellsFor(c, kind, name, resBy)])) }; };
  const groups = [...content.collections].sort((a, b) => a.order - b.order || a.id.localeCompare(b.id)).map((k) => ({ id: k.id, title: k.title, icon: k.icon, color: k.color, rows: k.members.filter((m) => content.items[m.kind]?.[m.name]).map((m) => row(m.kind, m.name)) }));
  const inAny = new Set(content.collections.flatMap((k) => k.members.map((m) => `${m.kind}/${m.name}`)));
  const loose = KINDS.flatMap((kind) => Object.keys(content.items[kind]).sort().filter((n) => !inAny.has(`${kind}/${n}`)).map((n) => row(kind, n)));
  groups.push({ id: null, title: 'Uncollected', icon: '', color: '', rows: loose });
  r.json(200, { clients: cols.map((c) => ({ id: c, display_name: content.clients[c].display_name, adapter: content.clients[c].adapter, adapter_installed: true })), groups });
});
function toggleOne(client, kind, name, enabled) {
  if (!content.clients[client]) throw new HttpErr(404, 'Not found', `no such client: ${client}`, 'not_found');
  requireItem(kind, name);
  const cell = C.cellFor(content, capsOf(client), client, kind, name, C.resolveClient(content, client));
  if (enabled && (cell.state === 'unsupported' || cell.state === 'blocked')) throw new HttpErr(409, 'Conflict', `cannot enable: ${cell.reason}`, `cell_${cell.state}`, { reason: cell.reason });
  C.setEnabled(content, client, kind, name, enabled);
  return { client, kind, name, ...cellsFor(client, kind, name, { [client]: C.resolveClient(content, client) }) };
}
route('POST', '/matrix/toggle', (r) => { const b = r.body || {}; if (typeof b.enabled !== 'boolean' || !b.client || !b.kind || !b.name) return invalid(r.res, [{ path: 'enabled', message: 'Input should be a valid boolean' }]); r.json(200, toggleOne(b.client, b.kind, b.name, b.enabled)); });
route('POST', '/matrix/bulk', (r) => {
  const b = r.body || {}; if (!Array.isArray(b.clients) || !Array.isArray(b.items) || typeof b.enabled !== 'boolean') return invalid(r.res, [{ path: 'clients', message: 'Field required' }]);
  const results = [];
  for (const c of b.clients) for (const it of b.items) { try { results.push({ ...toggleOne(c, it.kind, it.name, b.enabled), ok: true }); } catch (e) { if (!(e instanceof HttpErr)) throw e; results.push({ client: c, kind: it.kind, name: it.name, ok: false, error: e.detail, code: e.code }); } }
  r.json(200, { results, ok: results.every((x) => x.ok) });
});

// changes (git semantics: pending = working tree vs last commit)
route('GET', '/changes', (r) => { const files = C.changesBetween(base, content); r.json(200, { content_hash: treeHash(base), count: files.length, items: files.map((f) => ({ path: f.path, status: f.status, diff: f.diff })) }); });
route('POST', '/changes/commit', (r) => {
  const msg = r.body?.message; if (typeof msg !== 'string') return invalid(r.res, [{ path: 'message', message: 'Field required' }]);
  const files = C.changesBetween(base, content); if (!files.length) return r.problem(409, 'Conflict', 'nothing to commit', 'nothing_to_commit');
  const commit = sha40(); history.unshift({ commit, message: msg.trim(), author: 'admin', time: Math.floor(now()) }); base = structuredClone(content);
  r.json(200, { commit, content_hash: treeHash(base) });
});
route('POST', '/changes/discard', (r) => {
  const paths = r.body?.paths; const files = C.changesBetween(base, content);
  const touched = paths ? files.filter((f) => paths.includes(f.path)).map((f) => f.path) : files.map((f) => f.path);
  if (!paths || touched.length) content = structuredClone(base);   // approximation: restores the whole tree when any listed path is touched
  r.json(200, { discarded: touched });
});
route('GET', '/changes/history', (r) => r.json(200, { items: history.slice(0, Math.min(500, Number(r.url.searchParams.get('limit') || 50))), next_cursor: null }));
route('POST', '/changes/revert', (r) => {
  if (C.changesBetween(base, content).length) return r.problem(409, 'Conflict', 'commit or discard pending changes before reverting', 'dirty_tree');
  const c = history.find((h) => h.commit === r.body?.commit); if (!c) return bad(r.res, 'unknown commit', 'unknown_commit');
  const rv = sha40(); history.unshift({ commit: rv, message: `Revert "${c.message}"`, author: 'admin', time: Math.floor(now()) });
  r.json(200, { commit: rv, content_hash: treeHash(base) });   // the mock records the commit but does not restore file contents
});

// plan / apply: plans are built from COMMITTED content
route('POST', '/plan', (r) => {
  const want = r.body?.clients; const ids = want?.length ? want : clientIds(base);
  for (const id of ids) if (!base.clients[id]) return nf(r, `client '${id}' is not in committed content`);
  const id = uid('plan_');
  const built = ids.map((c) => C.planClient(base, c, applied, live));
  const plan = { id, content_hash: treeHash(base), created_at: now(), pending_changes: C.changesBetween(base, content).length, clients: built.map(({ _rendered, ...p }) => p) };
  plans.set(id, { plan, rendered: Object.fromEntries(built.map((p) => [p.client, p._rendered])) });
  r.json(200, plan);
});
route('GET', '/plan/:id', (r) => { const p = plans.get(r.p.id); return p ? r.json(200, p.plan) : nf(r, 'no such plan'); });
route('POST', '/apply', (r) => {
  const b = r.body || {}; if (!b.plan_id) return invalid(r.res, [{ path: 'plan_id', message: 'Field required' }]);
  if (b.confirm !== true) return r.problem(422, 'Unprocessable', 'apply needs confirm: true', 'confirm_required');
  const p = plans.get(b.plan_id); if (!p) return nf(r, 'no such plan');
  if (p.plan.content_hash !== treeHash(base)) return r.problem(409, 'Conflict', 'content changed since this plan was made; plan again', 'plan_stale');
  for (const cp of p.plan.clients) if (C.planClient(base, cp.client, applied, live).digest !== cp.digest) return r.problem(409, 'Conflict', 'live files changed since this plan was made; plan again', 'plan_stale');
  const adopt = new Set(arr(b.adopt_paths)); const results = [];
  for (const cp of p.plan.clients) {
    const id = cp.client;
    if (cp.blocked) { results.push({ client: id, status: 'blocked', written: [], removed: [], skipped_conflicts: [], backup_dir: null, checks: [], verify_ok: null, blocked_reasons: cp.blocked_reasons, error: '' }); continue; }
    const stage = { ...applied[id] }, stageLive = { ...live[id] }; const written = [], removed = [], skipped = [];
    for (const f of cp.files) {
      const k = `${f.root}:${f.path}`; const rf = p.rendered[id].find((x) => `${x.root}:${x.path}` === k);
      if (f.action === 'conflict' && !adopt.has(f.path)) { skipped.push({ root: f.root, path: f.path }); continue; }
      if (['add', 'change', 'conflict'].includes(f.action) && rf) { stage[k] = rf.text; stageLive[k] = rf.text; written.push({ root: f.root, path: f.path }); }
      else if (f.action === 'remove') { delete stage[k]; delete stageLive[k]; removed.push({ root: f.root, path: f.path }); }
    }
    applied[id] = stage; live[id] = stageLive; locks.add(id); appliedAt[id] = now();
    const status = written.length + removed.length ? 'applied' : 'nothing';
    const checks = status === 'applied' ? [{ name: 'files present', ok: true, detail: `${written.length + removed.length} change(s) written and read back` }, { name: 'hashes recorded in lock manifest', ok: true, detail: `${Object.keys(stage).length} file(s) tracked` }, { name: `${id} lists the delivered skills`, ok: skipped.length === 0, detail: skipped.length ? `${skipped.length} conflicting file(s) were skipped` : 'client reports all skills' }] : [];
    results.push({ client: id, status, written, removed, skipped_conflicts: skipped, backup_dir: status === 'applied' ? `~/.local/share/agent-hub/backups/${id}/${new Date().toISOString().replace(/[:.]/g, '-')}` : null, checks, verify_ok: checks.length ? checks.every((c) => c.ok) : null, blocked_reasons: [], error: '' });
  }
  plans.delete(b.plan_id);
  r.json(200, { plan_id: b.plan_id, content_hash: treeHash(base), results });
});
route('GET', '/policy/floor', (r) => r.json(200, D.FLOOR));
route('POST', '/policy/check', (r) => {
  const b = r.body || {}; const extra = Object.keys(b).filter((k) => !['rules', 'mcp'].includes(k)); if (extra.length) return r.problem(422, 'Unprocessable', `unknown keys: ${JSON.stringify(extra)}`, 'validation_error');
  const F = D.FLOOR; const violations = [];
  for (const [i, x] of arr(b.rules).entries()) {
    if (!ENUM.kind.includes(x.kind) || !ENUM.decision.includes(x.decision) || !x.match) return invalid(r.res, [{ path: `rules.${i}`, message: 'invalid rule' }], 'bad rule');
    const order = { deny: 3, ask: 2, allow: 1 };
    if (x.kind === 'tool' && F.tools[x.match] && order[x.decision] < order[F.tools[x.match].max]) violations.push({ code: 'tool_too_permissive', severity: 'error', message: `tool ${x.match}: ${x.decision} exceeds the floor maximum (${F.tools[x.match].max})`, item: `check-${i}`, concern: 'permissions', rule: `tools.${x.match}` });
    if (x.kind === 'command' && x.decision === 'allow' && F.command_allow_forbid_patterns.includes(x.match)) violations.push({ code: 'command_catch_all', severity: 'error', message: `command ${x.match}: a catch-all allow is forbidden`, item: `check-${i}`, concern: 'permissions', rule: 'command_allow_forbid_patterns' });
  }
  r.json(200, { ok: !violations.some((v) => v.severity === 'error'), violations });
});

// memory inbox (registered before /memory/:name by the static-first route order)
route('GET', '/memory/inbox', (r) => r.json(200, { items: inbox, next_cursor: null }));
route('POST', '/memory/inbox', (r) => {
  const b = r.body || {}; const errs = ['client', 'title', 'body'].filter((k) => !b[k]).map((k) => ({ path: k, message: 'Field required' })); if (errs.length) return invalid(r.res, errs);
  const it = { id: uid('inb-'), client: b.client, type: b.type || 'reference', title: b.title, body: b.body, created: now(), valid: true }; inbox.push(it); r.json(201, it);
});
route('POST', '/memory/inbox/:id/promote', (r) => {
  const i = inbox.findIndex((x) => x.id === r.p.id); if (i < 0) return nf(r, 'no such inbox entry'); const it = inbox[i]; const e = r.body || {};
  const memId = e.id || it.id; if (!ID.test(memId)) return bad(r.res, 'invalid memory id', 'invalid_id');
  if (content.items.memory[memId]) return r.problem(409, 'Conflict', `memory ${memId} already exists`, 'already_exists');
  content.items.memory[memId] = { name: memId, type: e.type || it.type, title: e.title || it.title, body: e.body ?? it.body, description: e.title || it.title, tags: [], notes: '', source: `inbox:${it.client}`, provenance: {}, updated_at: now() };
  inbox.splice(i, 1); r.json(200, { id: memId, promoted_from: `${it.client}/${it.id}` });
});
route('POST', '/memory/inbox/:id/reject', (r) => { const i = inbox.findIndex((x) => x.id === r.p.id); if (i < 0) return nf(r, 'no such inbox entry'); const [it] = inbox.splice(i, 1); r.json(200, { id: it.id, client: it.client, rejected: true }); });

// audit / settings / keys
route('GET', '/audit', (r) => r.json(200, paginate(auditRows, r.url)));
route('GET', '/settings', (r) => r.json(200, { editable: limits, info: { content_dir: '~/work/hub/content', data_dir: '~/.local/share/agent-hub', policy_file: 'hub/policy/floor.yaml', host: '127.0.0.1', port: 8792, knowledge: { ok: true, url: 'http://127.0.0.1:8795' }, adapters: D.ADAPTERS.map((a) => a.id), adapter_load_errors: {}, trust_loopback: TRUST } }));
route('PUT', '/settings', (r) => {
  const b = r.body || {}; const unknown = Object.keys(b).filter((k) => !(k in limits)); if (unknown.length) return r.problem(422, 'Unprocessable', `unknown or read-only settings: ${JSON.stringify(unknown)}`, 'validation_error');
  const RANGE = { plan_retention: [1, 200], inbox_max_per_client: [1, 5000], inbox_rate_per_min: [1, 600] };
  for (const [k, v] of Object.entries(b)) { const [lo, hi] = RANGE[k]; if (!Number.isInteger(v) || v < lo || v > hi) return r.problem(422, 'Unprocessable', `${k} must be an integer in ${lo}..${hi}`, 'validation_error'); }
  Object.assign(limits, b); r.json(200, { editable: limits, info: {} });
});
const DOCTOR = [
  { id: 'loopback_trust', severity: 'crit', title: 'Loopback trust is ON', detail: 'Any local process, including an agent with a shell, is treated as admin without a key.', fix: '# ~/.config/agent-hub/env\nHUB_TRUST_LOOPBACK=0\nsystemctl --user restart agent-hub' },
  { id: 'env_perms', severity: 'warn', title: 'Env file is readable by other users', detail: '~/.config/agent-hub/env has mode 0644; it holds the hub keys.', fix: 'chmod 600 ~/.config/agent-hub/env' },
  { id: 'data_dir_mode', severity: 'warn', title: 'Data directory is group-writable', detail: '~/.local/share/agent-hub should be 0700.', fix: 'chmod 700 ~/.local/share/agent-hub\nchmod -R go-rwx ~/.local/share/agent-hub' },
  { id: 'apparmor', severity: 'info', title: 'No AppArmor profile is applied', detail: 'Optional: confine the hub process. Templates are opt-in and never auto-applied.', fix: '' },
  { id: 'listen_loopback', severity: 'ok', title: 'Listens on 127.0.0.1 only', detail: '', fix: '' },
  { id: 'floor_present', severity: 'ok', title: 'Policy floor loaded', detail: '', fix: '' },
  { id: 'git_present', severity: 'ok', title: 'git is available', detail: '', fix: '' },
];
const doctorStatus = () => (DOCTOR.some((f) => f.severity === 'crit') ? 'action_needed' : DOCTOR.some((f) => f.severity === 'warn') ? 'attention' : 'ok');
route('GET', '/doctor', (r) => r.json(200, { status: doctorStatus(), findings: DOCTOR }));
route('GET', '/doctor/summary', (r) => r.json(200, { crit: DOCTOR.filter((f) => f.severity === 'crit').length, warn: DOCTOR.filter((f) => f.severity === 'warn').length, info: DOCTOR.filter((f) => f.severity === 'info').length }));
route('GET', '/keys', (r) => r.json(200, { items: keys, next_cursor: null }));
route('POST', '/keys', (r) => {
  const b = r.body || {}; if (!b.name || !String(b.name).trim()) return bad(r.res, 'name must be 1..100 characters');
  if (!['admin', 'viewer', 'client'].includes(b.role || 'viewer')) return bad(r.res, 'role must be admin, viewer or client');
  const role = b.role || 'viewer'; const scopes = arr(b.scopes);
  if (scopes.some((s) => !/^(inbox:write|knowledge:read:[A-Za-z0-9._-]+)$/.test(s))) return bad(r.res, 'scopes must be inbox:write or knowledge:read:<ns>');
  if (role === 'client' && !ID.test(b.client || '')) return bad(r.res, 'a client token needs a valid `client` id'); if (role !== 'client' && (b.client || scopes.length)) return bad(r.res, 'client/scopes only apply to role=client');
  const secret = `hc_${crypto.randomBytes(24).toString('base64url')}`;
  const k = { id: uid('key_'), name: b.name.trim(), role, prefix: secret.slice(0, 8), client: b.client || null, scopes, created_at: now(), last_used_at: null }; keys.push(k);
  r.json(201, { ...k, secret });   // the only time the secret is ever returned
});
route('DELETE', '/keys/:id', (r) => { const i = keys.findIndex((k) => k.id === r.p.id); if (i < 0) return nf(r, `no such key: ${r.p.id}`); keys.splice(i, 1); noContent(r.res); });

// ---- knowledge proxy (/api/v1/knowledge/*): the hub proxies GET and POST only ----
const kv = '/knowledge';
const nsOf = (n) => K.namespaces.find((x) => x.name === n);
route('GET', `${kv}/health`, (r) => r.json(200, { reachable: true, authenticated: true, backends: Object.fromEntries(K.backends.map((b) => [b.name, b.ok])) }));
route('GET', `${kv}/backends`, (r) => r.json(200, { items: K.backends, next_cursor: null }));
route('GET', `${kv}/namespaces`, (r) => r.json(200, paginate(K.namespaces, r.url)));
route('POST', `${kv}/namespaces`, (r) => {
  const b = r.body || {}; const errs = [];
  if (!ID.test(b.name || '')) errs.push({ path: 'name', message: 'must match ^[a-z0-9][a-z0-9._-]{0,63}$' }); if (!['qdrant', 'lancedb'].includes(b.backend)) errs.push({ path: 'backend', message: 'must be qdrant or lancedb' }); if (!(Number.isInteger(b.dim) && b.dim > 0 && b.dim <= 8192)) errs.push({ path: 'dim', message: 'must be an integer from 1 to 8192' });
  if (errs.length) return invalid(r.res, errs); if (nsOf(b.name)) return r.problem(409, 'Conflict', `namespace ${b.name} exists`, 'already_exists');
  const n = { name: b.name, backend: b.backend, embedding_model: b.embedding_model || 'bge-m3', dim: b.dim, description: b.description || '', disk_bytes: 0, count: 0 }; K.namespaces.push(n); r.json(201, n);
});
route('GET', `${kv}/namespaces/:ns`, (r) => (nsOf(r.p.ns) ? r.json(200, nsOf(r.p.ns)) : nf(r, 'unknown namespace')));
const ALLOWED_ROOTS = ['~/work/', '~/notes/'];
route('POST', `${kv}/namespaces/:ns/ingest`, (r) => {
  const n = nsOf(r.p.ns); if (!n) return nf(r, 'unknown namespace'); const b = r.body || {}; let total;
  if (b.path) { if (!(ALLOWED_ROOTS.some((x) => String(b.path).startsWith(x)) && !String(b.path).includes('..'))) return invalid(r.res, [{ path: 'path', message: `path must be under an allowlisted root (${ALLOWED_ROOTS.join(', ')})` }]); total = 40 + (String(b.glob || '').length * 7 % 60); }
  else { if (!Array.isArray(b.documents) || !b.documents.length) return invalid(r.res, [{ path: 'documents', message: 'provide documents[] or a path' }]); if (!b.documents.every((d) => typeof d.text === 'string' && d.text.length <= 20000)) return invalid(r.res, [{ path: 'documents', message: 'each document needs text of at most 20000 characters' }]); total = b.documents.length; for (const d of b.documents) K.docs.push({ id: d.id || uid('d'), ns: n.name, text: d.text, metadata: d.metadata || {} }); }
  const job = { id: uid('job_'), namespace: n.name, state: 'running', total, done: 0, created_at: now(), error: null }; jobs.unshift(job);
  const tick = setInterval(() => { job.done = Math.min(job.total, job.done + Math.max(1, Math.ceil(job.total / 6))); if (job.done >= job.total) { job.state = 'done'; clearInterval(tick); n.count += job.total; n.disk_bytes += job.total * 4096; } for (const s of jobStreams.get(job.id) || []) { s.write(`data: ${JSON.stringify(job)}\n\n`); if (job.state === 'done') s.end(); } }, 450);
  tick.unref?.(); r.json(202, job);
});
route('GET', `${kv}/jobs`, (r) => r.json(200, paginate(jobs, r.url)));
route('GET', `${kv}/jobs/:id`, (r) => { const j = jobs.find((x) => x.id === r.p.id); return j ? r.json(200, j) : nf(r, 'unknown job'); });
route('GET', `${kv}/jobs/:id/stream`, (r) => {
  const j = jobs.find((x) => x.id === r.p.id); if (!j) return nf(r, 'unknown job');
  r.res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' }); r.res.write(`data: ${JSON.stringify(j)}\n\n`);
  if (j.state === 'done') return r.res.end();
  const set = jobStreams.get(j.id) || new Set(); set.add(r.res); jobStreams.set(j.id, set); r.req.on('close', () => set.delete(r.res));
});
let queries = 0;
route('POST', `${kv}/namespaces/:ns/query`, (r) => {
  const n = nsOf(r.p.ns); if (!n) return nf(r, 'unknown namespace'); const b = r.body || {};
  if (typeof b.text !== 'string' || !b.text.trim()) return invalid(r.res, [{ path: 'text', message: 'a query text is required' }]); if (!Number.isInteger(b.k ?? 5) || (b.k ?? 5) < 1 || (b.k ?? 5) > 50) return invalid(r.res, [{ path: 'k', message: 'must be an integer from 1 to 50' }]);
  if (++queries % 25 === 0) return r.problem(429, 'Too many requests', 'Query rate limit reached', 'rate_limited');
  const words = b.text.toLowerCase().split(/\W+/).filter((w) => w.length > 2);
  const hits = K.docs.filter((d) => d.ns === n.name).map((d) => { const t = d.text.toLowerCase(); const s = words.length ? words.filter((w) => t.includes(w)).length / words.length : 0; return { id: d.id, score: Math.round((0.35 + s * 0.6) * 1000) / 1000, text: d.text, metadata: d.metadata }; }).filter((h) => h.score >= (b.min_score ?? 0)).sort((a, b2) => b2.score - a.score).slice(0, b.k ?? 5);
  r.json(200, { namespace: n.name, hits, took_ms: 12 });
});
route('GET', `${kv}/tokens`, (r) => r.json(200, { items: K.tokens, next_cursor: null }));
route('POST', `${kv}/tokens`, (r) => {
  const b = r.body || {}; if (!b.client || !Array.isArray(b.scopes) || !b.scopes.length || !b.scopes.every((s) => nsOf(s.ns) && ['read', 'write'].includes(s.mode))) return invalid(r.res, [{ path: 'scopes', message: 'each scope needs an existing namespace and a mode of read or write' }]);
  const secret = `ak_${crypto.randomBytes(20).toString('hex')}`; const t = { id: uid('kt_'), client: b.client, prefix: secret.slice(0, 7), scopes: b.scopes, created_at: now(), last_used_at: null }; K.tokens.push(t); r.json(201, { ...t, secret });
});
route('GET', `${kv}/indexes`, (r) => r.json(200, { items: K.indexes, next_cursor: null }));
route('POST', `${kv}/indexes`, (r) => {
  const b = r.body || {}; const errs = []; if (!b.name) errs.push({ path: 'name', message: 'Field required' }); if (!['codegraph', 'symbols', 'other'].includes(b.kind)) errs.push({ path: 'kind', message: 'must be codegraph, serena or other' }); if (!(typeof b.path === 'string' && !b.path.includes('..') && (b.path.startsWith('~/') || b.path.startsWith('/')))) errs.push({ path: 'path', message: 'must be an absolute path without ..' });
  if (errs.length) return invalid(r.res, errs); const ix = { id: uid('ix_'), name: b.name, kind: b.kind, path: b.path, project: b.project || '' }; K.indexes.push(ix); r.json(201, ix);
});

// ---------------- HTTP plumbing ----------------
// Static paths win over parameterised ones (/memory/inbox before /memory/:name).
let _sorted = null;
const sortedRoutes = () => (_sorted ||= [...routes].sort((a, b) => (a.re.source.includes('(?<') ? 1 : 0) - (b.re.source.includes('(?<') ? 1 : 0)));
const HOSTS = new Set(['localhost', '127.0.0.1', '[::1]']);
async function api(req, res, url) {
  const p = url.pathname.replace(/^\/api\/v1/, '') || '/';
  const hostname = (req.headers.host || '').replace(/:\d+$/, '');
  if (!HOSTS.has(hostname)) return problem(res, 421, 'Misdirected request', `Host "${hostname}" is not allowed`, 'bad_host');
  const method = req.method.toUpperCase();
  const mutating = !['GET', 'HEAD', 'OPTIONS'].includes(method);
  if (req.headers.origin && mutating) { try { if (new URL(req.headers.origin).host !== req.headers.host) return problem(res, 403, 'Forbidden', 'Cross-origin request refused', 'cross_origin'); } catch { return problem(res, 403, 'Forbidden', 'Bad Origin', 'cross_origin'); } }
  if (mutating && req.headers['x-agent-hub'] !== '1') return problem(res, 403, 'Forbidden', 'The X-Agent-Hub: 1 header is required on non-GET requests', 'csrf_header_required');
  if (KEY && p !== '/health' && req.headers.authorization !== `Bearer ${KEY}`) return problem(res, 401, 'Unauthorized', 'A valid API key is required', 'unauthorized');
  const fault = Number(req.headers['x-mock-status'] || 0); // dev/test hook: force an error status
  if (fault) return problem(res, fault, 'Injected fault', `mock fault ${fault}`, fault === 409 ? 'conflict' : fault === 503 ? 'unavailable' : undefined);
  if (p.startsWith('/knowledge/') && process.env.MOCK_KNOWLEDGE_DOWN === '1') return problem(res, 503, 'Service unavailable', 'the knowledge service is not reachable on 127.0.0.1:8795', 'knowledge_unavailable');
  if (p.startsWith('/knowledge/') && process.env.MOCK_KNOWLEDGE_UNAUTH === '1') return problem(res, 502, 'Bad gateway', 'the knowledge service rejected the hub key: set HUB_KNOWLEDGE_TOKEN in ~/.config/agent-hub/env', 'knowledge_upstream_unauthorized');
  if (p.startsWith('/knowledge/') && !['GET', 'POST'].includes(method)) return problem(res, 405, 'Method not allowed', 'the knowledge proxy only forwards GET and POST', 'method_not_allowed');
  let body = null;
  if (mutating && (req.headers['content-length'] > 0 || req.headers['transfer-encoding'])) {
    if (!/^application\/json/.test(req.headers['content-type'] || '')) return problem(res, 415, 'Unsupported media type', 'Content-Type must be application/json', 'unsupported_media_type');
    body = await readBody(req);
  }
  for (const rt of sortedRoutes()) {
    if (rt.method !== method) continue;
    const m = rt.re.exec(p); if (!m) continue;
    const params = Object.fromEntries(Object.entries(m.groups || {}).map(([k, v]) => [k, decodeURIComponent(v)]));
    const ctx = { req, res, url, p: params, body, json: (c, o) => json(res, c, o), problem: (...a) => problem(res, ...a) };
    if (mutating) res.on('finish', () => audit(method, url.pathname, res.statusCode, { keys: Object.keys(body || {}) }));   // metadata only, never values
    try { return rt.fn(ctx); } catch (e) { if (e instanceof HttpErr) return problem(res, e.status, e.title, e.detail, e.code, e.extra); throw e; }
  }
  return problem(res, 404, 'Not found', `No route for ${method} ${p}`, 'not_found');
}

function serveShot(res, url) {
  const ms = Math.min(15000, Number(url.searchParams.get('ms') || 4000));
  if (url.pathname === '/__demo.js') {
    const name = url.searchParams.get('name');
    const W = 'const until=async(f)=>{for(let i=0;i<80;i++){const v=f();if(v)return v;await new Promise(r=>setTimeout(r,100));}};const wait=(ms)=>new Promise(r=>setTimeout(r,ms));';
    const demos = {
      palette: `${W} await until(()=>document.querySelector('ah-palette')); await wait(900); window.dispatchEvent(new Event('ah:palette')); await wait(500); const i=document.querySelector('ah-palette input'); i.value='matrix'; i.dispatchEvent(new Event('input'));`,
      matrixstage: `${W} await until(()=>document.querySelector('.mx-cell.on')); await wait(400); const cells=[...document.querySelectorAll('.mx-cell.on, .mx-cell.off')].slice(0,6); for(const c of cells) c.click();`,
      libselect: `${W} await until(()=>document.querySelector('.lib-card')); await wait(400); const cs=[...document.querySelectorAll('.lib-card .sel-box')].slice(0,3); for(const c of cs) c.click();`,
      plandiff: `${W} const b=await until(()=>[...document.querySelectorAll('button')].find(x=>x.dataset.action==='make-plan')); b.click(); await until(()=>document.querySelector('.plan-file')); await wait(300); const t=document.querySelector('.plan-file summary'); t && t.click();`,
      importnext: `${W} await until(()=>document.querySelector('.wizard .btn.primary')); await wait(300); [...document.querySelectorAll('.wizard .btn.primary')].pop().click(); await until(()=>document.querySelector('.imp-row')); await wait(400);`,
      wizard: `${W} const b=await until(()=>[...document.querySelectorAll('button')].find(x=>/Add a client/.test(x.textContent))); b.click(); await wait(500); const g=await until(()=>[...document.querySelectorAll('.pick-card input')].find(x=>x.value==='generic')); g.click(); await wait(300); const n=()=>[...document.querySelectorAll('dialog .btn.primary')].pop(); n().click(); await wait(500); const nm=document.querySelector('#cw-name'); nm.value='Cursor 2'; nm.dispatchEvent(new Event('input')); n().click(); await wait(1200);`,
    };
    res.writeHead(200, { 'Content-Type': 'text/javascript' }); return res.end(demos[name] || '');
  }
  if (url.pathname === '/__slow.gif') return setTimeout(() => { res.writeHead(200, { 'Content-Type': 'image/gif' }); res.end(Buffer.from('R0lGODlhAQABAAAAACw=', 'base64')); }, ms);
  const demo = /^\w+$/.test(url.searchParams.get('demo') || '') ? `<script type="module" src="/__demo.js?name=${url.searchParams.get('demo')}"></script>` : '';
  const html = fs.readFileSync(path.join(ROOT, 'index.html'), 'utf8').replace('</body>', `<img src="/__slow.gif?ms=${ms}" width="1" height="1" alt="">${demo}</body>`);
  res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8', 'Content-Security-Policy': CSP }); res.end(html);
}
function serveStatic(req, res, url) {
  let rel; try { rel = decodeURIComponent(url.pathname); } catch { res.writeHead(400); return res.end('bad path'); }
  if (rel === '/') rel = '/index.html';
  const abs = path.resolve(ROOT, '.' + rel);
  if (!abs.startsWith(ROOT + path.sep) || /^\/(dev|tests|node_modules)\//.test(rel) || /package\.json$/.test(rel)) { res.writeHead(404); return res.end('not found'); }
  fs.readFile(abs, (err, buf) => {
    if (err) { res.writeHead(404); return res.end('not found'); }
    res.writeHead(200, { 'Content-Type': MIME[path.extname(abs)] || 'application/octet-stream', 'Content-Security-Policy': CSP, 'X-Content-Type-Options': 'nosniff', 'Cache-Control': 'no-store' });
    res.end(buf);
  });
}
function proxy(req, res) {
  const t = new URL(PROXY);
  const up = http.request({ host: t.hostname, port: t.port, path: req.url, method: req.method, headers: { ...req.headers, host: t.host, ...(req.headers.origin ? { origin: `http://${t.host}` } : {}), ...(req.headers.referer ? { referer: `http://${t.host}/` } : {}) } }, (r) => { res.writeHead(r.statusCode, r.headers); r.pipe(res); });
  up.on('error', (e) => problem(res, 502, 'Bad gateway', String(e.message), 'proxy_error'));
  res.on('close', () => up.destroy()); req.pipe(up);
}

export const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://x');
  try {
    if (PROXY && (url.pathname.startsWith('/api/') || url.pathname === '/metrics')) return proxy(req, res);
    if (url.pathname.startsWith('/api/v1')) return await api(req, res, url);
    if (url.pathname === '/metrics') { res.writeHead(200, { 'Content-Type': 'text/plain' }); return res.end('# mock\nagent_hub_up 1\n'); }
    if (['/__shot', '/__slow.gif', '/__demo.js'].includes(url.pathname)) return serveShot(res, url);
    return serveStatic(req, res, url);
  } catch (e) {
    if (res.headersSent) return res.end();
    if (e instanceof HttpErr) return problem(res, e.status, e.title, e.detail, e.code, e.extra);
    problem(res, 500, 'Internal error', String(e.message));
  }
});
if (process.argv[1] === fileURLToPath(import.meta.url)) {
  server.listen(PORT, HOST, () => console.log(`mock agent-hub: http://${HOST}:${server.address().port}/  (${KEY ? 'bearer key required' : 'no auth'}${EMPTY ? ', empty' : ''}${TRUST ? ', trust-loopback warning' : ''})`));
}
