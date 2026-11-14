// cfg-edit.js: Form edits write a value of the field's own type, in a quoting style that the real loader reads back
// as exactly what was typed, and change nothing else in the file.
import test from 'node:test';
import assert from 'node:assert/strict';
import { parseConfig, entryOf, isSecretEnvKey } from '../js/cfg-parse.js';
import {
  encodeValue,
  spliceValue,
  spliceBlock,
  displayValue,
  flattenFields,
  yamlPlainSafe,
  pathKey,
  shq,
  shPath,
  uncheckedResult,
} from '../js/cfg-edit.js';

const nodeAt = (fmt, text, path) =>
  path.reduce((n, k) => (typeof k === 'number' ? n.items[k] : entryOf(n, k).node), parseConfig(fmt, text).root);
/** Edit one value and read the file back with the same parser: the new value, and every other value untouched. */
function edit(fmt, text, path, input, spec) {
  const n = nodeAt(fmt, text, path);
  const enc = encodeValue(fmt, n, input, spec);
  if (enc.error) return { error: enc.error };
  const out = enc.block !== undefined ? spliceBlock(text, n, enc.block) : spliceValue(text, n, enc.raw);
  const before = flattenFields(parseConfig(fmt, text).root),
    after = flattenFields(parseConfig(fmt, out).root);
  assert.equal(after.length, before.length);
  for (const [i, f] of after.entries())
    if (pathKey(f.path) !== pathKey(path))
      assert.equal(f.node.raw, before[i].node.raw, `other value changed: ${pathKey(f.path)}`);
  return { out, node: nodeAt(fmt, out, path) };
}

test('YAML strings: plain when safe, quoted when PyYAML would read something else', () => {
  const T = 'a: plain  # keep me\nb: \'single\'\nc: "double"\n';
  assert.equal(edit('yaml', T, ['a'], 'other').out, 'a: other  # keep me\nb: \'single\'\nc: "double"\n');
  for (const tricky of [
    'yes',
    'null',
    '123',
    '1e3',
    'a: b',
    'x #y',
    '#lead',
    ' lead',
    '- dash',
    '@at',
    '',
    '2026-11-04',
    '[x]',
    'tab\tin',
  ]) {
    const r = edit('yaml', T, ['a'], tricky);
    assert.equal(r.node.type, 'string', tricky);
    assert.equal(r.node.value, tricky, tricky);
  }
  assert.equal(edit('yaml', T, ['b'], "it's").out.split('\n')[1], "b: 'it''s'");
  assert.equal(edit('yaml', T, ['c'], 'x"y').out.split('\n')[2], 'c: "x\\"y"');
  assert.equal(edit('yaml', 'k: [a, b]\n', ['k', 1], 'c, d').node.value, 'c, d'); // inside [ ]: quoted
  assert.ok(yamlPlainSafe('router/embed-small'));
  assert.ok(!yamlPlainSafe('on'));
  assert.ok(!yamlPlainSafe('x: y'));
});

test('numbers keep their kind; a value of the wrong kind is refused, not written', () => {
  const T = 'i: 4\nf: 0.5\n';
  assert.equal(edit('yaml', T, ['i'], '64').node.value, 64);
  assert.match(edit('yaml', T, ['i'], 'abc').error, /'abc' is not a whole number/);
  assert.match(edit('yaml', T, ['i'], '2.0').error, /without a decimal point/);
  assert.match(edit('yaml', T, ['i'], '0777').error, /not a whole number/); // would be octal
  assert.equal(edit('yaml', T, ['f'], '1').out, 'i: 4\nf: 1.0\n'); // stays a float
  assert.equal(edit('yaml', T, ['f'], '1e-3').node.value, 0.001); // rewritten so PyYAML reads a number
  assert.equal(edit('yaml', T, ['f'], '2e3').node.type, 'float');
  assert.equal(edit('yaml', T, ['i'], '0.25', { t: 'num' }).node.value, 0.25); // the schema allows any number
});

test('booleans keep their spelling; empty values', () => {
  assert.equal(edit('yaml', 'a: yes\nb: True\nc: ON\n', ['a'], false).out.split('\n')[0], 'a: no');
  assert.equal(edit('yaml', 'a: yes\nb: True\nc: ON\n', ['b'], false).out.split('\n')[1], 'b: False');
  assert.equal(edit('yaml', 'a: yes\nb: True\nc: ON\n', ['c'], false).out.split('\n')[2], 'c: OFF');
  assert.match(encodeValue('yaml', nodeAt('yaml', 'n: 1\n', ['n']), true).error, /not a true\/false field/);
  assert.equal(edit('yaml', 'e:\nz: 1\n', ['e'], 'text').out, 'e: text\nz: 1\n'); // the space is added
  assert.equal(edit('yaml', 'e:\nz: 1\n', ['e'], '').out, 'e:\nz: 1\n'); // still empty
  assert.equal(edit('yaml', 'e: ~\n', ['e'], '5', { t: 'int' }).node.value, 5);
});

test('YAML block scalars: the lines are re-indented under the key, the header is kept', () => {
  const T = 'prompt: |\n    Hello\n    world\nnext: 1\n';
  const n = nodeAt('yaml', T, ['prompt']);
  assert.equal(displayValue(n), 'Hello\nworld');
  const r = edit('yaml', T, ['prompt'], 'Hi\n\nthere');
  assert.equal(r.out, 'prompt: |\n    Hi\n\n    there\nnext: 1\n');
  assert.equal(r.node.value, 'Hi\n\nthere\n');
});

test('flattenFields lists every scalar with its path, in document order', () => {
  const f = flattenFields(parseConfig('yaml', 'a: 1\nb:\n  - x\n  - k: v\n').root);
  assert.deepEqual(
    f.map((x) => x.path),
    [['a'], ['b', 0], ['b', 1, 'k']]
  );
});

test('0.9.0 audit: values PyYAML would read differently are escaped, refused or normalised', () => {
  const q = edit('yaml', 'a: "x"\n', ['a'], 'a\u0085b\u2028c\u2029d');
  assert.equal(q.node.value, 'a\u0085b\u2028c\u2029d'); // escaped, not folded to spaces
  assert.match(q.out, /\\u0085/);
  assert.equal(edit('yaml', "a: 'x'\n", ['a'], 'p\u2028q').node.value, 'p\u2028q'); // single quotes cannot carry it
  assert.equal(edit('yaml', 'a: x\n', ['a'], '=').out, 'a: "="\n'); // a bare = is the YAML value tag
  assert.equal(edit('yaml', 'a: 0.5\n', ['a'], '-.5').node.value, -0.5); // -.5 is text to PyYAML
  assert.equal(edit('yaml', 'a: x\n', ['a'], '<<').out, 'a: "<<"\n'); // the merge key tag
  assert.match(edit('yaml', 'a: 0.5\n', ['a'], '-.').error, /not a number/); // a half-typed number is not -0.
  assert.match(edit('yaml', 'p: |\n  x\nz: 1\n', ['p'], '  indented\nnot').error, /first line/);
  assert.equal(edit('yaml', 'p: |\n  x\nz: 1\n', ['p'], '\nfirst\n  second').node.value, '\nfirst\n  second\n');
});

// ---- env files (the hub's own env file; added in Agent Hub) ----
test('parseEnv reads bindings the way python-dotenv does and keeps every span exact', () => {
  const text =
    '# header\nHUB_PORT=9000 # why\nexport HUB_HOST="127.0.0.1"\nHUB_X=\'a\\\\b\'\nHUB_E=\nnot a binding\nHUB_PORT=9001\n';
  const r = parseConfig('env', text);
  assert.equal(r.unsupported, null);
  assert.deepEqual(
    r.root.entries.map((e) => [e.key, e.node.value, e.node.style, e.line]),
    [
      ['HUB_PORT', '9000', 'plain', 2],
      ['HUB_HOST', '127.0.0.1', 'double', 3],
      ['HUB_X', 'a\\b', 'single', 4],
      ['HUB_E', '', 'empty', 5],
      ['HUB_PORT', '9001', 'plain', 7],
    ]
  );
  for (const e of r.root.entries) assert.equal(text.slice(e.node.start, e.node.end), e.node.raw);
  assert.equal(entryOf(r.root, 'HUB_PORT').node.value, '9001', 'a later duplicate wins');
  assert.match(parseConfig('env', 'A="open\nB=1\n').unsupported, /several lines/);
});

test('parseEnv stays linear on a long run of spaces (no regex backtracking)', () => {
  for (const line of [
    `A=${' '.repeat(65536)}x`,
    `A=x${' '.repeat(65536)}`,
    `A=x${' \t'.repeat(32768)}y #c`,
    `A=${' '.repeat(65536)}`,
  ]) {
    const t0 = performance.now();
    const r = parseConfig('env', line);
    assert.ok(performance.now() - t0 < 250, `${performance.now() - t0} ms`);
    assert.equal(r.root.entries.length, 1);
  }
  assert.equal(parseConfig('env', 'A=x  y \t # note\n').root.entries[0].node.value, 'x  y');
  assert.equal(parseConfig('env', 'A=x#y\n').root.entries[0].node.value, 'x#y');
});

// The env draft is no longer masked and sent (0.5.0): it is checked in the browser by hub-core (hubwasm.test.mjs proves
// no value leaves the tab). The name heuristic stays: it decides which Form fields are password fields.
test('isSecretEnvKey: which env keys the Form shows as password fields', () => {
  for (const k of [
    'hub_knowledge_token',
    'DB_PASSWD',
    'GPG_PASSPHRASE',
    'AUTH_HEADER',
    'HTTP_AUTHORIZATION',
    'AWS_CREDENTIALS',
    'BEARER',
    'SESSION_COOKIE',
    'SESSION_ID',
    'PRIVATE_PEM',
    'DB_PWD',
    'API_KEY',
    'GH_TOKEN',
    'CLIENT_SECRET',
    'MY_PASSWORD',
  ])
    assert.ok(isSecretEnvKey(k), k);
  for (const k of [
    'HUB_GIT_AUTHOR_NAME',
    'HUB_GIT_AUTHOR_EMAIL',
    'HUB_KNOWLEDGE_ADMIN_KEY_FILE',
    'GPG_PASSPHRASE_FILE',
    'HUB_PORT',
  ])
    assert.ok(!isSecretEnvKey(k), k);
  assert.ok(isSecretEnvKey('GIT_AUTHOR_TOKEN'), 'AUTHOR does not hide another secret word');
});

test("encodeValue(env) writes a value python-dotenv reads back unchanged, in the node's own style when it can", () => {
  const r = parseConfig('env', 'A=1\nB="x"\nC=\'y\'\nD=\n');
  const node = (k) => entryOf(r.root, k).node;
  assert.deepEqual(encodeValue('env', node('A'), '8793'), { raw: '8793' });
  assert.deepEqual(encodeValue('env', node('A'), 'a b #c'), { raw: "'a b #c'" });
  assert.deepEqual(encodeValue('env', node('A'), ' lead'), { raw: "' lead'" });
  assert.deepEqual(encodeValue('env', node('A'), "it's"), { raw: "it's" });
  assert.deepEqual(encodeValue('env', node('A'), "'q"), { raw: '"\'q"' });
  assert.deepEqual(encodeValue('env', node('B'), 'say "hi"\n'), { raw: '"say \\"hi\\"\\n"' });
  assert.deepEqual(encodeValue('env', node('C'), 'back\\slash'), { raw: "'back\\\\slash'" });
  assert.deepEqual(encodeValue('env', node('A'), '["a:1"]'), { raw: '["a:1"]' });
  assert.ok(encodeValue('env', node('A'), 'x\u0000y').error);
  // an empty value gets no separating space (KEY=value, not KEY= value)
  const t2 = spliceValue('A=1\nB="x"\nC=\'y\'\nD=\n', node('D'), encodeValue('env', node('D'), '5').raw);
  assert.equal(t2.split('\n')[3], 'D=5');
  // round trip through the parser
  for (const v of ['plain', 'with space', "quote'", 'dq"', 'tab\there', 'a #b', '${HOME}', '']) {
    const src = 'K=1\n';
    const n = parseConfig('env', src).root.entries[0].node;
    const out = spliceValue(src, n, encodeValue('env', n, v).raw);
    assert.equal(parseConfig('env', out).root.entries[0].node.value, v, JSON.stringify(v));
  }
});

test('install commands quote every path: ~/ as "$HOME"/\'...\', anything else single-quoted', () => {
  assert.equal(shPath('~/.config/agent-hub/env'), `"$HOME"/'.config/agent-hub/env'`);
  assert.equal(shPath("~/my dir/$x/it's.yaml"), `"$HOME"/'my dir/$x/it'\\''s.yaml'`);
  assert.equal(shPath('/srv/hub env'), `'/srv/hub env'`);
  assert.equal(shPath('~user/x'), `'~user/x'`, 'only ~/ expands');
  assert.equal(shq("a'b"), `'a'\\''b'`);
});

test('a check that looked at nothing is not a pass', () => {
  assert.ok(uncheckedResult({ ok: true, summary: { checked: false } }));
  assert.ok(!uncheckedResult({ ok: true, summary: { keys: [] } }));
  assert.ok(!uncheckedResult({ ok: false, diagnostics: [] }));
  assert.ok(!uncheckedResult(null));
});
