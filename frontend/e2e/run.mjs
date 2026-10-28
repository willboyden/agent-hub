#!/usr/bin/env node
// Browser end-to-end check of Agent Hub: the REAL backend, a real headless Chrome, the same auth mode as a real install
// (loopback NOT trusted; an admin key in sessionStorage, exactly where the UI keeps it after sign-in). Zero dependencies:
// Node 22's built-in WebSocket speaks the DevTools protocol (ADR-0008: no npm).
//
//   node frontend/e2e/run.mjs          (or `make e2e` from the repo root)
//
// Needs: the backend binaries (`make build`, i.e. `cargo build` in backend/: backend/target/debug/{agent-hub,hubctl}, or
// $E2E_RUST_BIN_DIR) and a Chrome: $CHROME_BIN (`make e2e` also reads `git config agent-hub.chrome`), else
// google-chrome / chromium / chrome-headless-shell on PATH. The hub is set up the way a person sets it up: `hubctl init`
// in a throwaway HOME and data dir, then the server with that state. A few items are created through the API (a skill,
// an agent, a collection and a Claude Code client whose project is a temp dir) so the detail pages and the plan have
// something to show. Every page of the nav, every detail route, and every tab is opened; the run FAILS on:
//   * the error state ("Something went wrong", class `state error`) or the router's not-found state,
//   * the sign-in dialog appearing (an API call was refused with the admin key), or any other dialog that opened by
//     itself (2026-10-26: leaving the editor opened its "Delete <item>?" confirmation, because the view's `remove()`
//     shadowed Element.remove() that the router calls),
//   * an uncaught exception, a console error, an API answer of 5xx, or a frontend file that does not load,
//   * a frontend file served without `Cache-Control: no-cache` (a stale module next to a new one breaks pages).
// The backend gets a minimal PATH (/usr/bin:/bin: git, nothing from ~/.local/bin), so a developer's installed tools
// (SkillEvaluator, the knowledge service) are as absent as they are in CI and on a fresh machine.
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, existsSync, mkdirSync, chmodSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const FRONTEND = dirname(dirname(fileURLToPath(import.meta.url)));
const BACKEND = join(FRONTEND, '..', 'backend');
// The Rust backend (0.3.0+): backend/target/debug by default, or E2E_RUST_BIN_DIR (e.g. backend/target/release).
const BIN_DIR = process.env.E2E_RUST_BIN_DIR || join(BACKEND, 'target', 'debug');
const SERVER = join(BIN_DIR, 'agent-hub');
const HUBCTL = join(BIN_DIR, 'hubctl');
const SETTLE_MS = 400;          // the page must stay free of skeletons for this long to count as rendered
const PAGE_TIMEOUT_MS = 20000;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Items the suite creates, and how each parameterised route is filled in. A new route with a parameter must be added
// here, or the run fails: an unvisited page is an untested page.
const SEED = { skill: 'e2e-skill', agent: 'e2e-agent', collection: 'e2e-core', client: 'e2e-claude' };
const DYNAMIC = {
  '/collections/:id': [`/collections/${SEED.collection}`],
  '/clients/:id': [`/clients/${SEED.client}`],
  '/edit/:kind/:name': [`/edit/skill/${SEED.skill}`, `/edit/agent/${SEED.agent}`, '/edit/skill/e2e-new?create=1'],
};

function findChrome() {
  if (process.env.CHROME_BIN) return process.env.CHROME_BIN;
  for (const dir of (process.env.PATH || '').split(':')) {
    for (const name of ['google-chrome', 'google-chrome-stable', 'chromium', 'chromium-browser', 'chrome-headless-shell']) {
      if (dir && existsSync(join(dir, name))) return join(dir, name);
    }
  }
  throw new Error('no Chrome found: set CHROME_BIN, or `git config agent-hub.chrome <path>`, or put google-chrome / chromium / chrome-headless-shell on PATH');
}

const freePort = () => new Promise((resolve, reject) => {
  const s = createServer();
  s.once('error', reject);
  s.listen(0, '127.0.0.1', () => { const { port } = s.address(); s.close(() => resolve(port)); });
});

/** Minimal DevTools client: one browser WebSocket, flat sessions. */
export class Cdp {
  constructor(ws) { this.ws = ws; this.id = 0; this.waiting = new Map(); this.handlers = []; }
  static async connect(url) {
    const ws = new WebSocket(url);
    await new Promise((res, rej) => { ws.onopen = res; ws.onerror = () => rej(new Error(`cannot connect to ${url}`)); });
    const c = new Cdp(ws);
    ws.onmessage = (m) => {
      const msg = JSON.parse(m.data);
      if (msg.id && c.waiting.has(msg.id)) {
        const { resolve, reject } = c.waiting.get(msg.id); c.waiting.delete(msg.id);
        msg.error ? reject(new Error(`${msg.error.message} (${msg.error.code})`)) : resolve(msg.result);
      } else if (msg.method) { for (const h of c.handlers) h(msg); }
    };
    return c;
  }
  send(method, params = {}, sessionId) {
    const id = ++this.id;
    this.ws.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
    return new Promise((resolve, reject) => this.waiting.set(id, { resolve, reject }));
  }
  on(h) { this.handlers.push(h); }
  close() { try { this.ws.close(); } catch { /* ignore */ } }
}

/** `hubctl init` + the server, in a throwaway HOME and data dir, loopback not trusted. */
export async function startBackend(tmp, { port } = {}) {
  if (!existsSync(SERVER) || !existsSync(HUBCTL)) throw new Error(`${SERVER} missing: run \`make build\` first`);
  port ||= await freePort();
  const home = join(tmp, 'home'); const data = join(tmp, 'data');
  for (const d of [home, data]) mkdirSync(d, { recursive: true });
  chmodSync(data, 0o700);
  const env = {
    PATH: '/usr/bin:/bin', HOME: home, LANG: 'C.UTF-8', AGENT_HUB_ENV_FILE: join(tmp, 'no-env-file'),
    HUB_PORT: String(port), HUB_DATA_DIR: data, HUB_TRUST_LOOPBACK: 'false',
  };
  const init = spawnSync(HUBCTL, ['init'], { cwd: BACKEND, env, encoding: 'utf8' });
  if (init.status !== 0) throw new Error(`hubctl init failed (${init.status}):\n${(init.stdout + init.stderr).slice(-2000)}`);
  const proc = spawn(SERVER, [], { cwd: BACKEND, env, stdio: ['ignore', 'pipe', 'pipe'] });
  let log = ''; proc.stdout.on('data', (d) => { log += d; }); proc.stderr.on('data', (d) => { log += d; });
  const base = `http://127.0.0.1:${port}`;
  let up = false;
  for (let i = 0; i < 150 && !up; i++) {
    if (proc.exitCode !== null) throw new Error(`backend exited early (${proc.exitCode}):\n${log.slice(-2000)}`);
    try { up = (await fetch(`${base}/api/v1/health`)).ok; } catch { /* not up yet */ }
    if (!up) await sleep(200);
  }
  if (!up) throw new Error(`backend never answered /api/v1/health:\n${log.slice(-2000)}`);
  const key = readFileSync(join(data, 'bootstrap-admin.key'), 'utf8').trim();
  return { proc, base, key, home, log: () => log };
}

/** Create what the detail routes and the plan need, through the API with the admin key (as the UI would). */
export async function seed(base, key, home) {
  const project = join(home, 'project'); mkdirSync(project, { recursive: true });
  const call = async (method, path, body) => {
    const r = await fetch(`${base}/api/v1${path}`, {
      method, body: JSON.stringify(body),
      headers: { Authorization: `Bearer ${key}`, 'X-Agent-Hub': '1', 'Content-Type': 'application/json' },
    });
    if (!r.ok) throw new Error(`seed ${method} ${path}: HTTP ${r.status} ${(await r.text()).slice(0, 300)}`);
  };
  await call('PUT', `/skills/${SEED.skill}`, { description: 'A skill created by the browser test.', body: '# e2e\n\nHello.\n', tags: ['e2e'] });
  await call('PUT', `/agents/${SEED.agent}`, { description: 'An agent created by the browser test.', capabilities: ['read'] });
  await call('POST', '/collections', { id: SEED.collection, title: 'E2E core', members: [{ kind: 'skill', name: SEED.skill }] });
  await call('POST', '/clients', { id: SEED.client, adapter: 'claude-code', display_name: 'E2E Claude Code', roots: { project } });
  return call;
}

/** True when the knowledge service is not running and the hub answers its proxy with the designed 503 problem. */
async function knowledgeDown(base, key) {
  const r = await fetch(`${base}/api/v1/knowledge/namespaces`, { headers: { Authorization: `Bearer ${key}` } });
  if (r.status !== 503) return false;
  try { return (await r.json()).code === 'knowledge_unavailable'; } catch { return false; }
}

export async function startChrome(profile) {
  const port = await freePort();
  const proc = spawn(findChrome(), [
    '--headless', `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, '--no-sandbox', '--disable-gpu',
    '--disable-dev-shm-usage', '--disable-background-networking', '--disable-sync', '--no-first-run',
    '--no-default-browser-check', '--window-size=1440,1000', 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'] });
  let err = ''; proc.stderr.on('data', (d) => { err += d; });
  for (let i = 0; i < 100; i++) {
    try {
      const v = await (await fetch(`http://127.0.0.1:${port}/json/version`)).json();
      if (v.webSocketDebuggerUrl) return { proc, wsUrl: v.webSocketDebuggerUrl, version: v.Browser };
    } catch { /* not listening yet */ }
    await sleep(150);
  }
  proc.kill('SIGKILL');
  throw new Error(`Chrome never exposed DevTools:\n${err.slice(-1500)}`);
}

export async function main(argv = process.argv.slice(2)) {
  const keepProfile = argv.includes('--keep-profile');
  const tmp = mkdtempSync(join(tmpdir(), 'ah-e2e-'));
  const failures = [];
  let backend; let chrome; let cdp;
  try {
    backend = await startBackend(tmp);
    const call = await seed(backend.base, backend.key, backend.home);
    // The knowledge service is a separate, optional process that the suite does not start: its proxy then answers 503
    // `knowledge_unavailable` by design (the page shows its "not running" panel). Any other 5xx still fails the run.
    const kDown = await knowledgeDown(backend.base, backend.key);
    chrome = await startChrome(join(tmp, 'profile'));
    cdp = await Cdp.connect(chrome.wsUrl);
    const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' });
    const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
    const s = (m, p) => cdp.send(m, p, sessionId);
    await Promise.all([s('Page.enable'), s('Runtime.enable'), s('Log.enable'), s('Network.enable')]);

    let route = '(sign-in)';
    // Before the key is in sessionStorage the app's first calls answer 401 by design: not a failure.
    const problems = (msg) => { if (route !== '(sign-in)') failures.push(`${route}: ${msg}`); };
    const seenHeaders = new Map();
    cdp.on((m) => {
      if (m.sessionId !== sessionId) return;
      if (m.method === 'Runtime.exceptionThrown') {
        const d = m.params.exceptionDetails; problems(`uncaught exception: ${d.exception?.description || d.text}`);
      } else if (m.method === 'Runtime.consoleAPICalled' && m.params.type === 'error') {
        problems(`console.error: ${m.params.args.map((a) => a.value ?? a.description ?? '').join(' ').slice(0, 300)}`);
      } else if (m.method === 'Log.entryAdded' && m.params.entry.level === 'error' && m.params.entry.source !== 'network') {
        problems(`browser error: ${m.params.entry.text.slice(0, 300)} ${m.params.entry.url || ''}`);    // network: below
      } else if (m.method === 'Network.responseReceived') {
        const r = m.params.response; const u = new URL(r.url);
        if (u.origin !== backend.base) return;
        if (u.pathname.startsWith('/api/')) {
          const designed = kDown && r.status === 503 && u.pathname.startsWith('/api/v1/knowledge/');
          if (r.status >= 500 && !designed) problems(`HTTP ${r.status} from ${u.pathname}`);
          return;
        }
        if (r.status >= 400) problems(`frontend file ${u.pathname} answered ${r.status}`);
        seenHeaders.set(u.pathname, Object.entries(r.headers).find(([k]) => k.toLowerCase() === 'cache-control')?.[1] || '');
      }
    });

    const evaluate = async (expr) => (await s('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise: true })).result.value;
    // Sign in the way the UI's form does: the key goes into sessionStorage, then the app boots with it.
    await s('Page.navigate', { url: `${backend.base}/` });
    await sleep(500);
    await evaluate(`sessionStorage.setItem('ah.apiKey', ${JSON.stringify(backend.key)}); true`);
    await s('Page.reload', { ignoreCache: false });
    await sleep(800);

    /** Wait until the page has rendered (no skeleton for SETTLE_MS) and report the visible error states. */
    const settle = async () => {
      const t0 = Date.now(); let quietSince = 0;
      while (Date.now() - t0 < PAGE_TIMEOUT_MS) {
        const st = await evaluate(`(() => {
          const main = document.querySelector('#main');
          const busy = !!document.querySelector('#main .skeleton, #main [aria-busy="true"]');
          const errs = [...document.querySelectorAll('#main .state.error')].filter((e) => e.offsetParent !== null)
            .map((e) => e.innerText.replace(/\\s+/g, ' ').slice(0, 240));
          const notFound = !!document.querySelector('#main > .state.empty');   // the router's own not-found state
          const signIn = !!document.querySelector('dialog[open] #key-in');     // a 401 with the admin key
          const dialogs = [...document.querySelectorAll('dialog[open]')].filter((d) => !d.querySelector('#key-in'))
            .map((d) => d.innerText.replace(/\s+/g, ' ').slice(0, 160));
          return { busy, errs, notFound, signIn, dialogs, hasContent: !!main && main.children.length > 0 };
        })()`);
        if (st.errs.length || st.notFound || st.signIn || st.dialogs.length) return st;
        if (!st.busy && st.hasContent) { quietSince ||= Date.now(); if (Date.now() - quietSince >= SETTLE_MS) return st; }
        else quietSince = 0;
        await sleep(100);
      }
      return { timeout: true, errs: [], notFound: false, signIn: false, dialogs: [] };
    };
    const judge = (st) => {
      if (st.timeout) problems('did not finish rendering');
      if (st.notFound) problems('shows the not-found state');
      if (st.signIn) problems('asked for a key although the admin key was set');
      for (const e of st.errs) problems(`error state: ${e}`);
      for (const d of st.dialogs) problems(`a dialog opened by itself: ${d}`);
    };
    // Close what judge() reported, so one stray dialog does not cascade into every later page.
    const closeDialogs = () => evaluate(`document.querySelectorAll('dialog[open]').forEach((d) => d.close()); true`);

    const patterns = [...readFileSync(join(FRONTEND, 'js', 'app.js'), 'utf8').matchAll(/\{ path: '(\/[^']*)'/g)].map((m) => m[1]);
    if (patterns.length < 10) throw new Error(`only ${patterns.length} routes parsed from js/app.js`);
    const paths = [];
    for (const p of patterns) {
      if (!p.includes(':')) { paths.push(p); continue; }
      if (!DYNAMIC[p]) throw new Error(`route ${p} has a parameter the suite does not know how to fill: add it to DYNAMIC in e2e/run.mjs`);
      paths.push(...DYNAMIC[p]);
    }
    let pages = 0;
    const visit = async (list, label) => {
      for (const path of list) {
        route = `${path} [${label}]`;
        await evaluate(`location.hash = ${JSON.stringify('#' + path)}; true`);
        await sleep(150);
        let st = await settle(); pages++;
        judge(st); await closeDialogs();
        const tabs = await evaluate(`[...document.querySelectorAll('#main [role="tab"]')].map((b) => b.id).filter(Boolean)`);
        for (const id of tabs || []) {
          route = `${path} › ${id} [${label}]`;
          await evaluate(`document.getElementById(${JSON.stringify(id)})?.click(); true`);
          await sleep(150);
          st = await settle(); pages++;
          judge(st); await closeDialogs();
        }
      }
    };
    await visit(paths, 'uncommitted');
    // Second pass with the seeded content committed: the plan, the client page and the editor's Rendered tab now
    // render real files instead of the "not committed yet" state.
    route = '(commit)';
    await call('POST', '/changes/commit', { message: 'e2e: seed' });
    await visit(paths.filter((p) => /^\/(changes|matrix|clients|edit\/(skill|agent)\/e2e-(skill|agent))/.test(p)), 'committed');
    route = '(static files)';
    for (const [p, cc] of seenHeaders) if (!/\bno-cache\b/.test(cc)) problems(`${p} served without Cache-Control: no-cache (got '${cc}')`);
    if (!seenHeaders.size) problems('no frontend file was observed (the page never loaded?)');

    console.log(`e2e: ${chrome.version}; ${patterns.length} routes (${paths.length} paths), ${pages} pages/tabs rendered in two passes, ${seenHeaders.size} static files checked${kDown ? '; knowledge service not running (its 503s are expected)' : ''}`);
  } catch (e) {
    failures.push(`harness: ${e.message}`);
  } finally {
    cdp?.close();
    chrome?.proc.kill('SIGKILL');
    if (backend) {
      backend.proc.kill('SIGTERM');
      for (let i = 0; i < 50 && backend.proc.exitCode === null; i++) await sleep(100);
      if (backend.proc.exitCode === null) backend.proc.kill('SIGKILL');
    }
    if (!keepProfile) rmSync(tmp, { recursive: true, force: true });
  }
  if (failures.length) {
    console.error(`e2e: FAILED (${failures.length})`);
    for (const f of failures) console.error(`  - ${f}`);
    if (backend) console.error(`backend log tail:\n${backend.log().slice(-1500)}`);
    return 1;
  }
  console.log('e2e: all pages rendered with no error state, no console error, and revalidating static files');
  return 0;
}

if (import.meta.url === `file://${process.argv[1]}`) process.exit(await main());
