// Getting started + FAQ (help.js, i18n/help.en.json): every page has a guide and questions, links point at real tabs
// and pages, nothing links off the machine, and the text helpers never produce markup. Ported from Nemo Console.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { helpProblems, richRuns, filterFaq, HUB_KEY } from '../js/help.js';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const read = (f) => fs.readFileSync(path.join(root, f), 'utf8');
const doc = JSON.parse(read('i18n/help.en.json'));
const ROUTES = [...read('js/app.js').matchAll(/\{ path: '(\/[^']*)'[^}]*?(hidden: true)?\s*\}/g)].filter((m) => !m[2]).map((m) => m[1]);

// The tab ids each page can show. Tabbed views define their own (views/*.js) and add Getting started + FAQ from
// help-tabs.js; a page without tabs gets `page` | start | faq from helpFrame(). Clients: the list is tab-less, the
// detail page has its own tabs, and both use the `clients` entry.
const HELP_TABS = ['start', 'faq'];
const OWN = {
  settings: ['general', 'doctor', 'keys', 'floor', 'audit', 'config'],
  knowledge: ['namespaces', 'ingest', 'query', 'tokens', 'indexes'],
  changes: ['pending', 'plan', 'history'],
  editor: ['edit', 'rendered', 'used'],
  clients: ['overview', 'manage', 'health', 'page'],
};
const FRAMED = ['home', 'library', 'evaluations', 'collections', 'matrix', 'import', 'inbox'];
const tabsByPage = Object.fromEntries([...Object.entries(OWN), ...FRAMED.map((p) => [p, ['page']])].map(([k, v]) => [k, [...v, ...HELP_TABS]]));

test('every nav page and the editor have a help entry, and nothing else does', () => {
  const navPages = ROUTES.map((r) => (r === '/' ? 'home' : r.slice(1)));
  assert.deepEqual([...navPages, 'editor'].sort(), Object.keys(tabsByPage).sort());
  assert.deepEqual(Object.keys(doc).filter((k) => k !== HUB_KEY).sort(), Object.keys(tabsByPage).sort());
});

test('the tab table matches the tabs the views build', () => {
  const ids = (src) => [...src.matchAll(/\{ id: '([a-z_]+)', label: t\('/g)].map((m) => m[1]);
  const listed = (src) => { const m = /const ids = \[([^\]]+)\]/.exec(src); return m ? [...m[1].matchAll(/'([a-z_]+)'/g)].map((x) => x[1]) : []; };
  assert.deepEqual(listed(read('js/views/settings.js')), OWN.settings);
  assert.deepEqual(listed(read('js/views/knowledge.js')), OWN.knowledge);
  assert.deepEqual(ids(read('js/views/changes.js')), OWN.changes);
  assert.deepEqual(ids(read('js/views/editor.js')), OWN.editor);
  assert.deepEqual(ids(read('js/views/clients.js')), OWN.clients.filter((x) => x !== 'page'));
  for (const v of ['settings', 'knowledge', 'changes', 'editor', 'clients']) assert.match(read(`js/views/${v}.js`), /helpTabItems\(\)/, v);
  for (const v of [...FRAMED, 'clients']) assert.match(read(`js/views/${v === 'home' ? 'home' : v}.js`), new RegExp(`helpFrame\\(this, \\{ key: '${v}'`), v);
  assert.match(read('js/components/help-tabs.js'), /export const HELP_TABS = \['start', 'faq'\]/);
});

test('help.en.json has a valid guide and FAQ for every page, and the hub-wide questions', () => {
  assert.deepEqual(helpProblems(doc, tabsByPage, ROUTES), []);
});

test('helpProblems catches the mistakes it is meant to catch', () => {
  const bad = {
    [HUB_KEY]: { faq: [{ q: 'Q', a: 'A', tab: 'general' }] },
    settings: { lead: 'x', steps: [{ title: 't', body: 'b', tab: 'nope' }], faq: [{ q: 'q', a: 'see https://example.org, not http://127.0.0.1:8792/' }, { q: 'q', a: 'odd `tick' }, { q: 'r', a: 'a', route: '/nowhere' }, { q: 's', a: 'a', extra: 1 }, { q: 'u', a: 'a', route: '/settings?tab=x&y=1' }] },
    nosuch: {},
  };
  const out = helpProblems(bad, { settings: ['general', 'start', 'faq'], library: ['page'] }, ['/settings']);
  for (const want of ['nosuch: not a page', 'only for a page', 'fewer than 3 steps', 'external URL', 'unbalanced backtick', 'unknown route /nowhere', 'unknown key extra', 'duplicate question', 'library: no entry', 'bad tab query']) {
    assert.ok(out.some((p) => p.includes(want)), `${want} in ${JSON.stringify(out)}`);
  }
});

test('richRuns splits paragraphs and inline code, and never yields markup', () => {
  assert.deepEqual(richRuns('Run `make all` now.\n\nThen <b>stop</b>.'), [
    [{ code: false, text: 'Run ' }, { code: true, text: 'make all' }, { code: false, text: ' now.' }],
    [{ code: false, text: 'Then <b>stop</b>.' }],
  ]);
  assert.deepEqual(richRuns(''), []);
  assert.deepEqual(richRuns(undefined), []);
});

test('filterFaq matches every word in question, answer or command, case-insensitively', () => {
  const items = [{ q: 'Plan is stale', a: 'Make a new plan.' }, { q: 'Why 409?', a: 'stale base', cmd: 'hubctl plan' }];
  assert.equal(filterFaq(items, '').length, 2);
  assert.deepEqual(filterFaq(items, 'NEW').map((i) => i.q), ['Plan is stale']);
  assert.deepEqual(filterFaq(items, '409 stale').map((i) => i.q), ['Why 409?']);
  assert.deepEqual(filterFaq(items, 'hubctl').map((i) => i.q), ['Why 409?']);
  assert.deepEqual(filterFaq(items, 'new 409'), []);
});

test('help text names no lab host, private address or user path (the repo is public)', () => {
  const text = JSON.stringify(doc) + read('i18n/fields.en.json');
  assert.doesNotMatch(text, /\/home\/[a-z]|\b(?:10|192\.168|172\.(?:1[6-9]|2\d|3[01]))\.\d+\.\d+/);
});

test('help-panel renders help text as text nodes only', () => {
  for (const f of ['js/components/help-panel.js', 'js/components/help-tabs.js', 'js/components/info-pop.js', 'js/components/config-editor.js', 'js/components/config-form.js']) {
    assert.doesNotMatch(read(f), /innerHTML|setTrustedHtml|insertAdjacentHTML/, f);
  }
});
