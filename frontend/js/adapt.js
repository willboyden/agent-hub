// Normalisers from the backend's response shapes (services/hub.py, organize.py, importer.py ... as read from the backend source) to the
// flat shapes the views use. Each accepts BOTH the real backend field names and the older/simpler ones from the architecture sketch, so a
// backend tweak does not blank a screen. Pure functions: unit-tested, no DOM.
import { NAME_RE } from './util.js';

const arr = (x) => (Array.isArray(x) ? x : []);
const PALETTE = ['#0072b2', '#009e73', '#e69f00', '#cc79a7', '#56b4e9', '#d55e00', '#f0e442', '#a6a6a6'];

// ---- items ----
// List rows: real = {kind,name,title,description(<=300),tags,groups(sidecar),collections,source,valid,issues:[{path,message,fix_hint}],updated,enabled_for}
export function normalizeItem(it, kind) {
  const bad = it.valid === false;
  return { ...it, kind: it.kind || kind, title: it.title || it.name, description: it.description || '', tags: arr(it.tags), source: it.source || 'local',
    groups: arr(it.collections ?? it.groups), enabled_for: arr(it.enabled_for), updated_at: it.updated_at ?? it.updated ?? 0, valid: it.valid !== false,
    issues: arr(it.issues).map((i) => ({ severity: i.severity || (bad ? 'error' : 'warn'), code: i.code, message: i.path && !String(i.message).startsWith(i.path) ? `${i.path}: ${i.message}` : i.message, fix_hint: i.fix_hint })) };
}

const SKILL_HIDDEN = new Set(['SKILL.md', 'hub.yaml']);
// Detail -> editor draft. Real = summary + {meta, body, file_list, files:{rel: text|null}, sidecar}; the sketch had flat fields and files:[{path,content}].
export function detailDraft(kind, it) {
  const m = { ...it, ...(it.meta || {}) };
  const d = { name: it.name, groups: arr(it.collections ?? it.groups), tags: arr(m.tags ?? it.tags), notes: it.sidecar?.notes ?? it.notes ?? '', description: m.description ?? it.description ?? '', source: it.source || 'local', updated_at: it.updated ?? it.updated_at ?? 0, issues: arr(it.issues) };
  if (kind === 'skill') {
    const files = Array.isArray(it.files) ? it.files.map((f) => [f.path, f.content ?? '']) : Object.entries(it.files || {});
    Object.assign(d, { body: it.body ?? '', files: files.filter(([p]) => !SKILL_HIDDEN.has(p)).map(([path, content]) => ({ path, content: content ?? '', binary: content == null })), tags: arr(it.sidecar?.tags ?? m.tags ?? it.tags) });
  } else if (kind === 'agent') Object.assign(d, { body: it.body ?? '', capabilities: arr(m.capabilities), model_tier: m.model_tier || '', mode: m.mode || 'subagent', read_only: !!m.read_only });
  else if (kind === 'instruction') Object.assign(d, { title: m.title ?? it.title ?? it.name, body: it.body ?? '', order: m.order ?? 100, applies_to: arr(m.applies_to) });
  else if (kind === 'mcp') Object.assign(d, { transport: m.transport || 'stdio', command: m.command || '', args: arr(m.args), url: m.url || null, env_names: arr(m.env_names), sandbox_profile: m.sandbox_profile || '', egress_hosts: arr(m.egress_hosts), pinned_ref: m.pinned_ref || null, notes: m.notes ?? '',
    scan: { status: m.scan_status || it.scan_status || 'unscanned', findings: arr(it.scan?.findings ?? it.findings) } });
  else if (kind === 'rule') Object.assign(d, { title: m.title ?? it.title ?? it.name, rule_kind: it.rule_kind || (m.kind && m.kind !== 'rule' ? m.kind : 'tool'), match: m.match || '', decision: m.decision || 'ask', reason: m.reason || '', applies_to: arr(m.applies_to) });
  else if (kind === 'memory') Object.assign(d, { title: m.title ?? it.title ?? it.name, type: m.type || 'project', body: it.body ?? '' });
  return d;
}
// Editor draft -> PUT body. The backend models FORBID unknown keys, so each kind sends exactly its own fields (no `groups`, no `notes` for agents ...).
export function toPutBody(kind, d) {
  const files = () => Object.fromEntries((d.files || []).filter((f) => !f.binary).map((f) => [f.path, f.content]));
  switch (kind) {
    case 'skill': {
      const body = { description: d.description, body: d.body, tags: d.tags, notes: d.notes || '' };
      // The backend deletes extra files that are missing from `files`. Binary files cannot round-trip through JSON text, so when a skill has any,
      // leave `files` out entirely: the extra files stay untouched on disk instead of being wiped.
      if (!(d.files || []).some((f) => f.binary)) body.files = files();
      return body;
    }
    case 'agent': return { description: d.description, body: d.body, capabilities: d.capabilities, model_tier: d.model_tier || null, mode: d.mode, read_only: d.read_only, tags: d.tags };
    case 'instruction': return { title: d.title, body: d.body, order: d.order, applies_to: d.applies_to, tags: d.tags };
    case 'mcp': return { transport: d.transport, command: d.command || null, args: d.args, url: d.url || null, env_names: d.env_names, sandbox_profile: d.sandbox_profile || null, egress_hosts: d.egress_hosts, pinned_ref: d.pinned_ref || null, tags: d.tags, notes: d.notes || '' };
    case 'rule': return { title: d.title, kind: d.rule_kind, match: d.match, decision: d.decision, reason: d.reason || null, applies_to: d.applies_to, tags: d.tags };
    default: return { title: d.title, type: d.type, body: d.body, tags: d.tags };
  }
}
export const validTag = (s) => NAME_RE.test(String(s ?? ''));

// ---- collections ----
// PUT /collections/{id} validates a whole CollectionDoc (extra keys forbidden), so always send the full doc, never a fragment.
export const collectionDoc = (c, patch = {}) => { const m = { ...c, ...patch }; return { id: m.id, title: m.title, description: m.description ?? '', icon: m.icon ?? '', color: m.color ?? '', order: m.order ?? 100, members: arr(m.members).map((x) => ({ kind: x.kind, name: x.name })) }; };

// ---- matrix ----
export function normalizeMatrix(m) {
  const cols = arr(m.columns ?? m.clients).map((c, i) => ({ ...c, color: c.color || PALETTE[i % PALETTE.length], caps: c.caps || null, unusable: c.adapter_installed === false }));
  return { columns: cols, groups: arr(m.groups).map((g) => ({ ...g, id: g.id ?? '_ungrouped', color: g.color || null, icon: g.icon || 'folder', rows: arr(g.rows).map((r) => ({ ...r, description: r.description || '', cells: r.cells || {} })) })) };
}
// bulk endpoints: real = {results:[{client,kind,name,ok,error?,code?,state?}], ok}; sketch = {changed, skipped:[...]}
export function bulkOutcome(resp) {
  if (Array.isArray(resp?.results)) {
    const bad = resp.results.filter((r) => r.ok === false);
    return { changed: resp.results.length - bad.length, skipped: bad.map((r) => ({ client: r.client, kind: r.kind, name: r.name, reason: r.error || r.code || '' })) };
  }
  return { changed: resp?.changed ?? 0, skipped: arr(resp?.skipped) };
}

// ---- changes / history ----
const STATUS = { A: 'added', '??': 'added', added: 'added', untracked: 'added', M: 'modified', modified: 'modified', D: 'deleted', deleted: 'deleted', R: 'modified', renamed: 'modified' };
export function normalizeChanges(c) {
  const files = arr(c.items ?? c.files).map((f) => ({ path: f.path, status: STATUS[f.status] || 'modified', diff: f.diff || '' }));
  return { files, count: c.count ?? files.length, content_hash: c.content_hash || '' };
}
export const normalizeHistory = (list) => arr(list).map((h) => ({ commit: h.commit, message: h.message || '', author: h.author || '', ts: h.ts ?? h.time ?? 0, files: h.files ?? null, reverts: h.reverts }));

// ---- clients ----
export function normalizeClient(c) {
  const status = { drift: 'edited_outside' }[c.status] ?? c.status;
  const drift = { edited_outside: arr(c.drift?.edited_outside), pending_changes: c.drift?.pending_changes ?? 0 };
  return { ...c, status, drift, adopted_at: c.adopted_at ?? null, last_applied_at: c.last_applied_at ?? null, manage_explicit: c.manage ?? {}, manage: c.manage_effective ?? c.manage ?? {}, caps: c.caps || null, roots: c.roots || {}, strict: c.strict !== false, counts: c.counts || null, unusable: c.adapter_installed === false };
}
// effective: real = {items:{plural:[{name,via}]}, rules:[Rule], egress:{default,allow_hosts}, findings, diagnostics, warnings:[str], caps}
export function normalizeEffective(e, client) {
  const names = (k) => arr(e.items?.[k]).map((x) => (typeof x === 'string' ? x : x.name)).sort();
  const via = (k, n) => { const x = arr(e.items?.[k]).find((y) => y && y.name === n); return x?.via || []; };
  const caps = e.caps || client?.caps || {};
  const warnings = [...arr(e.warnings).map((w) => (typeof w === 'string' ? { severity: 'warn', message: w } : w)),
    ...arr(e.findings).map((f) => ({ severity: f.severity || 'warn', message: f.message })), ...arr(e.diagnostics).map((d) => ({ severity: d.severity || 'warn', message: d.message }))];
  const seen = new Set(); const uniq = warnings.filter((w) => { const k = `${w.severity}|${w.message}`; if (seen.has(k)) return false; seen.add(k); return true; });
  return { client: e.client, items: { skills: names('skills'), agents: names('agents'), instructions: names('instructions'), mcp: names('mcp'), memory: names('memory'), rules: names('rules') }, via,
    tools: Object.entries(caps.tool_map || {}).map(([capability, native]) => ({ capability, native })),
    rules: arr(e.rules).map((r) => ({ ...r, title: r.title || r.match, source: r.source || (r.locked || /\bfloor\b/i.test(r.reason || '') ? 'floor' : 'content') })),
    mcp: arr(e.mcp), egress: { default: e.egress?.default ?? e.network_default ?? 'deny', allowed: arr(e.egress?.allow_hosts ?? e.egress?.allowed) }, warnings: uniq, source: e.source || 'committed' };
}
// drift: real states = in_sync | advisory | never_applied | missing | pending | orphaned | changed_live
export const DRIFT_OK = new Set(['in_sync', 'advisory', 'match']);
export const normalizeDrift = (d) => ({ client: d.client, status: d.status, files: arr(d.files).map((f) => ({ ...f, state: f.state === 'match' ? 'in_sync' : f.state })) });
export const driftProblems = (d) => d.files.filter((f) => !DRIFT_OK.has(f.state));

// ---- apply ----
export function normalizeApply(r) {
  const results = arr(r.results).map((x) => ({ client: x.client, status: x.status, written: Array.isArray(x.written) ? x.written.length : (x.written ?? 0), removed: Array.isArray(x.removed) ? x.removed.length : (x.removed ?? 0),
    skipped: arr(x.skipped_conflicts ?? x.skipped).map((s) => (typeof s === 'string' ? s : s.path)), backup: x.backup_dir ?? x.backup ?? '', verify: arr(x.checks ?? x.verify), verify_ok: x.verify_ok ?? null,
    reason: arr(x.blocked_reasons).join('; ') || x.reason || '', error: x.error || '' }));
  return { plan_id: r.plan_id, content_hash: r.content_hash, results, ok: results.every((x) => x.status === 'applied' || x.status === 'nothing') };
}

// ---- import ----
// conflict: null | {existing_source, equal, differences:[str]}. Older string forms are mapped onto the same object.
const OLD = { identical: true, exists_identical: true, differs: false, exists_different: false };
const conflictOf = (c) => (c == null || c === 'none' ? null : typeof c === 'object' ? { existing_source: c.existing_source || '', equal: !!c.equal, differences: arr(c.differences) } : { existing_source: '', equal: !!OLD[c], differences: [] });
export function normalizeDiscover(d) {
  const flat = Array.isArray(d.items) ? d.items : Object.entries(d.items || {}).flatMap(([kind, rows]) => arr(rows).map((r) => ({ ...r, kind: r.kind || kind })));
  return { client: d.client, notes: arr(d.notes), items: flat.map((r) => ({ kind: r.kind, name: r.name, description: r.description || '', conflict: conflictOf(r.conflict), files: Array.isArray(r.files) ? r.files.length : (r.files ?? null), floor_violation: r.floor_violation || '' })) };
}
export function normalizeImportResult(r) {
  const out = { imported: [], renamed: [], replaced: [], linked: [], skipped: [], identical: [], errors: [] };
  if (!Array.isArray(r.results)) return { ...out, imported: arr(r.imported), renamed: arr(r.renamed), replaced: arr(r.replaced), skipped: arr(r.skipped) };
  for (const x of r.results) {
    const ref = { kind: x.kind, name: x.name, final_name: x.final_name || x.name, detail: x.detail || '', warnings: arr(x.warnings) };
    if (x.status === 'created') out.imported.push(ref); else if (x.status === 'renamed') out.renamed.push({ from: x.name, to: ref.final_name, kind: x.kind, ...ref }); else if (x.status === 'replaced') out.replaced.push(ref); else if (x.status === 'linked') out.linked.push(ref);
    else if (x.status === 'identical') out.identical.push(ref); else if (x.status === 'error') out.errors.push(ref); else out.skipped.push(ref);
  }
  return out;
}

// ---- settings / audit / keys / floor ----
export function normalizeSettings(s) {
  if (s.editable || s.info) return { editable: s.editable || {}, info: s.info || {} };
  const { plan_retention, inbox_max_per_client, inbox_rate_per_min, ...rest } = s;
  return { editable: Object.fromEntries(Object.entries({ plan_retention, inbox_max_per_client, inbox_rate_per_min }).filter(([, v]) => v != null)), info: rest };
}
export function normalizeAudit(e) {
  const action = e.action || (e.method ? `${e.method} ${e.path}` : '');
  const params = e.params && Object.keys(e.params).length ? JSON.stringify(e.params) : '';
  return { id: e.id, ts: e.ts, actor: e.actor || '', role: e.role || '', action, target: e.target || (e.method ? e.path : ''), detail: e.detail || params, ok: e.ok ?? (e.status == null || e.status < 400), status: e.status ?? null };
}
// The floor is a policy document (deny_path_read, tools:{}, network_default ...). Turn it into readable rows: what is locked and why.
export function floorRows(p) {
  if (Array.isArray(p?.rules)) return p.rules.map((r) => ({ title: r.title || r.match, kind: r.kind, match: r.match, decision: r.decision, reason: r.reason || '' }));
  const rows = [];
  if (p?.network_default) rows.push({ title: 'floor.network', kind: 'egress_host', match: '*', decision: p.network_default, reason: 'floor.why_network' });
  for (const m of arr(p?.deny_path_read)) rows.push({ title: 'floor.path_read', kind: 'path_read', match: m, decision: 'deny', reason: 'floor.why_paths' });
  for (const m of arr(p?.deny_path_write)) rows.push({ title: 'floor.path_write', kind: 'path_write', match: m, decision: 'deny', reason: 'floor.why_paths' });
  for (const [tool, pol] of Object.entries(p?.tools || {})) rows.push({ title: 'floor.tool', kind: 'tool', match: tool, decision: pol.max, reason: 'floor.why_tool', extra: pol.default });
  for (const m of arr(p?.command_allow_forbid_patterns)) rows.push({ title: 'floor.command', kind: 'command', match: m, decision: 'ask', reason: 'floor.why_command' });
  if (p?.mcp?.require_scan_clean) rows.push({ title: 'floor.mcp_scan', kind: 'mcp_server', match: '*', decision: 'ask', reason: 'floor.why_mcp' });
  if (p?.mcp?.require_explicit_egress) rows.push({ title: 'floor.mcp_egress', kind: 'egress_host', match: '*', decision: 'ask', reason: 'floor.why_egress' });
  return rows;
}
// i18n-key rows (title/reason start with "floor.") are translated by the caller; plain strings pass through.
export const isFloorKey = (s) => typeof s === 'string' && /^floor\.[a-z_]+$/.test(s);

// ---- knowledge / inbox ----
export const normalizeInbox = (i) => ({ ...i, created_at: i.created_at ?? i.created ?? 0, title: i.title || i.id, body: i.body || '', type: i.type || 'reference' });

// PUT /clients/{id} validates a whole ClientDoc (adapter and display_name are required, unknown keys forbidden): send the full document with only the
// intended change applied. `manage` is the EXPLICIT flags stored in the doc, not the effective defaults.
export const clientDoc = (c, patch = {}) => {
  const m = { ...c, ...patch };
  const doc = { adapter: m.adapter, display_name: m.display_name, description: m.description || '', roots: m.roots || {}, params: m.params || {}, strict: m.strict !== false, manage: { ...(c.manage_explicit ?? {}), ...(patch.manage ?? {}) } };
  if (m.spec) doc.spec = m.spec; if (m.icon) doc.icon = m.icon; if (m.color) doc.color = m.color;
  return doc;
};

// ---- suggestions ----
export const normalizeSuggestion = (x) => ({ id: x.id, title: x.title || x.id, description: x.description || '', icon: x.icon || 'folder', color: x.color || '#0072b2', reason: x.reason || 'keyword', members: arr(x.members).map((m) => ({ kind: m.kind, name: m.name })) });

// knowledge /backends: real = {backends:{name:{ok,path|url,tables,...}}, namespaces, data_dir_free_bytes, embedder}; sketch = {items:[{name,...}]}.
export function normalizeBackends(b) {
  if (Array.isArray(b?.items)) return b.items;
  return Object.entries(b?.backends || {}).map(([name, v]) => ({ name, ok: !!v.ok, kind: v.url ? 'service' : 'embedded', url: v.url, path: v.path, tables: v.tables, version: v.version, dims: v.dims, disk_bytes: v.disk_bytes, detail: v.detail }));
}

// ---- doctor (host hardening) ----
const SEV_ORDER = { crit: 0, warn: 1, info: 2, ok: 3 };
export function groupFindings(list) {
  const g = { crit: [], warn: [], info: [], ok: [] };
  for (const f of Array.isArray(list) ? list : []) (g[f.severity] || g.info).push({ id: f.id, severity: f.severity in g ? f.severity : 'info', title: f.title || f.id, detail: f.detail || '', fix: f.fix || '' });
  return g;
}
export const doctorCounts = (s) => ({ crit: s?.crit ?? 0, warn: s?.warn ?? 0, info: s?.info ?? 0 });
export const severityRank = (s) => SEV_ORDER[s] ?? 2;
