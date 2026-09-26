// Hash router. Pure parsing/matching helpers are exported for unit tests; createRouter binds them to window.
export function parseHash(hash) {
  let s = String(hash || '').replace(/^#/, '');
  if (!s.startsWith('/')) s = '/' + s;
  const qi = s.indexOf('?');
  const path = qi < 0 ? s : s.slice(0, qi);
  const query = {};
  if (qi >= 0) for (const [k, v] of new URLSearchParams(s.slice(qi + 1))) query[k] = v;
  const segs = path.split('/').filter(Boolean);
  return { path: '/' + segs.join('/'), segments: segs, query };
}

export function buildHash(path, query) {
  const q = query ? new URLSearchParams(Object.entries(query).filter(([, v]) => v != null && v !== '')).toString() : '';
  return `#${path}${q ? '?' + q : ''}`;
}

// ':name' matches one segment; ':name*' matches the rest (joined with '/').
export function compileRoute(pattern) {
  const keys = [];
  const parts = pattern.split('/').filter(Boolean).map((seg) => {
    const m = /^:(\w+)(\*)?$/.exec(seg);
    if (!m) return seg.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    keys.push(m[1]);
    return m[2] ? '(.+)' : '([^/]+)';
  });
  return { re: new RegExp('^/' + parts.join('/') + '$'), keys };
}

export function matchRoutes(routes, path) {
  for (const route of routes) {
    const c = route._c || (route._c = compileRoute(route.path));
    const m = c.re.exec(path);
    if (m) {
      const params = {};
      c.keys.forEach((k, i) => { try { params[k] = decodeURIComponent(m[i + 1]); } catch { params[k] = m[i + 1]; } });
      return { route, params };
    }
  }
  return null;
}

export function createRouter({ routes, onNavigate, win = globalThis.window, notFound = null }) {
  const resolve = () => {
    const loc = parseHash(win.location.hash || '#/');
    const hit = matchRoutes(routes, loc.path) || (notFound ? { route: notFound, params: {} } : null);
    onNavigate({ ...loc, ...hit });
  };
  win.addEventListener('hashchange', resolve);
  return {
    start: resolve,
    refresh: resolve,
    go(path, query) { win.location.hash = buildHash(path, query); },
    stop() { win.removeEventListener('hashchange', resolve); },
  };
}
