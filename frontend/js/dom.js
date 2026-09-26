// Safe DOM construction: text goes through textContent/createTextNode, never innerHTML.
const PROPS = new Set(['value', 'checked', 'selected', 'indeterminate', 'disabled', 'open']);
export function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  if (attrs && (typeof attrs !== 'object' || attrs instanceof Node || Array.isArray(attrs))) { children.unshift(attrs); attrs = null; }
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'dataset') Object.assign(el.dataset, v);
    else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2).toLowerCase(), v);
    else if (PROPS.has(k)) el[k] = v;
    else el.setAttribute(k, v === true ? '' : String(v));
  }
  append(el, children);
  return el;
}
export function append(el, children) {
  for (const c of children.flat(Infinity)) {
    if (c == null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}
export const clear = (el) => { el.replaceChildren(); return el; };
export const $ = (sel, root = document) => root.querySelector(sel);
export const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

// SVG icons are static path data (trusted constants), 24x24 stroke icons.
const ICONS = {
  dashboard: 'M3 3h7v9H3zM14 3h7v5h-7zM14 12h7v9h-7zM3 16h7v5H3z',
  models: 'M12 2l9 5v10l-9 5-9-5V7zM3 7l9 5 9-5M12 12v10',
  downloads: 'M12 3v12M7 10l5 5 5-5M4 20h16',
  launch: 'M5 19c0-4 3-7 7-9 3-1 6-1 7-5-4 1-6 1-9 4-3 3-4 6-5 10zM9 15l-4 4M14 10a1 1 0 100 .01',
  instances: 'M3 4h18v6H3zM3 14h18v6H3zM7 7h.01M7 17h.01',
  metrics: 'M3 20V4M3 20h18M7 15l4-5 3 3 5-7',
  bench: 'M12 14l4-6M4 18a9 9 0 1116 0z',
  chat: 'M4 5h16v11H9l-5 4z',
  arena: 'M4 20L20 4M14 4h6v6M4 4l16 16M4 14v6h6',
  usage: 'M4 20V10M10 20V4M16 20v-7M22 20H2',
  settings: 'M12 15a3 3 0 100-6 3 3 0 000 6zM19 12l2-1-2-4-2 1-2-1-1-2H9L8 7 6 8 4 7 2 11l2 1v0l-2 1 2 4 2-1 2 1 1 2h4l1-2 2-1 2 1 2-4z',
  search: 'M11 4a7 7 0 100 14 7 7 0 000-14zM21 21l-5-5',
  sun: 'M12 8a4 4 0 100 8 4 4 0 000-8zM12 2v2M12 20v2M2 12h2M20 12h2M5 5l1.5 1.5M17.5 17.5L19 19M5 19l1.5-1.5M17.5 6.5L19 5',
  moon: 'M20 14A8 8 0 1110 4a7 7 0 0010 10z',
  key: 'M14 10a4 4 0 11-3 4l-8 0v-3h2v-2h2l2-2M17 8h.01',
  close: 'M6 6l12 12M18 6L6 18',
  play: 'M7 4l13 8-13 8z', stop: 'M6 6h12v12H6z', pause: 'M8 5v14M16 5v14',
  restart: 'M4 12a8 8 0 108-8M4 4v6h6',
  trash: 'M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13',
  copy: 'M9 9h11v11H9zM5 15V4h11',
  pin: 'M12 17v5M8 3h8l-1 7 3 3H6l3-3z',
  send: 'M4 12l16-8-6 16-3-7z',
  image: 'M3 5h18v14H3zM3 16l5-5 4 4 3-3 6 6M8 9h.01',
  external: 'M14 4h6v6M20 4l-9 9M18 14v6H4V6h6',
  warn: 'M12 3l10 18H2zM12 10v5M12 18h.01',
  check: 'M4 12l5 5 11-11',
  download: 'M12 3v12M7 10l5 5 5-5M4 20h16',
  plus: 'M12 5v14M5 12h14',
  menu: 'M4 6h16M4 12h16M4 18h16',
  // Agent Hub additions (24x24 stroke paths)
  home: 'M3 11l9-8 9 8M5 10v10h14V10M10 20v-6h4v6',
  library: 'M4 4h4v16H4zM10 4h4v16h-4zM16 6l4-1 3 15-4 1z',
  collections: 'M3 6a2 2 0 012-2h4l2 2h8a2 2 0 012 2v9a2 2 0 01-2 2H5a2 2 0 01-2-2z',
  matrix: 'M3 3h18v18H3zM3 9h18M3 15h18M9 3v18M15 3v18',
  editor: 'M4 20h4L19 9l-4-4L4 16zM14 6l4 4',
  clients: 'M4 5h16v10H4zM8 19h8M12 15v4',
  import: 'M12 3v12M7 10l5 5 5-5M4 20h16',
  changes: 'M6 3v12M6 19a2 2 0 100-.01M18 5a2 2 0 100-.01M18 7v4a4 4 0 01-4 4H9',
  inbox: 'M3 13l3-8h12l3 8v6H3zM3 13h5l1 3h6l1-3h5',
  knowledge: 'M12 3c5 0 8 1.5 8 3.5S17 10 12 10 4 8.5 4 6.5 7 3 12 3zM4 6.5v5C4 13.5 7 15 12 15s8-1.5 8-3.5v-5M4 11.5v5C4 18.5 7 20 12 20s8-1.5 8-3.5v-5',
  folder: 'M3 6a2 2 0 012-2h4l2 2h8a2 2 0 012 2v9a2 2 0 01-2 2H5a2 2 0 01-2-2z',
  star: 'M12 3l2.8 5.7 6.2.9-4.5 4.4 1 6.2L12 17.3 6.5 20.2l1-6.2L3 9.6l6.2-.9z',
  branch: 'M6 3v12M6 19a2 2 0 100-.01M18 6a2 2 0 100-.01M18 8c0 5-6 4-12 7',
  server: 'M3 4h18v6H3zM3 14h18v6H3zM7 7h.01M7 17h.01',
  shield: 'M12 3l8 3v6c0 5-4 8-8 9-4-1-8-4-8-9V6z',
  film: 'M4 4h16v16H4zM8 4v16M16 4v16M4 9h4M4 15h4M16 9h4M16 15h4',
  layout: 'M3 4h18v16H3zM3 9h18M9 9v11',
  terminal: 'M4 5h16v14H4zM8 10l3 2-3 2M13 15h3',
  box: 'M12 2l9 5v10l-9 5-9-5V7zM3 7l9 5 9-5M12 12v10',
  cursor: 'M5 3l14 8-6 2-2 6z',
  lock: 'M6 11h12v9H6zM8 11V8a4 4 0 018 0v3',
  grip: 'M9 6h.01M9 12h.01M9 18h.01M15 6h.01M15 12h.01M15 18h.01',
  up: 'M12 19V5M6 11l6-6 6 6',
  down: 'M12 5v14M6 13l6 6 6-6',
  chevron: 'M9 6l6 6-6 6',
  chevdown: 'M6 9l6 6 6-6',
  filter: 'M3 5h18l-7 8v6l-4-2v-4z',
  tag: 'M3 12V4h8l9 9-8 8zM8 8h.01',
  undo: 'M9 14L4 9l5-5M4 9h10a6 6 0 010 12h-3',
  eye: 'M2 12s4-7 10-7 10 7 10 7-4 7-10 7S2 12 2 12zM12 9a3 3 0 100 6 3 3 0 000-6z',
  info: 'M12 3a9 9 0 100 18 9 9 0 000-18zM12 11v5M12 8h.01',
  grid: 'M4 4h7v7H4zM13 4h7v7h-7zM4 13h7v7H4zM13 13h7v7h-7z',
  list: 'M4 6h16M4 12h16M4 18h16',
  play2: 'M5 4l14 8-14 8z',
  minus: 'M5 12h14',
  edit2: 'M4 20h4L19 9l-4-4L4 16z',
  file: 'M6 3h9l4 4v14H6zM14 3v5h5',
  history: 'M4 12a8 8 0 108-8M4 4v6h6M12 8v5l3 2',
  drag: 'M8 9l4-4 4 4M8 15l4 4 4-4',
};
export function icon(name, size = 18) {
  const ns = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(ns, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24'); svg.setAttribute('width', size); svg.setAttribute('height', size);
  svg.setAttribute('fill', 'none'); svg.setAttribute('stroke', 'currentColor'); svg.setAttribute('stroke-width', '1.8');
  svg.setAttribute('stroke-linecap', 'round'); svg.setAttribute('stroke-linejoin', 'round');
  svg.setAttribute('aria-hidden', 'true'); svg.classList.add('icon');
  const p = document.createElementNS(ns, 'path'); p.setAttribute('d', ICONS[name] || ICONS.dashboard);
  svg.append(p);
  return svg;
}
// Only for markup produced by markdown.js / charts.js (both escape all dynamic text).
export function setTrustedHtml(el, html) { el.innerHTML = html; return el; }

export function download(filename, text, type = 'text/plain') {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const a = h('a', { href: url, download: filename });
  document.body.append(a); a.click(); a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
export async function copyText(text) {
  try { await navigator.clipboard.writeText(text); return true; } catch {
    const ta = h('textarea', { value: text, class: 'sr-only' });
    document.body.append(ta); ta.select();
    let ok = false; try { ok = document.execCommand('copy'); } catch { /* ignore */ }
    ta.remove(); return ok;
  }
}
