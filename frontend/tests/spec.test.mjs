import test from 'node:test';
import assert from 'node:assert/strict';
import { mapSpecResult, issueMessage, parseSpec, specErrorResult } from '../js/spec.js';
import { parseYamlLite, YamlLiteError } from '../js/yaml-lite.js';
import { ApiError } from '../js/api.js';

const plain = (o) => JSON.parse(JSON.stringify(o));
test('known codes map to i18n keys with path/message variables; unknown codes keep the server text and hint', () => {
  assert.deepEqual(issueMessage({ code: 'missing_field', path: 'skills.dir', message: 'x' }), { key: 'spec.code.missing_field', vars: { path: 'skills.dir', message: 'x' }, fallback: 'x' });
  const u = issueMessage({ code: 'weird', message: 'hello', fix_hint: 'try y' });
  assert.equal(u.key, null); assert.equal(u.fallback, 'hello'); assert.equal(u.hint, 'try y');
  assert.equal(issueMessage({}).fallback, 'invalid');
});

test('mapSpecResult handles the real shape: artifacts + diagnostics with severity', () => {
  const r = mapSpecResult({ ok: true, artifacts: [{ root: 'home', path: 'skills/x/SKILL.md', kind: 'skill', merge: 'own', preview: 'hello' }], diagnostics: [{ severity: 'warn', code: 'no_model_mapping', message: 'w', item: 'a' }] });
  assert.equal(r.ok, true); assert.equal(r.warnings.length, 1); assert.deepEqual(r.files, [{ root: 'home', path: 'skills/x/SKILL.md', kind: 'skill', merge: 'own', content: 'hello' }]);
  const bad = mapSpecResult({ ok: false, artifacts: [], diagnostics: [{ severity: 'error', code: 'bad_value', message: 'no', item: '' }] });
  assert.equal(bad.ok, false); assert.equal(bad.errors.length, 1);
  assert.equal(mapSpecResult({ ok: true, errors: [{ code: 'bad_value', path: 'x', message: 'no' }] }).ok, false, 'an error list overrides a truthy ok');
  assert.equal(mapSpecResult(null).ok, false);
});

test('a 422 problem from the server becomes spec issues', () => {
  const e = new ApiError(422, { detail: 'spec failed validation', errors: [{ path: 'skills.dir', message: 'Field required', fix_hint: null }] });
  const r = specErrorResult(e);
  assert.equal(r.ok, false); assert.equal(r.errors[0].path, 'skills.dir'); assert.match(r.errors[0].fallback, /Field required/);
  assert.equal(specErrorResult(new ApiError(422, { detail: 'boom' })).errors[0].fallback, 'boom');
});

test('parseSpec: YAML and JSON become objects; syntax problems are reported with a line and never thrown', () => {
  assert.deepEqual(plain(parseSpec('skills:\n  root: home\n  dir: skills\n').spec), { skills: { root: 'home', dir: 'skills' } });
  assert.deepEqual(parseSpec('{"skills": {"dir": "s"}}').spec, { skills: { dir: 's' } });
  assert.equal(parseSpec('   ').empty, true);
  const bad = parseSpec('a: b\n  c: d\n'); assert.equal(bad.error.code, 'yaml_syntax'); assert.equal(bad.error.line, 2);
  assert.equal(parseSpec('{oops').error.code, 'yaml_syntax');
});

test('yaml-lite: maps, inline and block lists, comments, quotes, numbers/bools; rejects what it cannot parse', () => {
  assert.deepEqual(plain(parseYamlLite('a:\n  b: 1  # c\n  c: [x, "y z"]\n  t: true\nd:\n  - p\n  - q\n# x\n')), { a: { b: 1, c: ['x', 'y z'], t: true }, d: ['p', 'q'] });
  for (const bad of ['a: &x 1\n', 'a: |\n  text\n', '---\na: 1\n---\nb: 2\n', 'a: 1\na: 2\n', 'a: {b: 1}\n', 'nonsense line\n']) assert.throws(() => parseYamlLite(bad), YamlLiteError, bad);
  assert.deepEqual(plain(parseYamlLite('a:\nb: 2\n')), { a: null, b: 2 });
});

test('yaml-lite rejects prototype-polluting keys and returns null-prototype objects', () => {
  for (const k of ['__proto__', 'constructor', 'prototype']) assert.throws(() => parseYamlLite(`a:\n  ${k}:\n    polluted: 1\n`), YamlLiteError, k);
  assert.equal(({}).polluted, undefined);
  const o = parseYamlLite('a:\n  b: 1\n'); assert.equal(Object.getPrototypeOf(o), null); assert.equal(Object.getPrototypeOf(o.a), null);
  assert.deepEqual(JSON.parse(JSON.stringify(o)), { a: { b: 1 } });
  assert.equal(parseSpec('__proto__: x\n').error.code, 'yaml_syntax');
});
