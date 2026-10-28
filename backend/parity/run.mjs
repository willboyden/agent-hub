#!/usr/bin/env node
// Differential harness (Node 22, zero dependencies): runs the same corpus against two implementations of the hub (or
// the knowledge service) in the SAME hermetic temp layout on the SAME port, one after the other, and compares status,
// chosen headers, normalised bodies and the files each implementation wrote.
//
//   node backend/parity/run.mjs                              # hub: python (the last Python backend) vs rust
//   node backend/parity/run.mjs --a python@v0.2.0            # against the last Python release tag (any git ref)
//   node backend/parity/run.mjs --a python@/abs/py-tree      # against a Python tree on disk (lib/python-release.mjs)
//   node backend/parity/run.mjs --a python --b python        # proves the normalisation: must report 0 diffs
//   node backend/parity/run.mjs --app knowledge              # the knowledge service
//   node backend/parity/run.mjs --only '^keys\.'             # a subset (regex on case ids)
//   node backend/parity/run.mjs --json report.json           # machine-readable report
//   node backend/parity/run.mjs --dump results.json          # every normalised result of both sides (debugging)
//   node backend/parity/run.mjs --a python@/abs/py-tree --write-rust-expect
//                                                            # record Rust's dsh.* answers (only if that tree agrees)
//
// `rust` is backend/target/debug (`cargo build` first, or PARITY_RUST_BIN_DIR); `python` / `python@<ref>` is the
// Python backend + knowledge service of that git ref, exported and given venvs under backend/target/python-release
// (lib/python-release.mjs; needs uv and Python 3.13). Both sides start from state the PYTHON code seeded (`hubctl
// init`), so the Rust server is also proven to open a hub.db and a content repo Python wrote.
// Exit: 0 = no differences other than routes the Rust side reports as `not_ported` (501); 1 = real differences or
// uncovered routes; 2 = harness failure.
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { request as httpRequest } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { isDeepStrictEqual } from 'node:util';

import { CASES as HUB_CASES } from './corpus/hub.mjs';
import { CASES as KNOWLEDGE_CASES } from './corpus/knowledge.mjs';
import { FAKE_KNOWLEDGE_KEY, startFakeKnowledge } from './lib/fake-knowledge.mjs';
import { RULES, normHeaders, normMetrics, normSse, normValue, normString } from './lib/normalize.mjs';
import {
  PARITY, freePort, hubCommand, hubEnv, knowledgeCommand, readKey, resetTmp, seedWithPython, startServer, stopServer,
  writeKnowledgeConfig,
} from './lib/servers.mjs';
import { snapshot } from './lib/snapshot.mjs';

function args(argv) {
  const o = { app: 'hub', a: 'python', b: 'rust', only: null, json: null, dump: null, writeRustExpect: false };
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i].replace(/^--/, '');
    if (k === 'write-rust-expect') o.writeRustExpect = true;
    else if (k in o) o[k] = argv[++i];
  }
  return o;
}

// Rust's own answers to the cases a reference without the new adapter cannot check (the `dsh.*` cases, A-15), recorded
// by `--write-rust-expect` from a run in which a Python tree WITH the adapter and Rust agreed on every one of them. A run
// against a reference without the adapter (the default, c419f1e) still allows those cases, but checks the Rust side of
// each against this record, so a changed Rust answer is a difference, not an allowance.
const RUST_EXPECT = join(PARITY, 'expect', 'rust-dsh.json');
const loadRustExpect = () => (existsSync(RUST_EXPECT) ? JSON.parse(readFileSync(RUST_EXPECT, 'utf8')).cases ?? {} : null);
/** The run's temp dir -> `<TMP>` in every string and key (both sides of one run share it; records outlive the run). A
 *  file snapshot entry that keeps its text gets `size` and `sha256` of the rewritten text, so they stay comparable; a
 *  plan's `digest` (a hash over rendered files that name the temp dir) becomes `<DIGEST>`. Only for the recorded Rust
 *  answers: the two sides of one run are still compared unchanged. */
function untmp(v, tmp) {
  if (typeof v === 'string') return v.split(tmp).join('<TMP>');
  if (Array.isArray(v)) return v.map((x) => untmp(x, tmp));
  if (!v || typeof v !== 'object') return v;
  const out = Object.fromEntries(Object.entries(v).map(([k, x]) => [
    untmp(k, tmp),
    k === 'digest' && typeof x === 'string' && /^[0-9a-f]{64}$/.test(x) ? '<DIGEST>' : untmp(x, tmp),
  ]));
  if (typeof v.text === 'string' && v.text.includes(tmp) && typeof v.sha256 === 'string') {
    const bytes = Buffer.from(out.text, 'utf8');
    out.size = bytes.length;
    out.sha256 = createHash('sha256').update(bytes).digest('hex');
  }
  return out;
}

function send(port, { method, path, headers, body }) {
  return new Promise((resolve, reject) => {
    const req = httpRequest({ host: '127.0.0.1', port, method, path, headers, agent: false }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => resolve({ status: res.statusCode, headers: new Headers(Object.entries(res.headers).map(([k, v]) => [k, Array.isArray(v) ? v.join(', ') : String(v)])), body: Buffer.concat(chunks) }));
      res.on('error', reject);
    });
    req.on('error', reject);
    req.setTimeout(30000, () => req.destroy(new Error('timeout')));
    if (body != null) req.write(body);
    req.end();
  });
}

const fill = (s, vars) => (typeof s === 'string' ? s.replace(/\$\{(\w+)\}/g, (_, k) => (k in vars ? String(vars[k]) : `\${${k}}`)) : s);
const fillDeep = (v, vars) => (Array.isArray(v) ? v.map((x) => fillDeep(x, vars)) : v && typeof v === 'object'
  ? Object.fromEntries(Object.entries(v).map(([k, x]) => [k, fillDeep(x, vars)])) : fill(v, vars));

function pick(json, path) {
  let cur = json;
  for (const p of path.split('.')) cur = cur == null ? undefined : cur[/^\d+$/.test(p) ? Number(p) : p];
  return cur;
}

async function runCases(cases, port, keys, vars, tmp, only, csrfHeader) {
  const out = [];
  for (const c of cases) {
    if (only && !new RegExp(only).test(c.id)) continue;
    const method = c.m;
    let path = fill(c.p, vars);
    if (c.q) path += `?${new URLSearchParams(Object.entries(c.q).map(([k, v]) => [k, fill(v, vars)])).toString()}`;
    const headers = { host: fill(c.host ?? '127.0.0.1:${PORT}', vars), accept: 'application/json' };
    const who = c.as ?? 'admin';
    if (who === 'bad') headers.authorization = 'Bearer hc_not-a-real-key-0000000000000000000000000';
    else if (who !== 'none') {
      const k = keys[who] ?? vars[`${who}Key`];
      if (k) headers.authorization = `Bearer ${k}`;
    }
    if (method !== 'GET' && method !== 'HEAD' && c.csrf !== false) headers[c.csrfHeader ?? csrfHeader] = '1';
    if (c.origin) headers.origin = fill(c.origin, vars);
    for (const [k, v] of Object.entries(c.headers ?? {})) headers[k] = fill(v, vars);
    let body = null;
    if (c.raw != null) { body = Buffer.from(fill(c.raw, vars)); headers['content-type'] = c.ctype ?? 'application/json'; }
    else if (c.body !== undefined) { body = Buffer.from(JSON.stringify(fillDeep(c.body, vars))); headers['content-type'] = c.ctype ?? 'application/json'; }
    if (body != null) headers['content-length'] = String(body.length);
    let res;
    try { res = await send(port, { method, path, headers, body }); } catch (e) { out.push({ id: c.id, error: String(e.message) }); continue; }
    const ctype = res.headers.get('content-type') || '';
    let parsed;
    const text = res.body.toString('utf8');
    if (c.text) parsed = c.p === '/metrics' ? normMetrics(text) : ctype.includes('event-stream') ? normSse(text) : normString(text);
    else if (ctype.includes('json') && text) { try { parsed = JSON.parse(text); } catch { parsed = `<invalid json> ${text.slice(0, 200)}`; } }
    else parsed = text.length > 4000 ? `<${text.length} bytes sha:${(await import('node:crypto')).createHash('sha256').update(res.body).digest('hex')}>` : text;
    for (const [v, from] of Object.entries(c.save ?? {})) {
      const val = from.startsWith('@header:') ? res.headers.get(from.slice(8)) : (parsed && typeof parsed === 'object' ? pick(parsed, from) : undefined);
      if (val != null) vars[v] = val;
    }
    const rec = { id: c.id, status: res.status, headers: normHeaders(res.headers), body: typeof parsed === 'string' ? parsed : normValue(parsed) };
    if (c.snap) rec.files = snapshot(tmp);
    out.push(rec);
  }
  return out;
}

// Deliberate differences (backend/PORTING.md "Deviations"), by case id: `headers` = only the headers may differ (the
// status and body must still be equal); `all` = the case may differ entirely. Reported as "allowed", never as "equal".
// `rust` = what the Rust side itself must answer (status and normalised body), checked on every run that includes
// Rust, so an `all` entry cannot hide a wrong Rust answer.
const ALLOWED = {
  'wsf.policy.rule-not-a-mapping-500': { what: 'headers', deviation: 'D-500-HEADERS' },
  'wsf.policy.rules-not-iterable-500': { what: 'headers', deviation: 'D-500-HEADERS' },
  'wsf.policy.mcp-bad-url-500': { what: 'headers', deviation: 'D-500-HEADERS' },
  'core.body.deep-json-100': { what: 'headers', deviation: 'D-500-HEADERS' },
  'core.body.deep-json-200': { what: 'headers', deviation: 'D-500-HEADERS' },
  'b1.skills.put-files-not-dict': { what: 'headers', deviation: 'D-500-HEADERS' },
  'core.inbox.client-traversal': {
    what: 'all', deviation: 'D-INBOX-CLIENT (Python security bug SB-3)',
    rust: { status: 200, body: { items: [], next_cursor: null } },
  },
  // Python's own result varies between runs (it also fails python-vs-python): uvicorn may reset the connection while
  // the client is still uploading, before the client reads the 413; Rust drains the upload (PORTING.md A-11 §7.4)
  'wsd.body-over-cap': { what: 'all', deviation: 'flaky on the Python side; D-K-OVERCAP-DRAIN' },
};

// The DeepSeek Harness adapter is new in 0.3.0 (PORTING.md A-15): the default reference c419f1e does not have it. When
// exactly ONE side's own /adapters answer lacks it, that one entry is dropped from both sides' adapter lists before
// comparing (every other field is still compared) and the dsh.* cases are reported as allowed. When both sides have it
// (`--a python@<tree with the adapter>`) or neither does, nothing changes.
const NEW_ADAPTER = 'deepseek-harness';
const hasNewAdapter = (side) => side.some((r) => r.id === 'adapters' && r.body?.items?.some((i) => i?.id === NEW_ADAPTER));
function withoutNewAdapter(rec) {
  const r = structuredClone(rec);
  const drop = (list) => list.filter((x) => x !== NEW_ADAPTER && !(x && typeof x === 'object' && x.id === NEW_ADAPTER && 'caps' in x));
  if (Array.isArray(r.body?.adapters)) r.body.adapters = drop(r.body.adapters); // /health
  if (Array.isArray(r.body?.info?.adapters)) r.body.info.adapters = drop(r.body.info.adapters); // /settings
  if (Array.isArray(r.body?.items)) r.body.items = drop(r.body.items); // /adapters
  return r;
}

function firstDiff(a, b, path = '') {
  if (isDeepStrictEqual(a, b)) return null;
  if (a && b && typeof a === 'object' && typeof b === 'object' && Array.isArray(a) === Array.isArray(b)) {
    for (const k of new Set([...Object.keys(a), ...Object.keys(b)])) {
      const d = firstDiff(a[k], b[k], `${path}.${k}`);
      if (d) return d;
    }
  }
  const show = (v) => (v === undefined ? '<absent>' : JSON.stringify(v)?.slice(0, 300));
  return `${path || '.'}: ${show(a)} != ${show(b)}`;
}

async function runSide(app, impl, port, tmp, fakeK, only, seedImpl) {
  resetTmp(tmp);
  if (app === 'hub') {
    const env = hubEnv(tmp, port, { HUB_KNOWLEDGE_URL: `http://127.0.0.1:${fakeK.port}`, HUB_KNOWLEDGE_ADMIN_KEY: FAKE_KNOWLEDGE_KEY });
    seedWithPython(env, seedImpl);
    const s = await startServer(hubCommand(impl), env, port, '/api/v1/health');
    try {
      const vars = { HOME: env.HOME, PORT: port };
      return await runCases(HUB_CASES, port, { admin: readKey(tmp) }, vars, tmp, only, 'x-agent-hub');
    } finally { await stopServer(s); }
  }
  const cfgFile = writeKnowledgeConfig(tmp, port);
  const env = { PATH: '/usr/bin:/bin', HOME: join(tmp, 'home'), LANG: 'C.UTF-8', KNOWLEDGE_EMBED_API_KEY: 'parity-embed-key' };
  const s = await startServer(knowledgeCommand(impl, cfgFile), env, port, '/health');
  try {
    let admin = '';
    try { admin = readFileSync(join(tmp, 'kdata', 'admin.key'), 'utf8').trim(); } catch { /* not written by a stub */ }
    const vars = { HOME: env.HOME, PORT: port };
    return await runCases(KNOWLEDGE_CASES, port, { admin, token: undefined }, vars, tmp, only, 'x-agent-knowledge');
  } finally { await stopServer(s); }
}

function coverage(app, cases) {
  const routes = JSON.parse(readFileSync(join(PARITY, app === 'hub' ? 'routes.python.json' : 'routes.knowledge.json'), 'utf8')).routes;
  const uncovered = [];
  for (const r of routes) {
    const re = new RegExp(r.path_regex.replace(/\(\?P</g, '(?<'));
    const hit = cases.some((c) => r.methods.includes(c.m) && re.test(decodeURIComponent(c.p.replace(/\$\{\w+\}/g, 'x'))));
    if (!hit) uncovered.push(`${r.methods.join(',')} ${r.path}`);
  }
  return { routes: routes.length, uncovered };
}

export async function main(argv = process.argv.slice(2)) {
  const o = args(argv);
  const tmp = mkdtempSync(join(tmpdir(), 'agent-hub-parity-'));
  const fakeK = o.app === 'hub' ? await startFakeKnowledge() : null;
  try {
    const port = await freePort();
    const cases = o.app === 'hub' ? HUB_CASES : KNOWLEDGE_CASES;
    const cov = coverage(o.app, cases);
    // seed with the Python implementation being compared (side a first), else the last Python backend
    const seedImpl = [o.a, o.b].find((x) => x.startsWith('python')) ?? 'python';
    const A = await runSide(o.app, o.a, port, tmp, fakeK, o.only, seedImpl);
    const B = await runSide(o.app, o.b, port, tmp, fakeK, o.only, seedImpl);
    const byId = new Map(B.map((r) => [r.id, r]));
    const equal = []; const unported = []; const diffs = []; const allowed = [];
    const newAdapterGap = hasNewAdapter(A) !== hasNewAdapter(B);
    for (const ra of A) {
      const rb = byId.get(ra.id);
      const { id, ...ca } = newAdapterGap ? withoutNewAdapter(ra) : ra;
      const { id: _id, ...cb } = (newAdapterGap && rb ? withoutNewAdapter(rb) : rb) ?? {};
      const d = firstDiff(ca, cb);
      const gapAllow = newAdapterGap && id.startsWith('dsh.') ? { what: 'all', deviation: `${NEW_ADAPTER} is on one side only (A-15)` } : undefined;
      const allow = o.a !== o.b ? (ALLOWED[id] ?? gapAllow) : undefined;
      if (!d) equal.push(id);
      else if (rb?.status === 501 && rb?.body?.code === 'not_ported') unported.push({ id, detail: rb.body.detail });
      else if (allow?.what === 'all' || (allow?.what === 'headers' && !firstDiff({ ...ca, headers: null }, { ...cb, headers: null }))) {
        allowed.push({ id, deviation: allow.deviation, diff: d });
      } else diffs.push({ id, diff: d });
    }
    const recorded = o.app === 'hub' ? loadRustExpect() : {};
    for (const [side, recs] of o.a === o.b ? [[o.a, A]] : [[o.a, A], [o.b, B]]) {
      if (side !== 'rust') continue;
      for (const r of recs) {
        const want = ALLOWED[r.id]?.rust;
        const d = want && firstDiff({ status: want.status, body: want.body }, { status: r.status, body: r.body });
        if (d) diffs.push({ id: r.id, diff: `the Rust answer is not the expected one: ${d}` });
        if (!r.id.startsWith('dsh.') || o.writeRustExpect) continue;
        const { id: _rid, ...got } = untmp(r, tmp);
        if (!recorded || !(r.id in recorded)) {
          if (newAdapterGap) diffs.push({ id: r.id, diff: `no recorded Rust answer in ${RUST_EXPECT} (--write-rust-expect)` });
        } else {
          const dr = firstDiff(recorded[r.id], got);
          if (dr) diffs.push({ id: r.id, diff: `the Rust answer differs from the recorded one (parity/expect/rust-dsh.json): ${dr}` });
        }
      }
    }
    if (o.writeRustExpect) {
      const dsh = B.filter((r) => r.id.startsWith('dsh.'));
      const unproven = dsh.filter((r) => !equal.includes(r.id)).map((r) => r.id);
      if (o.b !== 'rust' || !o.a.startsWith('python') || newAdapterGap || !dsh.length || unproven.length || diffs.length) {
        throw new Error(`--write-rust-expect needs --b rust against a Python tree with ${NEW_ADAPTER}, every dsh.* case equal `
          + `and no difference (not equal: ${unproven.join(', ') || 'none'}; differences: ${diffs.length})`);
      }
      const cases = Object.fromEntries(dsh.map((r) => untmp(r, tmp)).map(({ id, ...rec }) => [id, rec]));
      mkdirSync(join(PARITY, 'expect'), { recursive: true });
      writeFileSync(RUST_EXPECT, `${JSON.stringify({
        _note: `Rust's answers to the dsh.* cases, recorded by run.mjs --write-rust-expect from a run in which a Python tree with the ${NEW_ADAPTER} adapter gave the same answers`,
        cases,
      }, null, 1)}\n`);
      console.log(`parity: recorded ${dsh.length} Rust answers in ${RUST_EXPECT}`);
    }
    const report = { app: o.app, a: o.a, b: o.b, cases: A.length, equal: equal.length, unported, allowed, diffs, coverage: cov, rules: RULES };
    if (o.json) writeFileSync(o.json, `${JSON.stringify(report, null, 1)}\n`);
    if (o.dump) writeFileSync(o.dump, `${JSON.stringify({ [o.a]: A, [`${o.b}${o.a === o.b ? '2' : ''}`]: B }, null, 1)}\n`);
    console.log(`parity ${o.app}: ${o.a} vs ${o.b}: ${A.length} cases, ${equal.length} equal, ${unported.length} not ported on ${o.b}, ${allowed.length} allowed deviations, ${diffs.length} differences; routes ${cov.routes}, uncovered ${cov.uncovered.length}`);
    for (const x of allowed) console.log(`  ALLOWED ${x.id} (${x.deviation}): ${x.diff}`);
    for (const u of cov.uncovered) console.log(`  UNCOVERED ${u}`);
    for (const d of diffs) console.log(`  DIFF ${d.id}: ${d.diff}`);
    return diffs.length || cov.uncovered.length ? 1 : 0;
  } catch (e) {
    console.error(`parity: harness failure: ${e.stack || e.message}`);
    return 2;
  } finally {
    fakeK?.server.close();
    rmSync(tmp, { recursive: true, force: true });
  }
}

if (import.meta.url === `file://${process.argv[1]}`) process.exit(await main());
