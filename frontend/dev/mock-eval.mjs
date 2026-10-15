// Mock of the F9 evaluation + observed-inventory routes (ARCHITECTURE Addendum F, F14). All data is invented; nothing here is a verified
// fact about the real backend. The queue is poll-driven: each GET /skill-evaluations/queue finishes the running job and starts the next.
import crypto from 'node:crypto';

const sha = (s) => crypto.createHash('sha256').update(s).digest('hex');
const TIERS = ['static', 'tier1', 'tier1+2'];
const rank = (t) => TIERS.indexOf(t);
const VERSION = '0.5.0';
const PUBLICATION = new Set(['Schema & Repository Governance', 'Semantic Version Validation', 'License Compliance', 'QUALITY', 'SCRIPT_LINT', 'RUBRIC_EVAL']);
const SEC = ['Prompt Injection', 'Command Injection', 'Secrets & Credentials', 'Unicode & Hidden Text', 'Code Integrity', 'PII Exposure', 'Dependency Vulnerabilities'];
const PUB = ['Schema & Repository Governance', 'Semantic Version Validation', 'License Compliance', 'QUALITY', 'SCRIPT_LINT'];
const MSG = {
  'Prompt Injection': ['critical', 'Instruction override phrase found in SKILL.md ("ignore all previous instructions")'],
  'Command Injection': ['high', 'scripts/install.sh pipes a remote download straight into a shell'],
  'Secrets & Credentials': ['high', 'scripts/diag.py reads every environment variable and prints it'],
  'Unicode & Hidden Text': ['medium', 'Zero-width characters found in the description'],
  'Code Integrity': ['medium', 'scripts/run.sh is executable but not listed in SKILL.md'],
  'PII Exposure': ['medium', 'An e-mail address appears in references/notes.md'],
  'Dependency Vulnerabilities': ['medium', 'requirements.txt pins a package with a known advisory (could not be checked offline)'],
  'Schema & Repository Governance': ['medium', 'metadata.author is missing'], QUALITY: ['medium', 'Description does not say when to use the skill'],
};

// name -> preset. gate cause is listed per name; `llm` marks a tier1 run that also has an LLM pass.
const P = (v, o = {}) => ({ v, q: o.q ?? ['B', 82], j: o.j ?? null, sec: o.sec || {}, llmSec: o.llmSec || {}, incomplete: o.inc || [], tiers: o.tiers || 'static', ago: o.ago ?? 30, stale: !!o.stale, t2: o.t2 || null });
const HUB_NAMES = ['code-review', 'commit-message', 'deploy-checklist', 'incident-triage', 'trace-reader', 'robust-scripts', 'docs-verify', 'a11y-audit', 'threat-model', 'sql-helper', 'python-typing', 'release-notes', 'test-first', 'pr-description'];
const PRESETS = {
  'hub:code-review': P('passed', { q: ['A', 94], j: 88, tiers: 'tier1+2', ago: 3, t2: 'passed' }), 'hub:commit-message': P('failed', { ago: 5 }),
  'hub:deploy-checklist': P('incomplete', { sec: { 'Dependency Vulnerabilities': 'incomplete' }, inc: ['Dependency Vulnerabilities'], ago: 8 }),
  'hub:incident-triage': P('failed', { sec: { 'PII Exposure': 'medium' }, ago: 20 }), 'hub:trace-reader': P('passed', { q: ['A', 91], ago: 11 }),
  'hub:robust-scripts': P('passed', { q: ['B', 86], j: 79, tiers: 'tier1', ago: 12 }), 'hub:docs-verify': P('passed', { q: ['A', 96], j: 92, tiers: 'tier1+2', ago: 2, t2: 'passed' }),
  'hub:a11y-audit': P('failed', { q: ['C', 71], ago: 40 }), 'hub:threat-model': P('failed', { stale: true, ago: 90 }), 'hub:sql-helper': P('error', { ago: 15 }),
  'hub:python-typing': P('passed', { ago: 50 }), 'hub:release-notes': P('failed', { q: ['C', 64], ago: 60 }), 'hub:test-first': P('passed', { q: ['B', 84], ago: 22 }),
  'hub:pr-description': P('passed', { q: ['B', 80], ago: 70 }),
  'curl-pipe-bash-installer': P('failed', { q: ['D', 41], sec: { 'Command Injection': 'critical' }, tiers: 'tier1', j: 35, ago: 6 }),
  'env-dump-diagnostics': P('failed', { q: ['C', 60], sec: { 'Secrets & Credentials': 'high' }, ago: 6 }),
  'ssh-config-audit': P('failed', { sec: { 'Unicode & Hidden Text': 'medium' }, ago: 9 }),
  'weekly-report-drafter': P('passed', { q: ['B', 85], j: 81, tiers: 'tier1', ago: 7 }), 'grafana-alert-triage': P('passed', { ago: 7 }), 'docker-cleanup-helper': P('failed', { q: ['C', 68], ago: 7 }),
  'csv-to-parquet': P('passed', { ago: 14 }), 'log-tail-summarizer': P('incomplete', { sec: { 'Dependency Vulnerabilities': 'incomplete' }, inc: ['Dependency Vulnerabilities'], ago: 14 }),
  'github-pr-workflow': P('passed', { q: ['A', 93], ago: 100 }), 'arxiv-search': P('passed', { ago: 100 }), 'systematic-debugging': P('failed', { ago: 100 }), 'plan': P('passed', { ago: 100 }),
  'xurl': P('failed', { sec: { 'Command Injection': 'medium' }, ago: 100 }), 'obsidian': P('passed', { ago: 100 }), 'nano-pdf': P('error', { ago: 100 }), 'excalidraw': P('passed', { ago: 100 }),
  'apple-notes': P('failed', { ago: 100 }), 'youtube-content': P('passed', { ago: 100 }),
  'prompt-injected-helper': P('failed', { q: ['E', 22], sec: { 'Prompt Injection': 'critical' }, tiers: 'tier1', ago: 4, llmSec: { 'Prompt Injection': 'critical' } }),
  'git-hygiene': P('passed', { ago: 18 }), 'port-scanner-notes': P('failed', { sec: { 'Command Injection': 'medium' }, ago: 18 }), 'shell-aliases': P('passed', { ago: 18 }),
  'meeting-notes': P('failed', { ago: 18 }), 'quick-todo': P('passed', { ago: 18 }),
};

// Observed inventories. [name, origin, extra]; extra: same (identical to the hub skill), differs (same name, other hash)
const O = (name, origin, extra = '', desc = '') => ({ name, origin, extra, desc: desc || `${name.replace(/[-_]/g, ' ')}: invented description for the mock inventory.` });
const OBSERVED = {
  hermes: {
    source: 'docker exec hermes-agent (python3, /opt/data/skills)',
    skills: [
      ...['apple-notes', 'github-pr-workflow', 'arxiv-search', 'ocr-and-documents', 'jupyter-live-kernel', 'systematic-debugging', 'plan', 'xurl', 'youtube-content', 'dogfood', 'webhook-subscriptions', 'blogwatcher', 'llm-wiki', 'obsidian', 'excalidraw', 'nano-pdf'].map((n) => O(n, 'shipped', '', `Bundled with the agent release: ${n.replace(/-/g, ' ')}.`)),
      ...['weekly-report-drafter', 'grafana-alert-triage', 'docker-cleanup-helper', 'csv-to-parquet', 'release-notes-writer', 'ssh-config-audit', 'curl-pipe-bash-installer', 'env-dump-diagnostics', 'log-tail-summarizer', 'pdf-merge-notes', 'prompt-injected-helper'].map((n) => O(n, 'agent', '', `Written by the agent itself: ${n.replace(/-/g, ' ')}.`)),
      O('commit-message', 'agent', 'differs', 'Agent-edited copy of the hub commit-message skill.'), O('code-review', 'agent', 'differs', 'Agent-edited copy of the hub code-review skill.'),
      ...['old-deploy-script', 'tmp-experiment-3', 'scratch-planner', 'legacy-sql-helper', 'unused-translator'].map((n) => O(n, 'archived', '', `Archived by the curator: ${n.replace(/-/g, ' ')}.`)),
      ...['deploy-checklist', 'incident-triage', 'trace-reader', 'robust-scripts', 'docs-verify'].map((n) => O(n, 'hub', 'same', `Delivered by the hub: ${n.replace(/-/g, ' ')}.`)),
      O('lab-health-check', 'lab_sync', '', 'Dropped in by the lab sync job.'), O('lab-mesh-notes', 'lab_sync', '', 'Dropped in by the lab sync job.'),
    ],
  },
  'claude-code': {
    source: 'discover() (skills directory)',
    skills: [O('git-hygiene', 'client'), O('port-scanner-notes', 'client'), O('shell-aliases', 'client'), O('meeting-notes', 'client'), O('quick-todo', 'client'), O('docs-verify', 'client', 'same', 'Same content as the hub docs-verify skill.'), O('refactor-helper', 'client'), O('regex-cheatsheet', 'client')],
  },
};

export function installEval(ctx) {
  const { route, clientIds, paginate, HttpErr, limits, now } = ctx;
  const content = new Proxy({}, { get: (_, k) => ctx.content()[k] });   // the mock reassigns its content tree on discard/revert
  const evals = new Map();     // subject -> records, newest first
  const byHash = new Map();    // content_hash -> newest finished record
  const observations = {};     // client -> {observed_at, complete, notes, skills:[...]}
  const queue = { running: null, queued: [], finished: 0, errors: 0 };
  let runSeq = 0;
  const bad = (m, code = 'bad_request') => new HttpErr(400, 'Bad request', m, code);

  // ---- hub side ----
  const hubHash = (name) => { const it = content.items.skill[name]; return it ? (it.__hash || sha(`${name}\0${it.description}\0${it.body}`)) : null; };
  const hubSkills = () => Object.keys(content.items.skill).sort();
  const hubByHash = (h) => hubSkills().find((n) => hubHash(n) === h) || null;

  // ---- observation ----
  function makeObserved(client, spec) {
    return spec.skills.map((s) => {
      const key = `${s.name.replace(/[^a-z0-9._-]/g, '-')}-${sha(`${client}/${s.name}`).slice(0, 8)}`;
      const path = s.origin === 'archived' ? `.archive/${s.name}` : s.origin === 'hub' ? `hub/${s.name}` : s.origin === 'lab_sync' ? `lab/${s.name}` : s.origin === 'client' ? s.name : s.name;
      const files = 1 + (sha(s.name).charCodeAt(0) % 4);
      return { key, path, name: s.name, description: s.desc, origin: s.origin, extra: s.extra, files, bytes: 700 + (sha(s.name).charCodeAt(1) % 40) * 90, subjectKey: key };
    });
  }
  const obsHash = (client, o) => (o.extra === 'same' && content.items.skill[o.name] ? hubHash(o.name) : sha(`${client}/${o.key}/${o.extra}/${o.name}`));
  function observe(client) {
    const spec = OBSERVED[client];
    const skills = spec ? makeObserved(client, spec) : [];
    observations[client] = { observed_at: now(), source: spec?.source || 'discover()', complete: !!spec || client === 'example', notes: spec || client === 'example' ? [] : ['This adapter cannot list skills outside the hub: nothing observed.'], skills };
    return observations[client];
  }
  const hubStatus = (client, o) => {
    const h = obsHash(client, o);
    const same = hubByHash(h);
    if (same) return { hub_status: 'in_hub_same', hub_name: same, enabled_for_client: !!(content.profiles[client]?.enable?.skills || []).includes(same) };
    if (content.items.skill[o.name]) return { hub_status: 'in_hub_differs', hub_name: o.name, enabled_for_client: false };
    return { hub_status: 'not_in_hub', hub_name: null, enabled_for_client: false };
  };
  function obsSummary(client) {
    const ob = observations[client]; if (!ob) return null;
    const by_origin = {}; let notIn = 0, differs = 0;
    for (const o of ob.skills) { by_origin[o.origin] = (by_origin[o.origin] || 0) + 1; const st = hubStatus(client, o).hub_status; if (st === 'not_in_hub') notIn++; else if (st === 'in_hub_differs') differs++; }
    return { observed_at: ob.observed_at, total: ob.skills.length, not_in_hub: notIn, in_hub_differs: differs, in_hub_same: ob.skills.length - notIn - differs, by_origin };
  }

  // ---- records ----
  function mkValidators(preset, llm) {
    const sevOf = (lvl) => ({ critical: lvl === 'critical' ? 1 : 0, high: lvl === 'high' ? 1 : 0, medium: lvl === 'medium' ? 1 : 0, low: 0 });
    const rows = [];
    for (const v of PUB) rows.push({ validator: v, status: v === 'Schema & Repository Governance' || (v === 'QUALITY' && preset.q[1] < 75) ? 'failed' : 'passed', severity: sevOf(v === 'Schema & Repository Governance' ? 'medium' : v === 'QUALITY' && preset.q[1] < 75 ? 'medium' : '') });
    const src = llm ? preset.llmSec : preset.sec;
    for (const v of SEC) { const lvl = src[v]; rows.push({ validator: v, status: lvl === 'incomplete' ? 'incomplete' : lvl ? 'failed' : 'passed', severity: sevOf(lvl === 'incomplete' ? '' : lvl) }); }
    if (llm) rows.push({ validator: 'RUBRIC_EVAL', status: 'passed', severity: sevOf('') });
    return rows;
  }
  function buildRecord(subject, meta, hash, tiers, preset, finishedAt, { reused } = {}) {
    const llm = tiers !== 'static';
    const vs = mkValidators(preset, false), vl = llm ? mkValidators({ ...preset, sec: preset.llmSec }, true) : null;
    const findings = (vl || vs).flatMap((v) => ['critical', 'high', 'medium'].filter((s) => v.severity[s]).map((s) => ({ severity: s, validator: v.validator, message: (MSG[v.validator] || [s, 'Issue reported'])[1] })));
    findings.sort((a, b) => ['critical', 'high', 'medium'].indexOf(a.severity) - ['critical', 'high', 'medium'].indexOf(b.severity));
    const sev = Object.fromEntries(['critical', 'high', 'medium', 'low'].map((s) => [s, vs.reduce((n, v) => n + v.severity[s], 0)]));
    const err = preset.v === 'error';
    return { run_id: `r${(++runSeq).toString().padStart(4, '0')}${sha(subject + runSeq).slice(0, 6)}`, subject, source: meta.source, path: meta.path, origin: meta.origin, ...(meta.source === 'hub' ? { skill: meta.name } : {}),
      tiers, launcher: 'skillevaluator-local', evaluator_version: VERSION, started_at: finishedAt - 4, finished_at: finishedAt, content_hash: hash, files: meta.files || 2, bytes: meta.bytes || 1800, actor: 'admin',
      model_calls: llm, verdict: preset.v, error: err ? 'the tier1 report has no overall_status' : null, duration_ms: reused ? 0 : 1800 + (hash.charCodeAt(0) % 40) * 90,
      tier1: err ? null : { status: preset.v, quality_score: preset.q[1], grade: preset.q[0], judge_status: llm ? 'succeeded' : 'skipped', judge_score: llm ? (preset.j ?? 74) : null, errors: findings.filter((f) => f.severity !== 'medium').length + 1, warnings: 3, severity: sev, incomplete: preset.incomplete },
      tier2: tiers === 'tier1+2' ? { status: preset.t2 || 'passed', errors: 0, warnings: 1, error: null } : null,
      findings: findings.slice(0, 20), findings_total: findings.length, validators: err ? null : vs, static: err ? null : { status: preset.v, quality_score: preset.q[1], grade: preset.q[0], errors: 1, warnings: 3, severity: sev, incomplete: preset.incomplete },
      security_findings: findings.filter((f) => !PUBLICATION.has(f.validator)).map((f) => ({ ...f, pass: 'static' })), validators_llm: err ? null : vl,
      reused_from: reused || null, reports: ['tier1.json', 'tier1.md'] };
  }
  function metaOf(subject) {
    const i = subject.indexOf(':'); const src = subject.slice(0, i), key = subject.slice(i + 1);
    if (src === 'hub') { const it = content.items.skill[key]; return it ? { source: 'hub', name: key, path: `skills/${key}`, origin: 'hub', hash: hubHash(key), files: 1 + (it.files || []).length, bytes: String(it.body || '').length + 200 } : null; }
    const o = observations[src]?.skills.find((x) => x.key === key);
    return o ? { source: src, name: o.name, path: o.path, origin: o.origin, hash: obsHash(src, o), files: o.files, bytes: o.bytes, key } : null;
  }
  const presetFor = (subject, name) => PRESETS[subject] || PRESETS[name] || null;
  function seed() {
    for (const n of HUB_NAMES) {
      const subject = `hub:${n}`; const m = metaOf(subject); const p = PRESETS[subject]; if (!m || !p) continue;
      const hash = p.stale ? sha(`old/${n}`) : m.hash;
      const rec = buildRecord(subject, m, hash, p.tiers, p, now() - 3600 * p.ago);
      evals.set(subject, [rec]); byHash.set(hash, rec);
    }
    for (const client of Object.keys(OBSERVED)) {
      observe(client); observations[client].observed_at = now() - 2400;
      for (const o of observations[client].skills) {
        const subject = `${client}:${o.key}`; const p = presetFor(subject, o.name); if (!p || (client === 'hermes' && o.origin === 'hub')) continue;
        const m = metaOf(subject); const rec = buildRecord(subject, m, m.hash, p.tiers, p, now() - 3600 * p.ago);
        evals.set(subject, [rec]); byHash.set(m.hash, rec);
      }
    }
  }

  // ---- gate (F7/F14): static security findings + high/critical LLM security findings ----
  function gateOf(hash) {
    const min = limits.eval_gate_min_tiers || 'static';
    const rec = byHash.get(hash);
    if (!rec || rec.verdict === 'error' || rank(rec.tiers) < rank(min)) return { status: 'missing', verdict: null, run_id: null, tiers: null, reasons: [] };
    const sec = (list) => (list || []).filter((v) => !PUBLICATION.has(v.validator));
    const reasons = []; let blocked = false, warn = false;
    for (const v of sec(rec.validators)) { if (v.severity.critical || v.severity.high) { blocked = true; reasons.push(`${v.validator}: ${(MSG[v.validator] || [0, 'blocking finding'])[1]}`); } else if (v.status === 'incomplete') { blocked = true; reasons.push(`${v.validator}: scan incomplete`); } else if (v.severity.medium) warn = true; }
    for (const v of sec(rec.validators_llm)) if (v.severity.critical || v.severity.high) { blocked = true; reasons.push(`${v.validator} (LLM): ${(MSG[v.validator] || [0, 'blocking finding'])[1]}`); }
    return { status: blocked ? 'blocked' : warn ? 'warn' : 'ok', verdict: rec.verdict, run_id: rec.run_id, tiers: rec.tiers, reasons: reasons.slice(0, 5) };
  }
  const gateField = (hash) => (limits.eval_gate === 'off' && limits.eval_gate_observed === 'off' ? undefined : gateOf(hash).status);

  // ---- rows ----
  function rowFor(subject, m) {
    const recs = evals.get(subject) || []; const latest = recs[0] || null;
    const state = !latest ? 'none' : latest.content_hash === m.hash ? 'current' : 'stale';
    const HEAVY = ['validators', 'validators_llm', 'security_findings', 'static'];
    const slim = latest ? Object.fromEntries(Object.entries(latest).filter(([k]) => !HEAVY.includes(k))) : null;
    if (slim && latest.security_findings) slim.security_severity = { critical: latest.security_findings.filter((f) => f.severity === 'critical').length, high: latest.security_findings.filter((f) => f.severity === 'high').length };   // PROPOSED field, not in the backend yet
    const row = { subject, source: m.source, name: m.name, key: m.key || m.name, path: m.path, origin: m.origin, content_hash: m.hash, state, latest: slim, queued: queue.queued.some((j) => j.subject === subject), running: queue.running?.subject === subject, gate: gateField(m.hash) };
    row.skill = m.name;
    if (m.source !== 'hub') { const o = observations[m.source].skills.find((x) => x.key === m.key); Object.assign(row, hubStatus(m.source, o), { description: o.description }); }
    if (row.gate === undefined) delete row.gate;
    return row;
  }
  function allSubjects(source) {
    const out = [];
    if (source === 'hub' || source === 'all') for (const n of hubSkills()) out.push(`hub:${n}`);
    for (const c of Object.keys(observations)) if (source === 'all' || source === c) for (const o of observations[c].skills) out.push(`${c}:${o.key}`);
    return out;
  }
  const rowsFor = (source) => allSubjects(source).map((s) => rowFor(s, metaOf(s))).filter(Boolean);
  function summarize(rows) {
    const s = { total: rows.length, by_state: {}, by_verdict: {}, by_gate: {} };
    for (const r of rows) { s.by_state[r.state] = (s.by_state[r.state] || 0) + 1; if (r.latest) s.by_verdict[r.latest.verdict] = (s.by_verdict[r.latest.verdict] || 0) + 1; if (r.gate) s.by_gate[r.gate] = (s.by_gate[r.gate] || 0) + 1; }
    return s;
  }

  // ---- queue ----
  const queuePublic = () => ({ running: queue.running ? { subject: queue.running.subject, run_id: queue.running.run_id, tiers: queue.running.tiers, started_at: queue.running.started_at } : null,
    queued: queue.queued.slice(0, 200).map((j) => ({ subject: j.subject, tiers: j.tiers })), queued_total: queue.queued.length, finished_since_start: queue.finished, errors_since_start: queue.errors });
  function startNext() {
    const j = queue.queued.shift();
    queue.running = j ? { ...j, run_id: `r${(++runSeq).toString().padStart(4, '0')}live`, started_at: now() } : null;
  }
  function finishRunning() {
    const j = queue.running; if (!j) return;
    const m = metaOf(j.subject);
    if (m) {
      const prior = byHash.get(m.hash);
      const reuse = !j.force && prior && prior.verdict !== 'error' && rank(prior.tiers) >= rank(j.tiers) && prior.evaluator_version === VERSION;
      const preset = presetFor(j.subject, m.name) || P(sha(m.hash).charCodeAt(2) % 5 === 0 ? 'passed' : 'failed', { q: ['B', 70 + (sha(m.hash).charCodeAt(3) % 25)] });
      const rec = reuse ? { ...structuredClone(prior), run_id: `r${(++runSeq).toString().padStart(4, '0')}reuse`, subject: j.subject, source: m.source, path: m.path, origin: m.origin, finished_at: now(), duration_ms: 0, reused_from: { subject: prior.subject, run_id: prior.run_id } }
        : buildRecord(j.subject, m, m.hash, j.tiers, { ...preset, stale: false }, now());
      if (rec.source === 'hub') rec.skill = m.name; else delete rec.skill;
      evals.set(j.subject, [rec, ...(evals.get(j.subject) || [])].slice(0, 10)); byHash.set(m.hash, rec);
      if (rec.verdict === 'error') queue.errors++;
    }
    queue.finished++; queue.running = null;
  }
  function enqueue(subjects, tiers, force) {
    const queued = [], skipped = [];
    for (const s of subjects) {
      if (!metaOf(s)) { skipped.push({ subject: s, reason: 'not_found' }); continue; }
      if (queue.queued.some((j) => j.subject === s) || queue.running?.subject === s) { skipped.push({ subject: s, reason: queue.running?.subject === s ? 'running' : 'queued' }); continue; }
      if (queue.queued.length >= 5000) { skipped.push({ subject: s, reason: 'queue_full' }); continue; }
      queue.queued.push({ subject: s, tiers, force: !!force }); queued.push(s);
    }
    if (!queue.running) startNext();
    return { queued, skipped };
  }
  const SUBJECT = /^[a-z0-9][a-z0-9._-]{0,80}:[A-Za-z0-9][A-Za-z0-9._-]{0,120}$/;
  const subjectOr400 = (s) => { if (!SUBJECT.test(s)) throw bad('invalid subject', 'invalid_subject'); return s; };

  // ---- routes ----
  route('GET', '/skill-evaluator', (r) => r.json(200, { installed: true, launcher: 'skillevaluator-local', version: VERSION, wrapper: true, env_names: ['OPENAI_BASE_URL'], install_hint: 'pipx install skillevaluator', tiers: TIERS.map((id) => ({ id, help: { static: 'Static scanners only; no model calls.', tier1: 'Static scan plus an LLM pass for quality and the judge.', 'tier1+2': 'Tier 1 plus intra-skill duplication.' }[id] })), keep_runs: 10, timeout_s: 600, running: queue.running ? { skill: queue.running.subject.replace(/^hub:/, ''), run_id: queue.running.run_id, tiers: queue.running.tiers, started_at: queue.running.started_at } : null }));
  route('GET', '/skill-evaluations', (r) => {
    const q = r.url.searchParams; const source = q.get('source') || 'hub';
    if (source !== 'all' && source !== 'hub' && !clientIds().includes(source)) throw bad(`unknown source: ${source}`, 'unknown_source');
    let rows = rowsFor(source);
    for (const k of ['state', 'origin', 'gate']) if (q.get(k)) rows = rows.filter((x) => (x[k] ?? (k === 'state' ? 'none' : '')) === q.get(k));
    if (q.get('verdict')) rows = rows.filter((x) => x.latest?.verdict === q.get('verdict'));
    const text = (q.get('q') || '').toLowerCase(); if (text) rows = rows.filter((x) => `${x.name} ${x.path}`.toLowerCase().includes(text));
    rows.sort((a, b) => a.name.localeCompare(b.name));
    r.json(200, { ...paginate(rows, r.url, 500, 200), summary: summarize(rows), running: queuePublic().running });
  });
  route('POST', '/skill-evaluations/run', (r) => {
    const b = r.body || {}; const tiers = b.tiers || 'static';
    if (!TIERS.includes(tiers)) return r.problem(422, 'Unprocessable', `tiers must be one of ${TIERS.join(', ')}`, 'validation_error');
    if (!!b.subjects === !!b.select) return r.problem(422, 'Unprocessable', 'give exactly one of subjects / select', 'validation_error');
    let subjects;
    if (b.subjects) { if (!Array.isArray(b.subjects) || !b.subjects.length) return r.problem(422, 'Unprocessable', 'subjects must be a non-empty list', 'validation_error'); subjects = b.subjects.map(subjectOr400); }
    else {
      if (!['none_or_stale', 'all'].includes(b.select)) return r.problem(422, 'Unprocessable', 'select must be none_or_stale or all', 'validation_error');
      const source = b.source || 'hub'; if (source !== 'all' && source !== 'hub' && !clientIds().includes(source)) throw bad(`unknown source: ${source}`, 'unknown_source');
      subjects = rowsFor(source).filter((x) => b.select === 'all' || x.state !== 'current').map((x) => x.subject);
    }
    const o = enqueue(subjects, tiers, b.force);
    r.json(202, { ...o, run_ids: Object.fromEntries(o.queued.map((x) => [x, null])) });
  });
  route('GET', '/skill-evaluations/queue', (r) => { finishRunning(); if (!queue.running) startNext(); r.json(200, queuePublic()); });
  route('POST', '/skill-evaluations/queue/cancel', (r) => { const n = queue.queued.length + (queue.running ? 1 : 0); queue.queued = []; queue.running = null; r.json(200, { cancelled: n }); });
  const detailOf = (subject) => {
    const m = metaOf(subject); if (!m) throw new HttpErr(404, 'Not found', `no such subject: ${subject}`, 'not_found');
    const row = rowFor(subject, m); const recs = evals.get(subject) || [];
    return { ...row, latest: recs[0] || null, history: recs.map((x) => ({ ...x, state: x.content_hash === m.hash ? 'current' : 'stale' })), running: queue.running?.subject === subject ? queuePublic().running : null, gate: limits.eval_gate === 'off' ? null : gateOf(m.hash) };
  };
  route('GET', '/skill-evaluations/:subject', (r) => r.json(200, detailOf(subjectOr400(r.p.subject))));
  route('GET', '/skill-evaluations/:subject/runs/:run_id/report', (r) => {
    const subject = subjectOr400(r.p.subject); const rec = (evals.get(subject) || []).find((x) => x.run_id === r.p.run_id);
    if (!rec) throw new HttpErr(404, 'Not found', 'no such run', 'not_found');
    const md = [`# Evaluation report: ${subject}`, '', `Evaluator ${rec.evaluator_version}, tiers \`${rec.tiers}\`, verdict **${rec.verdict}**.`, '', '## Static scan', '',
      '| Validator | Status |', '|---|---|', ...(rec.validators || []).map((v) => `| ${v.validator} | ${v.status} |`), '',
      '## Findings', '', ...(rec.findings.length ? rec.findings.map((f) => `- **${f.severity}** ${f.validator}: ${f.message}`) : ['No findings at medium or above.']), '',
      rec.origin === 'agent' && rec.verdict === 'failed' ? '> Quoted from the skill: <script>alert("xss")</script> [click](javascript:alert(1)) ![x](http://evil.example/p.png)\n' : '',
      '## Notes', '', 'Scrubbed: secret-shaped text is replaced by `[redacted]` before it reaches this page.'].join('\n');
    r.json(200, { subject, run_id: rec.run_id, markdown: md, truncated: false });
  });
  // legacy single-skill routes map to hub:<name>
  route('POST', '/skills/:name/evaluate', (r) => { const s = `hub:${r.p.name}`; if (!metaOf(s)) throw new HttpErr(404, 'Not found', 'no such skill', 'not_found'); const o = enqueue([s], (r.body || {}).tiers || 'static', true); r.json(202, { subject: s, skill: r.p.name, status: queue.running?.subject === s ? 'running' : 'queued', run_id: queue.running?.subject === s ? queue.running.run_id : null, tiers: (r.body || {}).tiers || 'static' }); void o; });
  route('GET', '/skills/:name/evaluations', (r) => { const s = `hub:${r.p.name}`; if (!metaOf(s)) throw new HttpErr(404, 'Not found', 'no such skill', 'not_found'); r.json(200, { ...detailOf(s), skill: r.p.name }); });

  route('POST', '/clients/:id/observe', (r) => {
    const id = r.p.id; if (!clientIds().includes(id)) throw new HttpErr(404, 'Not found', `no such client: ${id}`, 'not_found');
    const ob = observe(id); const s = obsSummary(id);
    const by_hub_status = { in_hub_same: s.total - s.not_in_hub - s.in_hub_differs, in_hub_differs: s.in_hub_differs, not_in_hub: s.not_in_hub };
    r.json(200, { client: id, observed_at: ob.observed_at, complete: ob.complete, total: s.total, by_origin: s.by_origin, by_hub_status, notes: ob.notes });
  });
  route('GET', '/clients/:id/observed', (r) => {
    const id = r.p.id; if (!clientIds().includes(id)) throw new HttpErr(404, 'Not found', `no such client: ${id}`, 'not_found');
    const ob = observations[id]; if (!ob) throw new HttpErr(404, 'Not found', 'this client has not been observed yet', 'not_observed');
    const q = r.url.searchParams;
    let items = ob.skills.map((o) => { const subject = `${id}:${o.key}`; const m = metaOf(subject); const row = rowFor(subject, m);
      return { key: o.key, path: o.path, name: o.name, description: o.description, origin: o.origin, content_hash: m.hash, files: o.files, bytes: o.bytes, ...hubStatus(id, o), evaluation: row.latest ? { state: row.state, verdict: row.latest.verdict, gate: row.gate ?? null, run_id: row.latest.run_id } : null }; });
    if (q.get('origin')) items = items.filter((x) => x.origin === q.get('origin'));
    if (q.get('hub_status')) items = items.filter((x) => x.hub_status === q.get('hub_status'));
    if (q.get('q')) items = items.filter((x) => `${x.name} ${x.path}`.toLowerCase().includes(q.get('q').toLowerCase()));
    r.json(200, { client: id, observed_at: ob.observed_at, source: ob.source, complete: ob.complete, notes: ob.notes, summary: obsSummary(id), ...paginate(items, r.url, 500, 200) });
  });
  route('POST', '/clients/:id/observed/import', (r) => {
    const id = r.p.id; if (!clientIds().includes(id)) throw new HttpErr(404, 'Not found', `no such client: ${id}`, 'not_found');
    const ob = observations[id]; if (!ob) throw new HttpErr(404, 'Not found', 'this client has not been observed yet', 'not_observed');
    const b = r.body || {}; const mode = limits.eval_gate_observed || 'require';
    if (!Array.isArray(b.items) || !b.items.length) return r.problem(422, 'Unprocessable', 'items must be a non-empty list', 'validation_error');
    const onc = b.on_conflict || 'skip'; if (!['skip', 'rename', 'replace', 'link'].includes(onc)) return r.problem(422, 'Unprocessable', 'on_conflict must be skip, rename, replace or link', 'validation_error');
    const reason = b.override_reason; if (reason != null && (typeof reason !== 'string' || reason.trim().length < 10 || reason.length > 500)) return r.problem(422, 'Unprocessable', 'override_reason must be 10..500 characters', 'validation_error');
    const results = [];
    for (const it of b.items) {
      const o = ob.skills.find((x) => x.key === it.key);
      if (!o) { results.push({ key: it.key, final_name: it.name || it.key, status: 'error', gate: null, detail: 'unknown observed key' }); continue; }
      let name = it.name || o.name;
      if (!/^[a-z0-9][a-z0-9._-]{0,63}$/.test(name)) { results.push({ key: it.key, final_name: name, status: 'error', gate: null, detail: 'invalid skill name' }); continue; }
      const g = gateOf(obsHash(id, o)); let gate = mode === 'off' ? null : g.status;
      if (mode === 'require' && (g.status === 'missing' || g.status === 'blocked')) {
        if (g.status === 'missing') { results.push({ key: it.key, final_name: name, status: 'refused', gate, detail: 'This skill has no evaluation at the required tier.', code: 'eval_required' }); continue; }
        if (!reason) { results.push({ key: it.key, final_name: name, status: 'refused', gate, detail: 'The gate blocks this skill and no override reason was given.', code: 'eval_blocked' }); continue; }
        gate = 'overridden';
      }
      const exists = !!content.items.skill[name]; let status = 'created';
      if (exists) {
        if (onc === 'skip') { results.push({ key: it.key, final_name: name, status: 'skipped', gate, detail: 'a skill with this name already exists' }); continue; }
        if (onc === 'link') { if (b.enable) { const p = (content.profiles[id] ||= { collections: [], enable: {}, disable: {} }); (p.enable.skills ||= []).includes(name) || p.enable.skills.push(name); } results.push({ key: it.key, final_name: name, status: 'linked', gate, detail: 'linked to the existing hub skill' }); continue; }
        if (onc === 'rename') { let n = 2; while (content.items.skill[`${name}-${n}`]) n++; name = `${name}-${n}`; status = 'renamed'; } else status = 'replaced';
      }
      content.items.skill[name] = { name, description: o.description, body: `# ${o.name}\n\nImported from ${id} (${o.origin}).\n`, files: [], tags: [], notes: '', source: `observed:${id}`, provenance: { client: id, path: o.path, origin: o.origin, content_hash: obsHash(id, o) }, updated_at: now(), __hash: obsHash(id, o) };
      if (b.enable) { const p = (content.profiles[id] ||= { collections: [], enable: {}, disable: {} }); (p.enable.skills ||= []).includes(name) || p.enable.skills.push(name); }
      results.push({ key: it.key, final_name: name, status, gate, detail: gate === 'overridden' ? 'imported with an override reason' : '' });
    }
    r.json(200, { client: id, results });
  });

  seed();
  return { obsSummary, observations, gateOf };
}
