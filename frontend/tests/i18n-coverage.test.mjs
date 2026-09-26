import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const en = JSON.parse(fs.readFileSync(path.join(root, 'i18n/en.json'), 'utf8'));
const walk = (d) => fs.readdirSync(d, { withFileTypes: true }).flatMap((e) => (e.isDirectory() ? walk(path.join(d, e.name)) : e.name.endsWith('.js') ? [path.join(d, e.name)] : []));
const files = walk(path.join(root, 'js'));
const ns = new Set(Object.keys(en).map((k) => k.split('.')[0]));
const has = (k) => k in en || `${k}_one` in en || `${k}_other` in en;
const NS_RE = /^(nav|lib|coll|mx|editor|cl|imp|chg|plan|diff|home|inbox|k|set|val|mcp|rule|rules|new|picker|wizard|err|common|auth|topbar|palette|spec|kind|status|floor|sugg)\./;

test('every literal key used in js/ exists in en.json (t("x.y") and any quoted "ns.key" of a known namespace)', () => {
  const missing = [];
  for (const f of files) {
    const src = fs.readFileSync(f, 'utf8'); const keys = new Set();
    for (const m of src.matchAll(/\bt\(\s*['"`]([a-z_]+\.[a-z_0-9.]+)['"`]/g)) keys.add(m[1]);
    for (const m of src.matchAll(/['"]([a-z_]+\.[a-z_0-9]+)['"]/g)) if (ns.has(m[1].split('.')[0]) && NS_RE.test(m[1])) keys.add(m[1]);
    for (const k of keys) {
      if (/\.(md|json|csv|yaml|mdc|js)$/.test(k) || /^(spec\.(skills|instructions|agents|mcp|roots|dir|root|target|file|layout|merge|tools)|k\.(id|name|dim|kind|path|count|score|text)|set\.(router|knowledge|floor_path|content_dir|state_dir)$)/.test(k)) continue;
      if (!has(k)) missing.push(`${path.relative(root, f)}: ${k}`);
    }
  }
  assert.deepEqual([...new Set(missing)], []);
});

test('dynamic key families are complete', () => {
  const fam = {
    'kind.': ['skill', 'skills', 'agent', 'agents', 'instruction', 'instructions', 'mcp', 'mcps', 'rule', 'rules', 'memory', 'memorys'],
    'status.': ['edited_outside', 'pending_changes', 'in_sync', 'drift', 'never_applied', 'error', 'clean', 'findings', 'unscanned', 'applied', 'blocked', 'partial', 'nothing', 'failed'],
    'mx.state_': ['on', 'off', 'via_collection', 'unsupported', 'blocked'], 'mx.legend_': ['on', 'via_collection', 'off', 'blocked', 'unsupported', 'pending'],
    'plan.a_': ['add', 'change', 'remove', 'unchanged', 'conflict', 'advisory'], 'rule.dec_': ['allow', 'ask', 'deny'],
    'rule.kind_': ['tool', 'command', 'path_read', 'path_write', 'egress_host', 'mcp_server'], 'rule.match_hint_': ['tool', 'command', 'path_read', 'path_write', 'egress_host', 'mcp_server'],
    'cap.': ['read', 'write', 'edit', 'shell', 'web_fetch', 'web_search', 'mcp', 'subagent', 'browser', 'notebook', 'todo', 'image'], 'tier.': ['fast', 'standard', 'deep'],
    'concern.': ['skills', 'agents', 'instructions', 'mcp', 'permissions', 'memory'], 'memtype.': ['user', 'feedback', 'project', 'reference'], 'time.': ['s', 'm', 'h', 'd'],
    'home.step_': ['import', 'organise', 'plan', 'apply'], 'chg.s_': ['added', 'modified', 'deleted'], 'chg.gate_': ['not_reviewed', 'all_blocked', 'no_clients', 'nothing_to_do'],
    'imp.ch_': ['link', 'rename', 'replace', 'skip'], 'imp.a_': ['import', 'link', 'skip', 'rename', 'replace'], 'sugg.reason_': ['common_prefix', 'source', 'tag', 'keyword'], 'k.tab_': ['namespaces', 'ingest', 'query', 'tokens', 'indexes'], 'k.job_': ['running', 'done', 'failed'],
    'set.tab_': ['general', 'keys', 'floor', 'audit'], 'set.role_': ['admin', 'viewer'], 'lib.sort_': ['relevance', 'name', 'updated', 'kind', 'issues', 'enabled'],
    'inbox.reason_': ['override', 'exfil', 'secrets', 'url', 'pipe_shell', 'disable_guard', 'standing_order'], 'cl.dstate_': ['modified_live', 'missing', 'unmanaged', 'in_sync', 'advisory', 'never_applied', 'pending', 'orphaned', 'changed_live'],
    'editor.sev_': ['ok', 'warn', 'error', 'na'], 'common.sev_': ['error', 'warn', 'info'], 'agentmode.': ['subagent', 'primary'],
    'spec.code.': ['missing_field', 'unknown_key', 'bad_type', 'bad_value', 'bad_path', 'yaml_syntax', 'unsafe_path', 'no_skills_root', 'unsupported'],
    'k.mode_': ['read', 'write'], 'floor.': ['network', 'path_read', 'path_write', 'tool', 'command', 'mcp_scan', 'mcp_egress', 'why_network', 'why_paths', 'why_tool', 'why_command', 'why_mcp', 'why_egress'],
    'err.': ['cell', 'never_applied', 'client_not_committed', 'content_not_initialised', 'content_invalid', 'dirty_tree', 'exists', 'confirm', 'floor_locked', 'adapter', 'method', 'network', 'unauthorized', 'csrf', 'forbidden', 'plan_stale', 'conflict', 'conflict_generic', 'too_large', 'unsupported_media', 'bad_host', 'rate_limited', 'unavailable', 'invalid'],
    'nav.': ['home', 'library', 'collections', 'matrix', 'clients', 'import', 'changes', 'inbox', 'knowledge', 'settings'],
  };
  for (const [p, ks] of Object.entries(fam)) for (const k of ks) assert.ok(has(`${p}${k}`), `${p}${k}`);
});

test('every val.* key produced by validate.js and every spec code has text', () => {
  const src = fs.readFileSync(path.join(root, 'js/validate.js'), 'utf8');
  for (const m of src.matchAll(/'(val\.[a-z_]+)'/g)) assert.ok(has(m[1]), m[1]);
  const spec = fs.readFileSync(path.join(root, 'js/spec.js'), 'utf8');
  const known = /new Set\(\[([^\]]+)\]\)/.exec(spec)[1].match(/'(\w+)'/g).map((s) => s.slice(1, -1));
  for (const k of known) assert.ok(`spec.code.${k}` in en, k);
});

test('no en.json value is empty', () => { for (const [k, v] of Object.entries(en)) assert.ok(typeof v === 'string' && v.length, k); });

test('no external origins, no eval, no style attributes, no inline scripts', () => {
  for (const f of [...files, path.join(root, 'index.html'), ...fs.readdirSync(path.join(root, 'css')).map((x) => path.join(root, 'css', x))]) {
    const src = fs.readFileSync(f, 'utf8');
    const bad = [...src.matchAll(/https?:\/\/[^\s'"`)]+/g)].map((m) => m[0]).filter((u) => !u.startsWith('http://www.w3.org/2000/svg'));
    assert.deepEqual(bad, [], path.relative(root, f));
  }
  for (const f of files) {
    const src = fs.readFileSync(f, 'utf8'); const rel = path.relative(root, f);
    assert.ok(!/\beval\s*\(|new Function\(/.test(src), `${rel} uses eval`);
    if (!rel.endsWith('dom.js')) assert.ok(!/\.innerHTML\s*=(?!=)/.test(src), `${rel} assigns innerHTML`);
    assert.ok(!/setAttribute\(\s*['"]style['"]/.test(src), `${rel} sets a style attribute`);
    assert.ok(!/\bstyle:\s*['"`{]/.test(src), `${rel} passes a style attribute to h()`);
    assert.ok(!/\bon(click|input|change|load|error)\s*=\s*['"]/.test(src), `${rel} has an inline handler string`);
  }
  const html = fs.readFileSync(path.join(root, 'index.html'), 'utf8');
  assert.ok(!/<script(?![^>]*\bsrc=)/i.test(html), 'inline script in index.html');
  assert.ok(!/\sstyle=/.test(html));
});

test('setTrustedHtml is only fed by the markdown renderer', () => {
  for (const f of files) {
    const src = fs.readFileSync(f, 'utf8'); const rel = path.relative(root, f);
    if (rel.endsWith('dom.js')) continue;
    for (const m of src.matchAll(/setTrustedHtml\(([^;]*)\);/g)) assert.match(m[1], /renderMarkdown\(/, `${rel}: ${m[1]}`);
  }
});
