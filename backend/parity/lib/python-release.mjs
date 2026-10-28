// The Python implementation for the parity harness after the cutover: the Python backend and knowledge service of a
// git ref (default: the last commit that still had them, the code the Rust port was verified against; `v0.2.0` is the
// last Python release tag). The ref's backend/, knowledge/, policy/ and deploy/ trees are exported with `git archive`
// (no worktree is created) into backend/target/python-release/<commit>/ and get their venvs with `uv sync --frozen`
// (Python 3.13; uv's cache, or the network the first time). Nothing outside backend/target is written.
//
//   node backend/parity/lib/python-release.mjs [ref]      # prepare (once) and print the release root
//   PY=$(node backend/parity/lib/python-release.mjs)/backend/.venv/bin/python   # e.g. for the golden generators
//
// A ref may also be an absolute path to a Python tree already on disk (backend/pyproject.toml with its .venv, e.g. an
// export plus uncommitted files under a scratch dir): it is used as it is, never synced or written to.
import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, isAbsolute, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const PARITY = dirname(dirname(fileURLToPath(import.meta.url)));
const REPO = join(PARITY, '..', '..');

/** The last commit with the Python backend and knowledge service (the port's reference; PORTING.md A-13). */
export const LAST_PYTHON_REF = 'c419f1edc6c5fc3aeddc23630a1864bd41c2c25f';

function run(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { encoding: 'buffer', maxBuffer: 512 * 1024 * 1024, ...opts });
  if (r.error || r.status !== 0) {
    const out = `${r.stdout ?? ''}${r.stderr ?? ''}`.slice(-2000);
    throw new Error(`${cmd} ${args.join(' ')} failed (${r.error?.message ?? r.status}): ${out}`);
  }
  return r.stdout;
}

/** `python` or `python@<ref>` -> the git ref (or an absolute tree path). */
export function refOf(impl) {
  if (impl === 'python') return LAST_PYTHON_REF;
  if (impl.startsWith('python@') && impl.length > 7) return impl.slice(7);
  return null;
}

/** Prepare (once) the Python release of `ref`; returns { root, backend, knowledge } (absolute paths). */
export function pythonRelease(ref = LAST_PYTHON_REF) {
  if (isAbsolute(ref) && existsSync(join(ref, 'backend', 'pyproject.toml'))) {
    const out = { root: ref, backend: join(ref, 'backend'), knowledge: join(ref, 'knowledge') };
    if (!existsSync(join(out.backend, '.venv', 'bin', 'python'))) {
      throw new Error(`${ref}: backend/.venv is missing (run \`uv sync --frozen\` in its backend/ first)`);
    }
    return out;
  }
  const sha = run('git', ['-C', REPO, 'rev-parse', '--verify', '--quiet', `${ref}^{commit}`]).toString().trim();
  const root = join(REPO, 'backend', 'target', 'python-release', sha);
  const out = { root, backend: join(root, 'backend'), knowledge: join(root, 'knowledge') };
  if (existsSync(join(root, '.ready'))) return out;
  rmSync(root, { recursive: true, force: true });
  mkdirSync(root, { recursive: true });
  const tar = run('git', ['-C', REPO, 'archive', '--format=tar', sha, '--', 'backend', 'knowledge', 'policy', 'deploy']);
  run('tar', ['-x', '-C', root], { input: tar });
  for (const pkg of [out.backend, out.knowledge]) {
    if (!existsSync(join(pkg, 'pyproject.toml'))) throw new Error(`${ref} has no Python package at ${pkg}`);
    run('uv', ['sync', '--frozen', '--python', '3.13'], { cwd: pkg, env: { UV_PYTHON_DOWNLOADS: 'never', ...process.env } });
  }
  writeFileSync(join(root, '.ready'), `${ref}\n`);
  return out;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  try {
    console.log(pythonRelease(process.argv[2] || LAST_PYTHON_REF).root);
  } catch (e) {
    console.error(`python-release: ${e.message}`);
    process.exit(1);
  }
}
