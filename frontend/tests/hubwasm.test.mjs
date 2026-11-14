// hubwasm.js: hub-core's checks in the browser (docs/adr/0013), run here on the module `make web` built.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { hubWasm, wasmStatus, wrapModule } from '../js/hubwasm.js';
import { createClient } from '../js/api.js';
import { SKIP_WASM, loadWasm } from './wasm-helper.mjs';

const cargo = readFileSync(new URL('../../backend/Cargo.toml', import.meta.url), 'utf8');
const WORKSPACE_VERSION = /\[workspace\.package\][^[]*?version = "([^"]+)"/.exec(cargo)[1];

test('the module is built from this checkout (its version is the workspace version)', { skip: SKIP_WASM }, async () => {
  const w = await loadWasm();
  assert.equal(w.version, WORKSPACE_VERSION);
  assert.equal(wasmStatus().state, 'ready');
});

test(
  'a hub env draft is checked in full in the tab, secret values included, and no value is echoed',
  { skip: SKIP_WASM },
  async () => {
    const w = await loadWasm();
    const r = w.checkConfig(
      'hub-env',
      'HUB_KNOWLEDGE_ADMIN_KEY=hunter2-value\nHUB_KNOWLEDGE_TOKEN=\nHUB_PORT=not-a-port\n'
    );
    assert.equal(r.file, 'hub-env');
    assert.equal(r.ok, false);
    assert.ok(!JSON.stringify(r).includes('hunter2'), JSON.stringify(r));
    const msgs = r.diagnostics.map((d) => `${d.severity} ${d.line} ${d.message}`);
    assert.ok(
      msgs.some((m) => /^info 1 HUB_KNOWLEDGE_ADMIN_KEY is a secret/.test(m)),
      msgs.join('\n')
    );
    assert.ok(
      msgs.some((m) => /^error 3 HUB_PORT: Input should be a valid integer/.test(m)),
      msgs.join('\n')
    );
    const hosts = w.checkConfig('hub-env', 'HUB_API_KEY=hunter3\nHUB_ALLOWED_HOSTS=nope\n');
    assert.match(hosts.diagnostics.at(-1).message, /^HUB_ALLOWED_HOSTS must be a JSON list/);
    assert.equal(hosts.diagnostics.at(-1).line, 2);
    assert.ok(!JSON.stringify(hosts).includes('hunter3'));
    // the shipped template passes, as on the server
    const tpl = readFileSync(new URL('../../deploy/agent-hub.env.example', import.meta.url), 'utf8');
    assert.equal(w.checkConfig('hub-env', tpl).ok, true);
    // a refused draft is the route's problem body
    const big = w.checkConfig('hub-env', '#'.repeat(64 * 1024 + 1));
    assert.deepEqual([big.status, big.code], [413, 'payload_too_large']);
    assert.equal(w.checkConfig('floor', 'x: 1').code, 'floor_locked');
  }
);

test(
  'knowledge drafts: the service validator; anchors are "not checked", never a pass',
  { skip: SKIP_WASM },
  async () => {
    const w = await loadWasm();
    const bad = w.checkConfig('knowledge', 'port: x\nzzz: 1\n');
    assert.equal(bad.ok, false);
    assert.deepEqual(
      bad.diagnostics.map((d) => d.path),
      [['port'], ['zzz']]
    );
    const anchors = w.checkConfig('knowledge', 'port: &p 8795\nother: *p\n');
    assert.equal(anchors.ok, false);
    assert.equal(anchors.summary.checked, false);
    for (const t of ['host', 'container']) {
      const f = t === 'host' ? 'config.example.yaml' : 'container-config.yaml';
      const text = readFileSync(new URL(`../../deploy/knowledge/${f}`, import.meta.url), 'utf8');
      assert.equal(w.checkConfig('knowledge', text).ok, true, t);
    }
  }
);

test(
  'client files: the local half, and the flag that the hub still has checks to add',
  { skip: SKIP_WASM },
  async () => {
    const w = await loadWasm();
    const text = readFileSync(new URL('../../examples/clients/cursor.yaml', import.meta.url), 'utf8');
    const r = w.checkConfig('client-spec', text);
    assert.equal(r.file, 'client-spec');
    assert.equal(r.server_check, true);
    const bad = w.checkConfig('client-spec', 'id: hub\nadapter: generic\n');
    assert.equal(bad.ok, false);
  }
);

test("scalar types are PyYAML's (what the Form view writes back must read the same)", { skip: SKIP_WASM }, async () => {
  const w = await loadWasm();
  for (const [s, t] of [
    ['yes', 'bool'],
    ['null', 'null'],
    ['0x1F', 'int'],
    ['1_000', 'int'],
    ['1.5', 'float'],
    ['1e3', 'str'],
    ['2026-11-04', 'timestamp'],
    ['text', 'str'],
  ])
    assert.equal(w.yamlScalarType(s), t, s);
});

test(
  'a trap drops the instance: calls answer null, the next hubWasm() loads a fresh one',
  { skip: SKIP_WASM },
  async () => {
    await loadWasm();
    const fake = wrapModule({
      version: () => 'x',
      check_config: () => {
        throw new WebAssembly.RuntimeError('unreachable');
      },
    });
    assert.equal(fake.checkConfig('hub-env', 'A=1'), null);
    assert.equal(fake.alive, false);
    assert.equal(wasmStatus().state, 'trapped');
    const fresh = await hubWasm();
    assert.ok(fresh?.alive);
    assert.equal(fresh.checkConfig('hub-env', 'HUB_PORT=1\n').ok, true);
  }
);

test('the API client never sends a hub env draft (no request is made)', async () => {
  const calls = [];
  const api = createClient({
    fetchImpl: async (...a) => {
      calls.push(a);
      return new Response('{}', { status: 200, headers: { 'content-type': 'application/json' } });
    },
  });
  await assert.rejects(api.validateConfig('hub-env', 'HUB_KNOWLEDGE_ADMIN_KEY=secret\n'), (e) => {
    assert.equal(e.code, 'env_checked_in_browser');
    return true;
  });
  assert.equal(calls.length, 0);
  await api.validateConfig('knowledge', 'port: 1\n');
  assert.equal(calls.length, 1);
});

// The Form view types plain scalars synchronously with cfg-parse.js yamlPlainType (it must work before and without the
// module). This keeps that copy honest: on every input below it must agree with hub-core's resolver (PyYAML's).
test("cfg-parse's yamlPlainType agrees with hub-core's YAML resolver", { skip: SKIP_WASM }, async () => {
  const w = await loadWasm();
  const { yamlPlainType } = await import('../js/cfg-parse.js');
  const js = { string: 'str', date: 'timestamp', null: 'null', bool: 'bool', int: 'int', float: 'float' };
  const corpus = [
    ...['', '~', 'null', 'Null', 'NULL', 'nULL', 'yes', 'Yes', 'YES', 'yEs', 'no', 'on', 'OFF', 'true', 'False', 'y', 'n'],
    ...['0', '-0', '+1', '007', '0o7', '08', '0b101', '0b2', '0x1F', '0xg', '1_000', '1:30', '-1:30:00', '190:20:30'],
    ...['1.5', '-1.5', '.5', '-.5', '1.', '1e3', '1.0e3', '1.0e+3', '1.0E-3', '.inf', '-.Inf', '+.INF', '.nan', '.NaN'],
    ...['1:30.5', '1_0.5', '2026-11-04', '2026-1-4', '2026-11-04T10:00:00Z', '2026-11-04 10:00:00.5 +02:00', '2026-11'],
    ...['text', 'hello world', '12abc', '0.0.0.0', '127.0.0.1', 'v1.2.3', '1,000', '$HOME', '=', '<<'],
  ];
  const diff = corpus
    .map((s) => [s, js[yamlPlainType(s).type], w.yamlScalarType(s)])
    .filter(([s, a, b]) => a !== b && !(s === '=' || s === '<<'));
  assert.deepEqual(diff, []);
});
