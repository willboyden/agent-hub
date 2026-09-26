import test from 'node:test';
import assert from 'node:assert/strict';
import { validateSkill, validateAgent, validateFilePath, skillFrontmatter, mapCapabilities, agentClientPreview } from '../js/validate.js';
import { validName, slug } from '../js/util.js';

test('skill validation mirrors the server rules', () => {
  const ok = { name: 'code-review', description: 'Review a diff for bugs before merging', body: '# x' };
  assert.deepEqual(validateSkill(ok), []);
  assert.deepEqual(validateSkill({ ...ok, name: 'Bad Name' }).map((i) => i.key), ['val.name_bad']);
  assert.equal(validateSkill({ ...ok, description: '' })[0].key, 'val.desc_required');
  assert.equal(validateSkill({ ...ok, description: 'too short' })[0].key, 'val.desc_short');
  assert.equal(validateSkill({ ...ok, description: 'x'.repeat(1025) }).some((i) => i.key === 'val.desc_long'), true);
  assert.equal(validateSkill({ ...ok, body: '  ' })[0].severity, 'warn');
  assert.equal(validateSkill({ ...ok, body: '---\nx' }).some((i) => i.key === 'val.body_frontmatter'), true);
});
test('agent validation', () => {
  assert.equal(validateAgent({ name: 'a', description: 'd', capabilities: [], model_tier: 'fast' })[0].key, 'val.agent_no_caps');
  assert.equal(validateAgent({ name: 'a', description: 'd', capabilities: ['read'], model_tier: 'huge' })[0].key, 'val.tier_bad');
});
test('file path safety', () => {
  assert.equal(validateFilePath('references/a.md'), null);
  for (const p of ['/etc/x', '../x', 'a/../b', 'a\\b', '', 'SKILL.md']) assert.ok(validateFilePath(p), p);
  assert.equal(validateFilePath('a.md', ['a.md']).key, 'val.file_dup');
});
test('frontmatter is one-line and generated from the fields', () => assert.equal(skillFrontmatter({ name: 'n', description: 'a\n b' }), '---\nname: n\ndescription: a b\n---'));
test('capability -> native tool mapping and per-client preview severity', () => {
  const tm = { read: 'Read', shell: 'Bash', browser: null };
  assert.deepEqual(mapCapabilities(['read', 'browser', 'image'], tm).map((r) => [r.native, r.supported]), [['Read', true], [null, false], [null, false]]);
  const a = { capabilities: ['read', 'browser'], model_tier: 'deep' };
  const strict = agentClientPreview(a, { id: 'c', strict: true, caps: { agents: true, tool_map: tm, model_tiers: { deep: 'opus' } } });
  assert.equal(strict.severity, 'error'); assert.deepEqual(strict.missing, ['browser']); assert.equal(strict.model, 'opus');
  assert.equal(agentClientPreview(a, { id: 'c', strict: false, caps: { agents: true, tool_map: tm } }).severity, 'warn');
  assert.equal(agentClientPreview({ capabilities: ['read'] }, { id: 'c', caps: { agents: true, tool_map: tm } }).severity, 'ok');
  assert.equal(agentClientPreview(a, { id: 'c', caps: { agents: false } }).severity, 'na');
  assert.equal(agentClientPreview(a, { id: 'c', caps: { agents: true, tool_map: tm, model_tiers: {} } }).modelMissing, true);
});
test('names: validName and slug', () => {
  assert.equal(validName('a.b-c_d'), true); assert.equal(validName('A'), false); assert.equal(validName('-a'), false); assert.equal(validName('a'.repeat(65)), false);
  assert.equal(slug('  My Cool Skill!! '), 'my-cool-skill'); assert.equal(validName(slug('Cool skill 2')), true);
});
