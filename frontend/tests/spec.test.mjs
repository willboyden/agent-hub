import test from 'node:test';
import assert from 'node:assert/strict';
import { defaultSpecText, mapSpecResult, issueMessage, parseSpec, specErrorResult } from '../js/spec.js';
import { SKIP_WASM, loadWasm } from './wasm-helper.mjs';
import { ApiError } from '../js/api.js';

test('known codes map to i18n keys with path/message variables; unknown codes keep the server text and hint', () => {
  assert.deepEqual(issueMessage({ code: 'missing_field', path: 'skills.dir', message: 'x' }), {
    key: 'spec.code.missing_field',
    vars: { path: 'skills.dir', message: 'x' },
    fallback: 'x',
  });
  const u = issueMessage({ code: 'weird', message: 'hello', fix_hint: 'try y' });
  assert.equal(u.key, null);
  assert.equal(u.fallback, 'hello');
  assert.equal(u.hint, 'try y');
  assert.equal(issueMessage({}).fallback, 'invalid');
});

test('mapSpecResult handles the real shape: artifacts + diagnostics with severity', () => {
  const r = mapSpecResult({
    ok: true,
    artifacts: [{ root: 'home', path: 'skills/x/SKILL.md', kind: 'skill', merge: 'own', preview: 'hello' }],
    diagnostics: [{ severity: 'warn', code: 'no_model_mapping', message: 'w', item: 'a' }],
  });
  assert.equal(r.ok, true);
  assert.equal(r.warnings.length, 1);
  assert.deepEqual(r.files, [
    { root: 'home', path: 'skills/x/SKILL.md', kind: 'skill', merge: 'own', content: 'hello' },
  ]);
  const bad = mapSpecResult({
    ok: false,
    artifacts: [],
    diagnostics: [{ severity: 'error', code: 'bad_value', message: 'no', item: '' }],
  });
  assert.equal(bad.ok, false);
  assert.equal(bad.errors.length, 1);
  assert.equal(
    mapSpecResult({ ok: true, errors: [{ code: 'bad_value', path: 'x', message: 'no' }] }).ok,
    false,
    'an error list overrides a truthy ok'
  );
  assert.equal(mapSpecResult(null).ok, false);
});

test('a 422 problem from the server becomes spec issues', () => {
  const e = new ApiError(422, {
    detail: 'spec failed validation',
    errors: [{ path: 'skills.dir', message: 'Field required', fix_hint: null }],
  });
  const r = specErrorResult(e);
  assert.equal(r.ok, false);
  assert.equal(r.errors[0].path, 'skills.dir');
  assert.match(r.errors[0].fallback, /Field required/);
  assert.equal(specErrorResult(new ApiError(422, { detail: 'boom' })).errors[0].fallback, 'boom');
});

test(
  "parseSpec reads a pasted spec with the hub's own YAML loader (PyYAML types, anchors, block scalars)",
  { skip: SKIP_WASM },
  async () => {
    const w = await loadWasm();
    const load = w.yamlLoad;
    assert.deepEqual(parseSpec('skills:\n  root: home\n  dir: skills\n', load).spec, {
      skills: { root: 'home', dir: 'skills' },
    });
    assert.deepEqual(parseSpec('{"skills": {"dir": "s"}}', load).spec, { skills: { dir: 's' } });
    assert.equal(parseSpec('   ', load).empty, true);
    // what yaml-lite read as strings (or refused), typed as PyYAML types them
    const spec = parseSpec(
      'base: &b {root: home}\nskills:\n  <<: *b\n  dir: skills\nflag: yes\nenabled: off\nnone: null\ntilde: ~\nratio: 1.5\nnote: |\n  two\n  lines\nflow: {a: 1}\n',
      load
    ).spec;
    assert.deepEqual(spec.skills, { root: 'home', dir: 'skills' });
    assert.equal(spec.flag, true);
    assert.equal(spec.enabled, false);
    assert.equal(spec.none, null);
    assert.equal(spec.tilde, null);
    assert.equal(spec.ratio, 1.5);
    assert.equal(spec.note, 'two\nlines\n');
    assert.deepEqual(spec.flow, { a: 1 });
    const bad = parseSpec('a: b\n  c: d\n', load);
    assert.equal(bad.error.code, 'yaml_syntax');
    assert.equal(bad.error.line, 2);
    assert.equal(parseSpec('{oops', load).error.code, 'yaml_syntax');
    // a key named __proto__ is an own data property, never the prototype
    const p = parseSpec('__proto__:\n  polluted: 1\n', load).spec;
    assert.equal({}.polluted, undefined);
    assert.equal(Object.getPrototypeOf(p), Object.prototype);
    // an alias bomb stops at the strict loader's node limit
    let bomb = 'l0: &l0 [a, a, a, a, a, a, a, a, a, a]\n';
    for (let i = 1; i < 8; i++)
      bomb += `l${i}: &l${i} [${Array(10)
        .fill(`*l${i - 1}`)
        .join(', ')}]\n`;
    assert.equal(parseSpec(bomb, load).error.code, 'yaml_syntax');
  }
);

test('parseSpec without the module: JSON still works, YAML says the reader is unavailable', () => {
  assert.deepEqual(parseSpec('{"skills": {"dir": "s"}}').spec, { skills: { dir: 's' } });
  assert.equal(parseSpec('{oops').error.code, 'yaml_syntax');
  assert.equal(parseSpec('skills:\n  dir: s\n').error.code, 'yaml_unavailable');
  assert.equal(parseSpec('a: 1', () => null).error.code, 'yaml_unavailable', 'a trapped module');
});

test("the wizard's default spec never blocks Next: JSON without the YAML reader, YAML with it", async (t) => {
  const defaults = { skills: { root: 'project', dir: 'skills' }, instructions: { target: 'AGENTS.md' } };
  const want = {
    skills: { root: 'project', dir: 'skills', layout: 'dir/SKILL.md' },
    instructions: { root: 'home', target: 'AGENTS.md', merge: 'block' },
  };
  // without the module (or after a trap) the default is JSON, which parseSpec reads on its own
  const json = defaultSpecText(defaults, false);
  assert.deepEqual(parseSpec(json).spec, want);
  assert.deepEqual(parseSpec(json, () => null).spec, want);
  assert.equal(defaultSpecText(null, false), '');
  assert.equal(defaultSpecText(null), '');
  // the YAML default needs the reader, and reads as the same object with it
  assert.equal(parseSpec(defaultSpecText(defaults, true)).error.code, 'yaml_unavailable');
  if (SKIP_WASM) return t.skip(SKIP_WASM);
  const load = (await loadWasm()).yamlLoad;
  assert.deepEqual(parseSpec(defaultSpecText(defaults, true), load).spec, want);
});
