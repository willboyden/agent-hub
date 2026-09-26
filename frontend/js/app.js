// Bootstrap: i18n, theme, shell (nav/topbar), router -> lazy-loaded view custom elements, auth dialog, nav badges, Ctrl-K palette.
import { loadMessages, t } from './i18n.js';
import { store, api, setApiKey, applyTheme } from './app-context.js';
import { createRouter } from './router.js';
import { h, icon, clear, $ } from './dom.js';
import { toast } from './components/toast.js';
import { registerCommands } from './components/palette.js';
import { openModal } from './components/dialog.js';
import { KINDS } from './api.js';
import { stagedCount } from './matrix.js';
import './components/toast.js';
import './components/palette.js';

export const ROUTES = [
  { path: '/', tag: 'ah-home', mod: 'home', nav: 'home', icon: 'home' },
  { path: '/library', tag: 'ah-library', mod: 'library', nav: 'library', icon: 'library' },
  { path: '/collections', tag: 'ah-collections', mod: 'collections', nav: 'collections', icon: 'collections' },
  { path: '/collections/:id', tag: 'ah-collections', mod: 'collections', nav: 'collections', icon: 'collections', hidden: true },
  { path: '/matrix', tag: 'ah-matrix', mod: 'matrix', nav: 'matrix', icon: 'matrix' },
  { path: '/edit/:kind/:name', tag: 'ah-editor', mod: 'editor', nav: 'library', icon: 'editor', hidden: true },
  { path: '/clients', tag: 'ah-clients', mod: 'clients', nav: 'clients', icon: 'clients' },
  { path: '/clients/:id', tag: 'ah-clients', mod: 'clients', nav: 'clients', icon: 'clients', hidden: true },
  { path: '/import', tag: 'ah-import', mod: 'import', nav: 'import', icon: 'import' },
  { path: '/changes', tag: 'ah-changes', mod: 'changes', nav: 'changes', icon: 'changes' },
  { path: '/inbox', tag: 'ah-inbox', mod: 'inbox', nav: 'inbox', icon: 'inbox' },
  { path: '/knowledge', tag: 'ah-knowledge', mod: 'knowledge', nav: 'knowledge', icon: 'knowledge' },
  { path: '/settings', tag: 'ah-settings', mod: 'settings', nav: 'settings', icon: 'settings' },
];

function buildShell(root) {
  const nav = h('nav', { class: 'nav', 'aria-label': t('nav.primary') },
    ROUTES.filter((r) => !r.hidden).map((r) => h('a', { href: `#${r.path}`, 'data-route': r.path, title: t(`nav.${r.nav}`) }, icon(r.icon), h('span', { class: 'nav-label' }, t(`nav.${r.nav}`)), h('span', { class: 'nav-badge', id: `nb-${r.nav}`, hidden: true }))));
  const themeBtn = h('button', { type: 'button', class: 'btn ghost', id: 'theme-btn', 'aria-label': t('topbar.theme'), onClick: toggleTheme });
  const keyBtn = h('button', { type: 'button', class: 'btn ghost', 'aria-label': t('topbar.api_key'), title: t('topbar.api_key'), onClick: () => promptKey() }, icon('key'));
  const palBtn = h('button', { type: 'button', class: 'btn ghost palette-btn', onClick: () => window.dispatchEvent(new Event('ah:palette')) },
    icon('search', 16), h('span', { class: 'palette-label' }, t('palette.open')), h('kbd', 'Ctrl K'));
  root.append(
    h('a', { class: 'skip', href: '#main' }, t('common.skip')),
    h('aside', { class: 'sidebar' }, h('div', { class: 'brand' }, h('span', { class: 'logo', 'aria-hidden': 'true' }, '◆'), h('span', { class: 'brand-name' }, t('app.name'))), nav),
    h('div', { class: 'content' },
      h('div', { id: 'banner', class: 'banner', role: 'status' }),
      h('header', { class: 'topbar' }, h('div', { id: 'crumb', class: 'crumb', role: 'heading', 'aria-level': '1' }), h('div', { class: 'row gap' }, palBtn, themeBtn, keyBtn)),
      h('main', { id: 'main', tabindex: '-1' })),
    document.createElement('ah-toasts'), document.createElement('ah-palette'));
  syncThemeBtn();
}
function syncThemeBtn() {
  const b = $('#theme-btn'); if (!b) return;
  const light = document.documentElement.dataset.theme === 'light';
  clear(b).append(icon(light ? 'moon' : 'sun'));
  b.title = light ? t('topbar.to_dark') : t('topbar.to_light');
}
function toggleTheme() { applyTheme(document.documentElement.dataset.theme === 'light' ? 'dark' : 'light'); syncThemeBtn(); }

let authDialog = null;
function promptKey() {
  if (authDialog) return;
  const input = h('input', { type: 'password', autocomplete: 'off', spellcheck: 'false', 'aria-label': t('auth.key_label'), placeholder: 'hc_…', value: store.get().apiKey || '' });
  const form = h('form', { onSubmit: (e) => { e.preventDefault(); setApiKey(input.value.trim()); authDialog?.close(); toast(t('auth.saved'), { kind: 'ok' }); router.refresh(); refreshBadges(); } },
    h('p', t('auth.explain')), h('div', { class: 'field' }, h('label', { for: 'key-in' }, t('auth.key_label')), input),
    h('div', { class: 'row gap end' }, h('button', { type: 'button', class: 'btn', onClick: () => { setApiKey(''); authDialog?.close(); } }, t('auth.clear')), h('button', { type: 'submit', class: 'btn primary' }, t('common.save'))));
  input.id = 'key-in';
  authDialog = openModal(t('auth.title'), form, { onClose: () => { authDialog = null; } });
  input.focus();
}

function setBadge(nav, n, label) {
  const el = document.getElementById(`nb-${nav}`); if (!el) return;
  el.hidden = !n; el.textContent = n ? String(n) : ''; if (n) el.setAttribute('aria-label', label);
}
// Nav badges: pending content changes, inbox items, unsaved matrix edits. Best effort; failures leave the badge as is.
async function refreshBadges() {
  try {
    const [ch, inbox] = await Promise.all([api.changes(), api.inbox()]);
    setBadge('changes', ch.count, t('nav.badge_changes', { count: ch.count }));
    const n = inbox.length; setBadge('inbox', n, t('nav.badge_inbox', { count: n }));
  } catch { /* offline or unauthorised: the view itself shows the error */ }
  const s = stagedCount({ staged: store.get().staged }); setBadge('matrix', s, t('nav.badge_matrix', { count: s }));
}

// Health warnings from the backend (e.g. loopback trust is ON) and an uninitialised content repo are shown on every page.
async function showBanner() {
  const el = $('#banner'); if (!el) return;
  try {
    const hl = await api.health();
    const msgs = [...(hl.warnings || [])];
    if (hl.content_initialised === false) msgs.unshift(t('app.uninitialised'));
    clear(el).append(...msgs.map((m) => h('p', { class: 'note warn' }, m)));
  } catch { clear(el); }
}

let current = null, seq = 0;
async function navigate({ path, route, params, query }) {
  const main = $('#main');
  const my = ++seq;
  if (!route) { clear(main).append(h('div', { class: 'state empty' }, h('strong', t('common.not_found')), h('a', { href: '#/' }, t('nav.home')))); return; }
  const navPath = ROUTES.find((r) => !r.hidden && r.nav === route.nav)?.path;
  document.querySelectorAll('.nav a').forEach((a) => {
    const on = a.dataset.route === navPath;
    if (on) a.setAttribute('aria-current', 'page'); else a.removeAttribute('aria-current');
  });
  const title = route.hidden && route.path.startsWith('/edit') ? `${t(`kind.${params.kind}`)}: ${params.name}` : t(`nav.${route.nav}`);
  $('#crumb').textContent = title;
  document.title = `${title} · ${t('app.name')}`;
  try { await import(`./views/${route.mod}.js`); }
  catch (e) { clear(main).append(h('div', { class: 'state error', role: 'alert' }, `${t('common.error')}: ${e.message}`)); return; }
  if (my !== seq) return;
  current?.remove();
  current = document.createElement(route.tag);
  current.params = params || {}; current.query = query || {}; current.path = path;
  clear(main).append(current);
  main.focus({ preventScroll: true });
  refreshBadges();
}

let router;
async function boot() {
  try { await loadMessages(); } catch (e) { console.error(e); }
  if (/[?&]theme=(light|dark)/.test(location.search)) store.set({ theme: document.documentElement.dataset.theme }); else applyTheme(store.get().theme);
  buildShell($('#app'));
  showBanner();
  store.subscribe((a) => { if (a) promptKey(); }, (s) => s.authRequired);
  store.subscribe((staged) => { const n = Object.keys(staged || {}).length; setBadge('matrix', n, t('nav.badge_matrix', { count: n })); }, (s) => s.staged);
  window.addEventListener('ah:changed', () => refreshBadges());
  const goto = (p, q) => () => router.go(p, q);
  registerCommands(() => [
    ...ROUTES.filter((r) => !r.hidden).map((r) => ({ label: t('palette.goto', { name: t(`nav.${r.nav}`) }), run: goto(r.path) })),
    { label: t('palette.new_skill'), run: goto('/edit/skill/new-skill', { create: '1' }) },
    { label: t('palette.make_plan'), run: goto('/changes', { tab: 'plan' }) },
    { label: t('palette.toggle_theme'), run: () => toggleTheme() },
    { label: t('palette.set_key'), run: () => promptKey() },
  ]);
  // Every item is a palette entry ("code-review", "Claude Code", ...), fetched lazily each time the palette opens.
  registerCommands(async () => {
    const lists = await Promise.all(KINDS.map((k) => api.items(k).catch(() => [])));
    const cl = await api.clients().catch(() => []);
    return [
      ...lists.flatMap((l, i) => l.filter((it) => !it.locked).map((it) => ({ label: it.name, hint: t(`kind.${KINDS[i]}`), run: goto(`/edit/${KINDS[i]}/${encodeURIComponent(it.name)}`) }))),
      ...cl.map((c) => ({ label: c.display_name, hint: t('nav.clients'), run: goto(`/clients/${encodeURIComponent(c.id)}`) })),
    ];
  });
  router = createRouter({ routes: ROUTES, onNavigate: navigate });
  router.start();
}
boot();
