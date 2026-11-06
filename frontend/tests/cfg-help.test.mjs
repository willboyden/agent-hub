// cfg-help.js + i18n/fields.en.json: the field help covers every key of every file the config editor offers (the
// shipped templates, the example client specs, the policy floor), every setting the hub's own loaders know (read
// from the Rust sources, so a new setting without help fails here), and the Settings > General fields.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { parseConfig } from '../js/cfg-parse.js';
import { flattenFields } from '../js/cfg-edit.js';
import { catalogFor, matchScore, lookupHelp, ENTRY_KEYS } from '../js/cfg-help.js';

const repo = (f) => readFileSync(new URL(`../../${f}`, import.meta.url), 'utf8');
const doc = JSON.parse(readFileSync(new URL('../i18n/fields.en.json', import.meta.url), 'utf8'));
const CATS = ['settings', 'hub-env', 'knowledge', 'client-spec', 'floor'];

test('the catalog document is well formed', () => {
  assert.deepEqual(Object.keys(doc.catalogs).sort(), [...CATS].sort());
  for (const [id, cat] of Object.entries(doc.catalogs)) {
    for (const [pat, e] of Object.entries(cat)) {
      const segs = pat.split('.');
      assert.ok(segs.every((s) => s !== ''), `${id}: empty segment in '${pat}'`);
      assert.ok(!segs.slice(0, -1).includes('**'), `${id}: '**' must be last in '${pat}'`);
      assert.ok(typeof e.what === 'string' && e.what.length >= 10, `${id}.${pat}: what`);
      assert.deepEqual(Object.keys(e).filter((k) => !ENTRY_KEYS.includes(k)), [], `${id}.${pat}: unknown keys`);
      for (const k of ['change', 'accepts', 'default', 'effect']) if (k in e) assert.equal(typeof e[k], 'string', `${id}.${pat}.${k}`);
      if ('choices' in e) assert.ok(Array.isArray(e.choices) && e.choices.every((c) => typeof c === 'string'), `${id}.${pat}.choices`);
      const all = ['what', 'change', 'accepts', 'default', 'effect'].map((k) => e[k] || '').join('');
      assert.equal((all.match(/`/g) || []).length % 2, 0, `${id}.${pat}: unbalanced backticks`);
      assert.doesNotMatch(all, /https?:\/\/(?!127\.0\.0\.1)/, `${id}.${pat}: external URL`);
    }
  }
});

test('matchScore and lookupHelp: literal beats wildcard, # matches list indexes, ** the rest, env names ignore case', () => {
  assert.ok(matchScore(['spec', 'mcp', 'format'], ['spec', 'mcp', 'format']) > matchScore(['spec', '*', 'format'], ['spec', 'mcp', 'format']));
  assert.equal(matchScore(['a', '#'], ['a', 'x']), -1);
  assert.ok(matchScore(['a', '#'], ['a', 0]) > 0);
  assert.ok(matchScore(['spec', '**'], ['spec', 'x', 'y']) > 0);
  assert.equal(matchScore(['spec', '**'], ['spec']), -1);
  const cs = catalogFor(doc, 'client-spec');
  assert.match(lookupHelp(cs, ['spec', 'mcp', 'format']).what, /format/);
  assert.match(lookupHelp(cs, ['spec', 'skills', 'root']).what, /named root/);
  assert.ok(lookupHelp(cs, ['spec', 'agents', 'fields', 'id']));
  assert.equal(lookupHelp(catalogFor(doc, 'hub-env'), ['hub_port']), doc.catalogs['hub-env'].HUB_PORT);
  assert.equal(catalogFor(doc, 'nope'), null);
  assert.equal(lookupHelp(null, ['x']), null);
});

test('every setting the hub reads from its env file has help (config.rs settings_model)', () => {
  const src = repo('backend/crates/agent-hub/src/config.rs');
  const names = [...src.matchAll(/optional\("([a-z_]+)"/g)].map((m) => m[1]);
  assert.ok(names.length >= 30, `${names.length} settings parsed`);
  const cat = catalogFor(doc, 'hub-env');
  for (const n of names) {
    const keys = n === 'knowledge_admin_key_file' ? ['KNOWLEDGE_ADMIN_KEY_FILE', 'HUB_KNOWLEDGE_ADMIN_KEY_FILE'] : [`HUB_${n.toUpperCase()}`];
    for (const k of keys) assert.ok(lookupHelp(cat, [k]), `hub-env: ${k}`);
  }
  // the template lists only names the hub reads, each with help
  for (const m of repo('deploy/agent-hub.env.example').matchAll(/^(?:# )?([A-Z_]+)=/gm)) assert.ok(lookupHelp(cat, [m[1]]), `template ${m[1]}`);
});

test('every key of the knowledge service config has help (hub_core::knowledge_config fields)', () => {
  const src = repo('backend/crates/hub-core/src/knowledge_config.rs');
  const body = src.slice(src.indexOf('fn fields()'), src.indexOf('enum Val'));
  const names = [...body.matchAll(/\("([a-z_]+)", (?:Kind|int|float)/g)].map((m) => m[1]);
  assert.ok(names.length >= 30, `${names.length} keys parsed`);
  const cat = catalogFor(doc, 'knowledge');
  for (const n of names) assert.ok(lookupHelp(cat, [n]), `knowledge: ${n}`);
  for (const f of ['deploy/knowledge/config.example.yaml', 'deploy/knowledge/container-config.yaml']) {
    const r = parseConfig('yaml', repo(f));
    assert.ok(r.root, f);
    for (const x of flattenFields(r.root)) assert.ok(lookupHelp(cat, x.path), `${f}: ${x.path.join('.')}`);
  }
});

test('every key of the example client specs and the policy floor has help', () => {
  const pairs = [['client-spec', 'examples/clients/cursor.yaml'], ['client-spec', 'examples/clients/codex-cli.yaml'], ['client-spec', 'examples/clients/gemini-cli.yaml'], ['floor', 'policy/floor.yaml']];
  for (const [id, f] of pairs) {
    const r = parseConfig('yaml', repo(f));
    assert.ok(r.root, `${f}: ${r.unsupported || JSON.stringify(r.errors)}`);
    const cat = catalogFor(doc, id);
    for (const x of flattenFields(r.root)) assert.ok(lookupHelp(cat, x.path), `${f}: ${x.path.join('.')}`);
  }
});

test('the Settings > General fields all have help', () => {
  const src = readFileSync(new URL('../js/views/settings.js', import.meta.url), 'utf8');
  const keys = [...src.matchAll(/(plan_retention|inbox_max_per_client|inbox_rate_per_min|eval_gate_observed|eval_gate_min_tiers|eval_gate)\b/g)].map((m) => m[1]);
  const cat = catalogFor(doc, 'settings');
  for (const k of new Set(keys)) {
    const e = lookupHelp(cat, [k]);
    assert.ok(e && e.effect && e.accepts && e.default, `settings: ${k}`);
  }
});
