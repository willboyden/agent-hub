// Start one implementation of the hub or the knowledge service in a hermetic temp setup and stop it again.
// Every process gets HOME, the data dir and the env file pointed into the temp dir, a PATH of the fake tools plus
// /usr/bin:/bin (real git), and an ephemeral port. Only processes started here are ever signalled.
// Implementations: `rust` (backend/target/debug, or PARITY_RUST_BIN_DIR) and `python` / `python@<ref>` (the Python
// backend and knowledge service of a git ref, prepared by lib/python-release.mjs). Both sides use THIS checkout's
// frontend/ and policy/floor.yaml (HUB_FRONTEND_DIR, HUB_POLICY_FILE), so an older ref's copies cannot skew the
// comparison.
import { spawn, spawnSync } from 'node:child_process';
import { chmodSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { LAST_PYTHON_REF, pythonRelease, refOf } from './python-release.mjs';

export const PARITY = dirname(dirname(fileURLToPath(import.meta.url)));
export const REPO = join(PARITY, '..', '..');
const RUST_BIN = process.env.PARITY_RUST_BIN_DIR || join(REPO, 'backend', 'target', 'debug');

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export const freePort = () => new Promise((resolve, reject) => {
  const s = createServer();
  s.once('error', reject);
  s.listen(0, '127.0.0.1', () => { const { port } = s.address(); s.close(() => resolve(port)); });
});

export function hubEnv(tmp, port, extra = {}) {
  return {
    PATH: `${join(PARITY, 'fake-bin')}:/usr/bin:/bin`,
    HOME: join(tmp, 'home'), LANG: 'C.UTF-8', AGENT_HUB_ENV_FILE: join(tmp, 'no-env-file'),
    HUB_PORT: String(port), HUB_DATA_DIR: join(tmp, 'data'), HUB_TRUST_LOOPBACK: 'false',
    HUB_FRONTEND_DIR: join(REPO, 'frontend'), HUB_POLICY_FILE: join(REPO, 'policy', 'floor.yaml'),
    ...extra,
  };
}

/** Fresh temp layout: home/ (with project dirs and the DSH stage dirs for client roots: the hub never creates a
 *  root), data/ (0700). */
export function resetTmp(tmp) {
  rmSync(tmp, { recursive: true, force: true });
  for (const d of ['home', 'home/project', 'home/project2', 'home/dsh-stage', 'home/dsh-stage2', 'data']) {
    mkdirSync(join(tmp, d), { recursive: true });
  }
  chmodSync(join(tmp, 'data'), 0o700);
}

/** `hubctl init` with a PYTHON implementation (`seedImpl`, default the last Python backend): both sides start from
 *  the same Python-seeded state, so the Rust server is also proven to open what Python wrote. */
export function seedWithPython(env, seedImpl = 'python') {
  const rel = pythonRelease(refOf(seedImpl) ?? LAST_PYTHON_REF);
  const r = spawnSync(join(rel.backend, '.venv', 'bin', 'hubctl'), ['init'], { cwd: rel.backend, env, encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`python hubctl init failed (${r.status}): ${(r.stdout + r.stderr).slice(-1500)}`);
}

export function hubCommand(impl) {
  const ref = refOf(impl);
  if (ref) {
    const rel = pythonRelease(ref);
    return { cmd: join(rel.backend, '.venv', 'bin', 'python'), args: ['-I', '-m', 'agent_hub.main'], cwd: rel.backend };
  }
  if (impl === 'rust') return { cmd: join(RUST_BIN, 'agent-hub'), args: [], cwd: REPO };
  throw new Error(`unknown implementation ${impl} (rust, python or python@<ref>)`);
}

export function knowledgeCommand(impl, configFile) {
  const ref = refOf(impl);
  if (ref) {
    const rel = pythonRelease(ref);
    return { cmd: join(rel.knowledge, '.venv', 'bin', 'python'), args: ['-I', '-m', 'agent_knowledge.main', '--config', configFile], cwd: rel.knowledge };
  }
  if (impl === 'rust') return { cmd: join(RUST_BIN, 'agent-knowledge'), args: ['--config', configFile], cwd: REPO };
  throw new Error(`unknown implementation ${impl} (rust, python or python@<ref>)`);
}

/** Start a server and wait until it answers any HTTP request on `probePath`. */
export async function startServer({ cmd, args, cwd }, env, port, probePath, host = '127.0.0.1') {
  if (!existsSync(cmd)) throw new Error(`${cmd} missing (build or set up the implementation first)`);
  const proc = spawn(cmd, args, { cwd, env, stdio: ['ignore', 'pipe', 'pipe'] });
  let log = '';
  proc.stdout.on('data', (d) => { log += d; });
  proc.stderr.on('data', (d) => { log += d; });
  for (let i = 0; i < 200; i++) {
    if (proc.exitCode !== null) throw new Error(`${cmd} exited early (${proc.exitCode}):\n${log.slice(-2000)}`);
    try {
      await fetch(`http://${host}:${port}${probePath}`, { headers: { host: `127.0.0.1:${port}` } });
      return { proc, log: () => log };
    } catch { await sleep(100); }
  }
  proc.kill('SIGKILL');
  throw new Error(`${cmd} never answered on port ${port}:\n${log.slice(-2000)}`);
}

export async function stopServer(s) {
  if (!s) return;
  s.proc.kill('SIGTERM');
  for (let i = 0; i < 50 && s.proc.exitCode === null; i++) await sleep(100);
  if (s.proc.exitCode === null) s.proc.kill('SIGKILL');
}

export const readKey = (tmp) => readFileSync(join(tmp, 'data', 'bootstrap-admin.key'), 'utf8').trim();

export function writeKnowledgeConfig(tmp, port) {
  const f = join(tmp, 'knowledge.yaml');
  writeFileSync(f, `host: 127.0.0.1\nport: ${port}\ndata_dir: ${join(tmp, 'kdata')}\nrouter_url: http://127.0.0.1:9\n`);
  return f;
}
