// Getting started + FAQ content for every page (i18n/help.en.json). Pure logic, no DOM, so tests import it.
// The file holds one entry per page (the nav pages plus the editor: {lead, steps: [{title, body, cmd?, tab?, route?}],
// faq: [{q, a, cmd?, tab?, route?}]}) plus `_hub` ({faq}), the questions that apply on every page. Ported from Nemo
// Console; a `route` may carry a query (`/settings?tab=config`). Text is plain text: `backticks` mark
// inline code and a blank line splits paragraphs; nothing in it is ever parsed as HTML.

export const HELP_URL = 'i18n/help.en.json';
export const HUB_KEY = '_hub';
const STEP_KEYS = new Set(['title', 'body', 'cmd', 'tab', 'route']);
const FAQ_KEYS = new Set(['q', 'a', 'cmd', 'tab', 'route']);

let pending = null;
/** Fetch the help file once per page load; a failed fetch is not cached, so Retry fetches again. */
export function loadHelp(url = HELP_URL) {
  pending ||= fetch(url)
    .then((r) => {
      if (!r.ok) throw new Error(`help load failed: ${r.status}`);
      return r.json();
    })
    .catch((e) => {
      pending = null;
      throw e;
    });
  return pending;
}

/** Split text into paragraphs of [{code: bool, text}] runs: `x` is inline code, a blank line starts a paragraph. */
export function richRuns(text) {
  return String(text ?? '')
    .split(/\n\s*\n/)
    .map((para) =>
      para
        .split('`')
        .map((s, i) => ({ code: i % 2 === 1, text: s }))
        .filter((r) => r.text !== '')
    )
    .filter((p) => p.length);
}

/** FAQ items whose question or answer contains every word of the query (case-insensitive). An empty query keeps all. */
export function filterFaq(items, query) {
  const words = String(query ?? '')
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean);
  if (!words.length) return items.slice();
  return items.filter((it) => {
    const hay = `${it.q} ${it.a} ${it.cmd || ''}`.toLowerCase();
    return words.every((w) => hay.includes(w));
  });
}

/**
 * Problems with a help document, for the tests: every page has a lead, at least three steps and four questions,
 * every field is a non-empty string, `tab` names a tab of that page, `route` a nav route, and no URL leaves loopback.
 * @param {object} doc  the parsed help file
 * @param {Record<string, string[]>} tabsBySystem  tab ids each page can show (keys: the pages)
 * Note: `route` links are checked against the nav paths only; a `?tab=` query is checked for shape, not existence.
 * @param {string[]} routes  nav paths (`/settings`, ...)
 */
export function helpProblems(doc, tabsBySystem, routes) {
  const out = [];
  const str = (v) => typeof v === 'string' && v.trim().length > 0;
  const checkItem = (where, it, keys, required, tabs) => {
    if (!it || typeof it !== 'object') {
      out.push(`${where}: not an object`);
      return;
    }
    for (const k of Object.keys(it)) if (!keys.has(k)) out.push(`${where}: unknown key ${k}`);
    for (const k of required) if (!str(it[k])) out.push(`${where}: ${k} missing or empty`);
    for (const k of ['cmd', 'tab', 'route']) if (k in it && !str(it[k])) out.push(`${where}: ${k} empty`);
    if (it.tab && tabs && !tabs.includes(it.tab)) out.push(`${where}: unknown tab ${it.tab}`);
    if (it.tab && !tabs) out.push(`${where}: tab links are only for a page's own entry`);
    if (it.route && !routes.includes(it.route.split('?')[0])) out.push(`${where}: unknown route ${it.route}`);
    if (it.route && /[?&]tab=/.test(it.route) && !/^\?tab=[a-z_]+$/.test(it.route.slice(it.route.indexOf('?'))))
      out.push(`${where}: bad tab query in ${it.route}`);
    for (const [k, v] of Object.entries(it)) {
      if (typeof v !== 'string') continue;
      if (/https?:\/\/(?!(127\.0\.0\.1|localhost)[:/])/.test(v)) out.push(`${where}.${k}: external URL`); // loopback examples are fine
      if ((v.match(/`/g) || []).length % 2) out.push(`${where}.${k}: unbalanced backtick`);
    }
  };
  for (const id of Object.keys(doc || {})) if (id !== HUB_KEY && !(id in tabsBySystem)) out.push(`${id}: not a page`);
  const common = doc?.[HUB_KEY];
  if (!Array.isArray(common?.faq) || !common.faq.length) out.push(`${HUB_KEY}: faq missing`);
  else common.faq.forEach((it, i) => checkItem(`${HUB_KEY}.faq[${i}]`, it, FAQ_KEYS, ['q', 'a'], null));
  for (const [id, tabs] of Object.entries(tabsBySystem)) {
    const e = doc?.[id];
    if (!e) {
      out.push(`${id}: no entry`);
      continue;
    }
    if (!str(e.lead)) out.push(`${id}: lead missing`);
    if (!Array.isArray(e.steps) || e.steps.length < 3) out.push(`${id}: fewer than 3 steps`);
    else e.steps.forEach((it, i) => checkItem(`${id}.steps[${i}]`, it, STEP_KEYS, ['title', 'body'], tabs));
    if (!Array.isArray(e.faq) || e.faq.length < 4) out.push(`${id}: fewer than 4 questions`);
    else e.faq.forEach((it, i) => checkItem(`${id}.faq[${i}]`, it, FAQ_KEYS, ['q', 'a'], tabs));
    const qs = (e.faq || []).map((f) => f?.q);
    if (new Set(qs).size !== qs.length) out.push(`${id}: duplicate question`);
  }
  return out;
}
