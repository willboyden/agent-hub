// Mock content model for the Agent Hub UI: content tree, effective-config resolution, per-client rendering and planning.
// This imitates ARCHITECTURE §3/§4 closely enough to exercise the UI. It is NOT the compiler: real rendering lives in the
// backend adapters. Everything here is invented.
import crypto from 'node:crypto';
import * as D from './mock-data.mjs';

export const NAME_RE = /^[a-z0-9][a-z0-9._-]{0,63}$/;
export const KINDS = ['skill', 'agent', 'instruction', 'mcp', 'rule', 'memory'];
export const PK = { skill: 'skills', agent: 'agents', instruction: 'instructions', mcp: 'mcp', rule: 'rules', memory: 'memory' };
const CONCERN = { skill: 'skills', agent: 'agents', instruction: 'instructions', mcp: 'mcp', memory: 'memory', rule: 'permissions' };
export const now = () => Date.now() / 1000;
export const sha = (s) => crypto.createHash('sha256').update(s).digest('hex');

// ---------- diff (LCS) ----------
export function unifiedDiff(a, b, pathA, pathB, ctx = 3) {
  const A = a == null ? [] : a.split('\n'), B = b == null ? [] : b.split('\n');
  if (a != null && a.endsWith('\n')) A.pop();
  if (b != null && b.endsWith('\n')) B.pop();
  const n = A.length, m = B.length;
  const L = Array.from({ length: n + 1 }, () => new Uint16Array(m + 1));
  for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) L[i][j] = A[i] === B[j] ? L[i + 1][j + 1] + 1 : Math.max(L[i + 1][j], L[i][j + 1]);
  const ops = []; let i = 0, j = 0;
  while (i < n || j < m) {
    if (i < n && j < m && A[i] === B[j]) { ops.push([' ', A[i]]); i++; j++; }
    else if (i < n && (j >= m || L[i + 1][j] >= L[i][j + 1])) { ops.push(['-', A[i]]); i++; }   // deletions before additions, like git
    else { ops.push(['+', B[j]]); j++; }
  }
  const changed = ops.map((o, k) => (o[0] !== ' ' ? k : -1)).filter((k) => k >= 0);
  if (!changed.length) return '';
  const hunks = []; let cur = null;
  for (const k of changed) {
    const lo = Math.max(0, k - ctx), hi = Math.min(ops.length - 1, k + ctx);
    if (cur && lo <= cur.hi + 1) cur.hi = hi; else { cur = { lo, hi }; hunks.push(cur); }
  }
  const out = [`--- ${a == null ? '/dev/null' : 'a/' + pathA}`, `+++ ${b == null ? '/dev/null' : 'b/' + pathB}`];
  for (const h of hunks) {
    let oa = 1, ob = 1;
    for (let k = 0; k < h.lo; k++) { if (ops[k][0] !== '+') oa++; if (ops[k][0] !== '-') ob++; }
    const seg = ops.slice(h.lo, h.hi + 1);
    const ca = seg.filter((o) => o[0] !== '+').length, cb = seg.filter((o) => o[0] !== '-').length;
    out.push(`@@ -${ca ? oa : oa - 1},${ca} +${cb ? ob : ob - 1},${cb} @@`);
    for (const [t, line] of seg) out.push(t + line);
  }
  return redact(out.join('\n') + '\n');
}
// Plan/diff output is redacted (§4): anything that looks like `secret = value` never leaves the server.
export const redact = (s) => s.replace(/((?:token|secret|password|api[_-]?key)["']?\s*[:=]\s*["']?)([^\s"',]{6,})/gi, (m, p, v) => (/^\$\{|^\[/.test(v) ? m : `${p}[redacted]`));

import { parseYamlLite as miniYaml } from '../js/yaml-lite.js';
export { miniYaml };

export function toYaml(v, ind = 0) {
  const pad = ' '.repeat(ind);
  const sc = (x) => (typeof x === 'string' ? (/^[A-Za-z0-9_./@~*$-][^:#\n]*$/.test(x) && !/^(true|false|null|\d+)$/.test(x) ? x : JSON.stringify(x)) : String(x));
  if (Array.isArray(v)) return v.length ? v.map((x) => `${pad}- ${typeof x === 'object' && x ? JSON.stringify(x) : sc(x)}`).join('\n') + '\n' : `${pad}[]\n`;
  return Object.entries(v).map(([k, x]) => {
    if (x == null) return `${pad}${k}: null\n`;
    if (Array.isArray(x)) return x.length ? `${pad}${k}:\n${toYaml(x, ind + 2)}` : `${pad}${k}: []\n`;
    if (typeof x === 'object') return Object.keys(x).length ? `${pad}${k}:\n${toYaml(x, ind + 2)}` : `${pad}${k}: {}\n`;
    return `${pad}${k}: ${sc(x)}\n`;
  }).join('');
}

// ---------- content ----------
const skillBody = (s) => `# ${s.name.replace(/-/g, ' ')}\n\n${s.desc}\n\n## When to use\n\n- The task matches: ${s.desc.charAt(0).toLowerCase()}${s.desc.slice(1)}\n- You have read the relevant files first.\n\n## Steps\n\n1. State the goal in one sentence.\n2. Gather the facts you need before changing anything.\n3. Make the smallest change that works.\n4. Run it and report exactly what you ran.\n`;
const ISSUES = {
  'csv-wrangler': [{ severity: 'warn', code: 'no_examples', message: 'The skill body has no example section.' }],
  'sql-helper': [{ severity: 'error', code: 'frontmatter_invalid', message: 'description: is too short to tell an agent when to use this skill (min 20 characters).' }],
  'docs-alt-text': [{ severity: 'warn', code: 'large_file', message: 'references/labels.json is 480 KiB; large files slow every sync.' }],
};

export function seedContent() {
  const c = { items: { skill: {}, agent: {}, instruction: {}, mcp: {}, rule: {}, memory: {} }, collections: [], profiles: {}, clients: {} };
  D.SKILLS.forEach((s, i) => {
    c.items.skill[s.name] = { name: s.name, description: ISSUES[s.name]?.[0]?.severity === 'error' ? 'Explain plans' : s.desc, body: skillBody(s), tags: s.tags, notes: '', source: s.source, provenance: s.source.includes(':') ? { origin: s.source.split(':')[1], imported_at: '2026-09-18' } : {}, updated_at: now() - 3600 * (3 + ((i * 37) % 400)),
      files: s.name === 'code-review' ? [{ path: 'references/checklist.md', content: '# Review checklist\n\n- Does it do what the description says?\n- Are errors handled at the boundary?\n- Is there a test that would fail without the change?\n' }, { path: 'scripts/diffstat.sh', content: '#!/usr/bin/env bash\ngit diff --stat "${1:-HEAD~1}"\n' }] : [] };
  });
  for (const a of D.AGENTS) c.items.agent[a.name] = { ...a, tags: [], notes: '', source: 'local', provenance: {}, body: `You are the ${a.name.replace(/-/g, ' ')}. ${a.description}\n\nWork step by step. Report only what you actually ran.\n`, updated_at: now() - 3600 * 40 };
  for (const x of D.INSTRUCTIONS) c.items.instruction[x.name] = { ...x, description: x.title, tags: [], notes: '', source: 'local', provenance: {}, updated_at: now() - 3600 * 90 };
  for (const x of D.MCP) c.items.mcp[x.name] = { ...x, title: x.name, tags: [], notes: '', source: 'local', provenance: {}, updated_at: now() - 3600 * 60 };
  for (const x of D.RULES) c.items.rule[x.name] = { ...x, description: x.title, tags: [], notes: '', source: 'local', provenance: {}, updated_at: now() - 3600 * 120 };
  for (const x of D.MEMORY) c.items.memory[x.name] = { ...x, description: x.title, tags: [], notes: '', source: 'local', provenance: {}, updated_at: now() - 3600 * 200 };
  c.collections = structuredClone(D.COLLECTIONS).map((x) => ({ ...x, members: x.members.map((m) => { const [kind, ...r] = m.split('/'); return { kind, name: r.join('/') }; }) }));
  c.profiles = Object.fromEntries(Object.entries(structuredClone(D.PROFILES)).map(([id, p]) => [id, { collections: p.collections, enable: Object.fromEntries(Object.entries(p.enable).map(([k, v]) => [PK[k], v])), disable: Object.fromEntries(Object.entries(p.disable || {}).map(([k, v]) => [PK[k], v])) }]));
  for (const cl of D.CLIENTS) { const { status, last_applied_at, ...rest } = cl; c.clients[cl.id] = structuredClone(rest); }
  return c;
}

// ---------- serialisation (working tree as files) ----------
export function serialize(c) {
  const f = {};
  for (const s of Object.values(c.items.skill)) {
    f[`skills/${s.name}/SKILL.md`] = `---\nname: ${s.name}\ndescription: ${s.description}\n---\n\n${s.body}`;
    f[`skills/${s.name}/hub.yaml`] = toYaml({ tags: s.tags, notes: s.notes || '', source: s.source });
    for (const x of s.files || []) f[`skills/${s.name}/${x.path}`] = x.content;
  }
  for (const a of Object.values(c.items.agent)) f[`agents/${a.name}.md`] = `---\nname: ${a.name}\ndescription: ${a.description}\ncapabilities: [${a.capabilities.join(', ')}]\nmodel_tier: ${a.model_tier ?? 'null'}\nmode: ${a.mode}\nread_only: ${a.read_only}\n---\n\n${a.body}`;
  for (const x of Object.values(c.items.instruction)) f[`instructions/${x.name}.md`] = `---\nid: ${x.name}\ntitle: ${x.title}\norder: ${x.order}\napplies_to: [${x.applies_to.join(', ')}]\n---\n\n${x.body}`;
  for (const m of Object.values(c.items.mcp)) { const { name, title, tags, notes, source, provenance, updated_at, findings, description, ...rest } = m; f[`mcp/${name}.yaml`] = toYaml({ name, ...rest, tags, notes }); }
  for (const r of Object.values(c.items.rule)) f[`rules/${r.name}.yaml`] = toYaml({ id: r.name, title: r.title, kind: r.kind, match: r.match, decision: r.decision, reason: r.reason || '', applies_to: r.applies_to });
  for (const m of Object.values(c.items.memory)) f[`memory/${m.name}.md`] = `---\nid: ${m.name}\ntype: ${m.type}\ntitle: ${m.title}\n---\n\n${m.body}\n`;
  for (const k of c.collections) f[`collections/${k.id}.yaml`] = toYaml({ id: k.id, title: k.title, description: k.description, icon: k.icon, color: k.color, order: k.order, members: k.members.map((m) => `${m.kind}/${m.name}`) });
  for (const [id, p] of Object.entries(c.profiles)) f[`profiles/${id}.yaml`] = toYaml({ collections: p.collections, enable: p.enable, disable: p.disable || {} });
  for (const [id, cl] of Object.entries(c.clients)) f[`clients/${id}.yaml`] = toYaml({ id, adapter: cl.adapter, display_name: cl.display_name, strict: cl.strict, roots: cl.roots, manage: cl.manage });
  return f;
}
export function changesBetween(base, cur) {
  const a = serialize(base), b = serialize(cur), out = [];
  for (const p of [...new Set([...Object.keys(a), ...Object.keys(b)])].sort()) {
    if (a[p] === b[p]) continue;
    const status = a[p] == null ? 'added' : b[p] == null ? 'deleted' : 'modified';
    out.push({ path: p, status, diff: unifiedDiff(a[p], b[p], p, p) });
  }
  return out;
}
export const contentHash = (c) => sha(JSON.stringify(serialize(c))).slice(0, 16);

// ---------- resolution ----------
export const memberOf = (c, kind, name) => c.collections.filter((k) => k.members.some((m) => m.kind === kind && m.name === name)).map((k) => k.id);
export function resolveClient(c, id) {
  const p = c.profiles[id] || { collections: [], enable: {}, disable: {} };
  const out = {};
  for (const kind of KINDS) {
    const explicit = new Set(p.enable?.[PK[kind]] || []);
    const via = new Set();
    for (const cid of p.collections || []) for (const m of c.collections.find((k) => k.id === cid)?.members || []) if (m.kind === kind) via.add(m.name);
    const off = new Set(p.disable?.[PK[kind]] || []);
    const all = new Set([...explicit, ...via]);
    out[kind] = { explicit, via, off, on: new Set([...all].filter((n) => !off.has(n) && c.items[kind][n])) };
  }
  return out;
}
const applies = (item, client) => !item.applies_to?.length || item.applies_to.includes(client);
export function cellFor(c, adapterCaps, clientId, kind, name, res = resolveClient(c, clientId)) {
  const item = c.items[kind][name];
  const concern = CONCERN[kind];
  if (!adapterCaps[concern]) return { state: 'unsupported', reason: `${c.clients[clientId].display_name} has no support for ${concern}.` };
  if (kind === 'instruction' && !applies(item, clientId)) return { state: 'unsupported', reason: `Only applies to ${item.applies_to.join(', ')}.` };
  const r = res[kind];
  const isOn = r.on.has(name);
  if (kind === 'mcp' && item.scan_status !== 'clean') return { state: 'blocked', reason: item.scan_status === 'findings' ? 'Scan found problems; fix them and rescan before enabling.' : 'Not scanned yet; run the scan before enabling.', enabled: isOn };
  return { state: isOn ? (r.explicit.has(name) ? 'on' : 'via_collection') : 'off' };
}
export function setEnabled(c, clientId, kind, name, enabled) {
  const p = (c.profiles[clientId] ||= { collections: [], enable: {}, disable: {} });
  p.enable ||= {}; p.disable ||= {};
  const en = new Set(p.enable[PK[kind]] || []), dis = new Set(p.disable[PK[kind]] || []);
  const via = resolveClient(c, clientId)[kind].via.has(name);
  if (enabled) { dis.delete(name); if (!via) en.add(name); }
  else { en.delete(name); if (via) dis.add(name); }
  p.enable[PK[kind]] = [...en].sort(); p.disable[PK[kind]] = [...dis].sort();
}

// ---------- rendering ----------
const LAYOUT = {
  claude_code: { root: 'home', skills: 'skills', agents: 'agents', instr: 'CLAUDE.md', mcp: ['project', '.mcp.json'] },
  opencode: { root: 'config', skills: 'skill', agents: 'agent', instr: 'AGENTS.md', mcp: ['config', 'opencode.json'] },
  hermes: { root: 'state', skills: 'skills', agents: null, instr: 'instructions.md', mcp: ['state', 'mcp.json'], memory: 'memories' },
  generic: { root: 'home', skills: 'skills', agents: null, instr: 'INSTRUCTIONS.md', mcp: null },
};
export function layoutFor(cfg) {
  const l = { ...LAYOUT[cfg.adapter] };
  const sp = cfg.spec;
  if (cfg.adapter === 'generic' && sp) { l.root = sp.skills?.root || Object.keys(cfg.roots)[0]; l.skills = sp.skills?.dir || 'skills'; l.instr = sp.instructions?.target || 'INSTRUCTIONS.md'; }
  return l;
}
export function renderClient(c, clientId, { caps, manageOverride } = {}) {
  const cfg = c.clients[clientId];
  const ad = D.ADAPTERS.find((a) => a.id === cfg.adapter);
  const capsX = caps || cfg.caps || ad.caps;
  const L = layoutFor(cfg);
  const res = resolveClient(c, clientId);
  const files = [], diags = [];
  const manage = { skills: true, agents: true, instructions: true, mcp: false, permissions: false, memory: false, ...(manageOverride || cfg.manage) };
  const put = (root, path, kind, text, source_ids, concern, merge = 'own') => files.push({ root, path, kind, text, source_ids, concern, merge, managed: !!manage[concern] });
  if (capsX.skills) for (const n of [...res.skill.on].sort()) { const s = c.items.skill[n]; put(L.root, `${L.skills}/${n}/SKILL.md`, 'skill', `---\nname: ${n}\ndescription: ${s.description}\n---\n\n${s.body}`, [n], 'skills'); for (const x of s.files || []) put(L.root, `${L.skills}/${n}/${x.path}`, 'skill', x.content, [n], 'skills'); }
  if (capsX.agents && L.agents) for (const n of [...res.agent.on].sort()) {
    const a = c.items.agent[n]; const tm = capsX.tool_map || {}; const tools = [];
    for (const cp of a.capabilities) { if (tm[cp]) tools.push(tm[cp]); else diags.push({ severity: cfg.strict ? 'error' : 'warn', code: 'unsupported_capability', message: `Agent "${n}" needs "${cp}" but ${cfg.display_name} has no native tool for it.`, item: n }); }
    const model = a.model_tier && capsX.model_tiers?.[a.model_tier];
    if (a.model_tier && !model) diags.push({ severity: 'warn', code: 'no_model_mapping', message: `No model is mapped for tier "${a.model_tier}"; the client default will be used.`, item: n });
    put(L.root, `${L.agents}/${n}.md`, 'agent', `---\nname: ${n}\ndescription: ${a.description}\ntools: ${tools.join(', ')}\n${model ? `model: ${model}\n` : ''}---\n\n${a.body}`, [n], 'agents');
  } else if (res.agent.on.size && !capsX.agents) diags.push({ severity: 'warn', code: 'unsupported_concern', message: `${cfg.display_name} cannot load subagents; ${res.agent.on.size} enabled agent(s) are ignored.` });
  if (capsX.instructions) {
    const list = [...res.instruction.on].map((n) => c.items.instruction[n]).filter((x) => applies(x, clientId)).sort((x, y) => x.order - y.order);
    if (list.length) put(L.root, L.instr, 'instruction', `<!-- agent-hub:begin -->\n${list.map((x) => x.body.trim()).join('\n\n')}\n<!-- agent-hub:end -->\n`, list.map((x) => x.name), 'instructions', 'block');
  }
  if (capsX.mcp && L.mcp) {
    const servers = [...res.mcp.on].map((n) => c.items.mcp[n]);
    for (const m of servers) if (m.scan_status !== 'clean') diags.push({ severity: 'error', code: 'mcp_not_clean', message: `MCP server "${m.name}" is ${m.scan_status}; only clean servers can be delivered.`, item: m.name });
    if (servers.length) put(L.mcp[0], L.mcp[1], 'mcp', JSON.stringify({ mcpServers: Object.fromEntries(servers.map((m) => [m.name, m.transport === 'http' ? { url: m.url } : { command: m.command, args: m.args, env: Object.fromEntries((m.env_names || []).map((e) => [e, `\${${e}}`])) }])) }, null, 2) + '\n', servers.map((m) => m.name), 'mcp', 'json_keys');
  }
  if (capsX.memory && L.memory) for (const n of [...res.memory.on].sort()) put(L.root, `${L.memory}/${n}.md`, 'memory', `${c.items.memory[n].body}\n`, [n], 'memory');
  return { files, diags };
}
const STRICT = { deny: 3, ask: 2, allow: 1 };
const finding = (code, message, extra = {}) => ({ code, severity: 'error', message, item: null, concern: null, rule: null, ...extra });
// Mirrors services/floor.py in spirit: content may only be stricter than the floor. Returns structured findings (never throws).
export function floorViolations(c, clientId) {
  const out = [];
  const res = resolveClient(c, clientId);
  const F = D.FLOOR;
  for (const r of Object.values(c.items.rule)) {
    if (!applies(r, clientId)) continue;
    if (r.kind === 'tool' && F.tools[r.match] && STRICT[r.decision] < STRICT[F.tools[r.match].max]) out.push(finding('tool_too_permissive', `Rule "${r.name}" sets tool ${r.match} to ${r.decision}, but the policy floor allows at most ${F.tools[r.match].max}.`, { item: r.name, concern: 'permissions', rule: `tools.${r.match}` }));
    if (r.kind === 'command' && r.decision === 'allow' && F.command_allow_forbid_patterns.includes(r.match)) out.push(finding('command_catch_all', `Rule "${r.name}" allows every command; the floor forbids a catch-all allow.`, { item: r.name, concern: 'permissions', rule: 'command_allow_forbid_patterns' }));
    if ((r.kind === 'path_read' || r.kind === 'path_write') && r.decision === 'allow' && F[`deny_${r.kind}`].some((p) => p === r.match)) out.push(finding('path_denied', `Rule "${r.name}" allows ${r.match}, which the floor denies.`, { item: r.name, concern: 'permissions', rule: `deny_${r.kind}` }));
  }
  for (const n of res.mcp.on) { const m = c.items.mcp[n]; if ((m.egress_hosts || []).includes('*')) out.push(finding('egress_wildcard', `MCP server "${n}" asks for wildcard egress; egress is deny-by-default.`, { item: n, concern: 'mcp', rule: 'egress.forbid_allow_patterns' })); }
  return out;
}

// ---------- planning ----------
export function planClient(c, clientId, applied, live, opts = {}) {
  const { files, diags } = renderClient(c, clientId, opts);
  const key = (f) => `${f.root}:${f.path}`;
  const rendered = new Map(files.map((f) => [key(f), f]));
  const ap = applied[clientId] || {}, lv = live[clientId] || {};
  const out = [];
  const shape = (f, action, diff, reason = '') => ({ root: f.root, path: f.path, action, kind: f.kind, source_ids: f.source_ids, diff, reason, merge: f.merge, managed: f.managed, concern: f.concern });
  for (const f of files) {
    const k = key(f);
    const liveText = lv[k] ?? null, lastText = ap[k] ?? null;
    if (!f.managed) { out.push(shape(f, lv[k] === f.text ? 'unchanged' : 'advisory', unifiedDiff(liveText, f.text, f.path, f.path), lv[k] === f.text ? '' : `${f.concern} is advisory for this client: the hub only reports`)); continue; }
    if (liveText != null && lastText != null && liveText !== lastText) { out.push(shape(f, 'conflict', unifiedDiff(liveText, f.text, f.path, f.path), 'live file differs from what the hub last wrote')); continue; }
    if (liveText === f.text) out.push(shape(f, 'unchanged', ''));
    else out.push(shape(f, liveText == null ? 'add' : 'change', unifiedDiff(liveText, f.text, f.path, f.path)));
  }
  for (const k of Object.keys(ap)) if (!rendered.has(k)) { const [root, ...p] = k.split(':'); const path = p.join(':'); out.push({ root, path, action: lv[k] === ap[k] ? 'remove' : 'conflict', kind: /SKILL|skill/.test(path) ? 'skill' : 'agent', source_ids: [], merge: 'own', concern: 'skills', diff: unifiedDiff(lv[k] ?? ap[k], null, path, path) }); }
  const order = { conflict: 0, change: 1, add: 2, remove: 3, advisory: 4, unchanged: 5 };
  out.sort((a, b) => order[a.action] - order[b.action] || a.path.localeCompare(b.path));
  const summary = { add: 0, change: 0, remove: 0, unchanged: 0, conflict: 0, advisory: 0 };
  for (const f of out) summary[f.action]++;
  const fv = floorViolations(c, clientId);
  const strict = c.clients[clientId].strict;
  const errs = diags.filter((d) => d.severity === 'error');
  const blocked = fv.length > 0 || (strict && errs.length > 0);
  const blocked_reasons = [...fv.map((v) => v.message), ...(strict ? errs.map((d) => d.message) : [])];
  const digest = sha(JSON.stringify(out.map((f) => [f.path, f.action, f.diff]))).slice(0, 16);
  return { client: clientId, files: out, diagnostics: diags, floor_violations: fv, summary, blocked, blocked_reasons, digest, _rendered: files };
}
