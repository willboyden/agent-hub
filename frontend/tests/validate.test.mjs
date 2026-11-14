import test from 'node:test';
import assert from 'node:assert/strict';
import {
  validateSkill,
  validateAgent,
  validateFilePath,
  skillFrontmatter,
  mapCapabilities,
  agentClientPreview,
  modelIssues,
} from '../js/validate.js';
import { SKIP_WASM, loadWasm } from './wasm-helper.mjs';
import { validName, slug } from '../js/util.js';

test(
  "skill validation: blocking errors are the server model's, the 20-character rule is only a suggestion",
  { skip: SKIP_WASM },
  async () => {
    const meta = (await loadWasm()).validateMeta;
    const ok = { name: 'code-review', description: 'Review a diff for bugs before merging', body: '# x' };
    assert.deepEqual(validateSkill(ok, meta), []);
    assert.deepEqual(
      validateSkill({ ...ok, name: 'Bad Name' }, meta).map((i) => i.key),
      ['val.name_bad']
    );
    assert.equal(validateSkill({ ...ok, description: '' }, meta)[0].key, 'val.desc_required');
    // the server accepts 1..1024 characters (hub-core models.rs SkillMeta): 15 characters is a warning, never an error
    const short = validateSkill({ ...ok, description: 'Fifteen chars!!' }, meta);
    assert.deepEqual(
      short.map((i) => [i.severity, i.key]),
      [['warn', 'val.desc_short']]
    );
    assert.equal(short.filter((i) => i.severity === 'error').length, 0);
    assert.equal(
      validateSkill({ ...ok, description: 'x'.repeat(1025) }, meta).some((i) => i.key === 'val.desc_long'),
      true
    );
    assert.equal(validateSkill({ ...ok, body: '  ' }, meta)[0].severity, 'warn');
    assert.equal(
      validateSkill({ ...ok, body: '---\nx' }, meta).some((i) => i.key === 'val.body_frontmatter'),
      true
    );
  }
);
test('skill validation without the module: hints only, nothing blocks, and the editor says the checks are missing', () => {
  const issues = validateSkill({ name: 'Bad Name', description: '', body: '' });
  assert.ok(
    issues.every((i) => i.severity === 'warn'),
    JSON.stringify(issues)
  );
  assert.deepEqual(
    validateSkill({ name: 'a', description: 'short', body: 'b' }).map((i) => i.key),
    ['wasm.editor_unavailable', 'val.desc_short']
  );
  // a trapped module (validateMeta answers null) is never a silent clean state: the same note
  assert.deepEqual(
    validateSkill({ name: 'a', description: 'Review a diff for bugs before merging', body: 'b' }, () => null).map(
      (i) => i.key
    ),
    ['wasm.editor_unavailable']
  );
  assert.deepEqual(
    validateAgent({ name: 'a', description: 'd', capabilities: ['read'] }, () => null).map((i) => i.key),
    ['wasm.editor_unavailable']
  );
});
test(
  'a description of only spaces gets a suggestion (the server accepts it), never an error',
  { skip: SKIP_WASM },
  async () => {
    const meta = (await loadWasm()).validateMeta;
    const issues = validateSkill({ name: 'a-skill', description: '   ', body: 'b' }, meta);
    assert.deepEqual(
      issues.map((i) => [i.severity, i.key]),
      [['warn', 'val.desc_blank']]
    );
    // an empty one is the model's error, with no extra suggestion
    assert.deepEqual(
      validateSkill({ name: 'a-skill', description: '', body: 'b' }, meta).map((i) => i.key),
      ['val.desc_required']
    );
  }
);
test('agent validation', { skip: SKIP_WASM }, async () => {
  const meta = (await loadWasm()).validateMeta;
  assert.equal(
    validateAgent({ name: 'a', description: 'd', capabilities: [], model_tier: 'fast' }, meta)[0].key,
    'val.agent_no_caps'
  );
  assert.equal(
    validateAgent({ name: 'a', description: 'd', capabilities: ['read'], model_tier: 'huge' }, meta)[0].key,
    'val.tier_bad'
  );
  const caps = validateAgent({ name: 'a', description: 'd', capabilities: ['read', 'fly'] }, meta);
  assert.equal(caps[0].key, 'val.model');
  assert.match(caps[0].vars.msg, /unknown capability/);
});
test("model errors map to the editor's words, anything else keeps the field and pydantic's message", () => {
  assert.deepEqual(
    modelIssues([
      { loc: ['name'], msg: 'Value error, x', type: 'value_error' },
      { loc: ['description'], msg: 'too long', type: 'string_too_long' },
      { loc: ['mode'], msg: "Input should be 'subagent' or 'primary'", type: 'literal_error' },
    ]).map((i) => [i.key, i.vars.field ?? '']),
    [
      ['val.name_bad', ''],
      ['val.desc_long', ''],
      ['val.model', 'mode'],
    ]
  );
  assert.deepEqual(modelIssues(null), []);
});
test('file path safety', () => {
  assert.equal(validateFilePath('references/a.md'), null);
  for (const p of ['/etc/x', '../x', 'a/../b', 'a\\b', '', 'SKILL.md']) assert.ok(validateFilePath(p), p);
  assert.equal(validateFilePath('a.md', ['a.md']).key, 'val.file_dup');
});
test('frontmatter is one-line and generated from the fields', () =>
  assert.equal(skillFrontmatter({ name: 'n', description: 'a\n b' }), '---\nname: n\ndescription: a b\n---'));
test('capability -> native tool mapping and per-client preview severity', () => {
  const tm = { read: 'Read', shell: 'Bash', browser: null };
  assert.deepEqual(
    mapCapabilities(['read', 'browser', 'image'], tm).map((r) => [r.native, r.supported]),
    [
      ['Read', true],
      [null, false],
      [null, false],
    ]
  );
  const a = { capabilities: ['read', 'browser'], model_tier: 'deep' };
  const strict = agentClientPreview(a, {
    id: 'c',
    strict: true,
    caps: { agents: true, tool_map: tm, model_tiers: { deep: 'opus' } },
  });
  assert.equal(strict.severity, 'error');
  assert.deepEqual(strict.missing, ['browser']);
  assert.equal(strict.model, 'opus');
  assert.equal(
    agentClientPreview(a, { id: 'c', strict: false, caps: { agents: true, tool_map: tm } }).severity,
    'warn'
  );
  assert.equal(
    agentClientPreview({ capabilities: ['read'] }, { id: 'c', caps: { agents: true, tool_map: tm } }).severity,
    'ok'
  );
  assert.equal(agentClientPreview(a, { id: 'c', caps: { agents: false } }).severity, 'na');
  assert.equal(
    agentClientPreview(a, { id: 'c', caps: { agents: true, tool_map: tm, model_tiers: {} } }).modelMissing,
    true
  );
});
test('names: validName and slug', () => {
  assert.equal(validName('a.b-c_d'), true);
  assert.equal(validName('A'), false);
  assert.equal(validName('-a'), false);
  assert.equal(validName('a'.repeat(65)), false);
  assert.equal(slug('  My Cool Skill!! '), 'my-cool-skill');
  assert.equal(validName(slug('Cool skill 2')), true);
});
