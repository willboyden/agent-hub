// Singletons shared by views: store, API client (bearer key from sessionStorage), stream opener.
import { createStore, session, local } from './store.js';
import { createClient, CSRF_HEADER, SAFE_METHODS } from './api.js';
import { openStream } from './sse.js';

export const store = createStore({
  apiKey: session.get('ah.apiKey', ''),
  authRequired: false,
  theme: local.get('ah.theme', 'dark'),
  // Unsaved matrix edits live in the store so they survive navigating away and back (cleared on save/discard).
  staged: {},
  stagedUndo: [],
});

export const api = createClient({
  getKey: () => store.get().apiKey,
  onUnauthorized: () => store.set({ authRequired: true }),
});

export function setApiKey(key) {
  if (key) session.set('ah.apiKey', key); else session.remove('ah.apiKey');
  store.set({ apiKey: key || '', authRequired: false });
}

export function stream(path, opts = {}) {
  const { query, ...rest } = opts;
  return openStream(api.url(path, query), {
    ...rest,
    headers: { ...api.authHeaders(), ...(SAFE_METHODS.has((rest.method || 'GET').toUpperCase()) ? {} : { [CSRF_HEADER]: '1' }), ...(rest.headers || {}) },
    onError: (e) => { if (e?.status === 401 && !String(e?.code || '').startsWith('knowledge_')) store.set({ authRequired: true }); rest.onError?.(e); },
  });
}

export function applyTheme(theme) {
  const eff = theme === 'system' ? (matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark') : theme;
  document.documentElement.dataset.theme = eff;
  local.set('ah.theme', theme);
  store.set({ theme });
}
