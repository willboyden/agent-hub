// Small pure helpers: debounce, relative time, bytes, plain-text helpers.
export function debounce(fn, ms) {
  let id; const d = (...a) => { clearTimeout(id); id = setTimeout(() => fn(...a), ms); }; d.cancel = () => clearTimeout(id); return d;
}
export function timeAgo(epochSeconds, now = Date.now() / 1000) {
  if (!epochSeconds) return '';
  const s = Math.max(0, now - epochSeconds);
  if (s < 60) return { n: Math.round(s), u: 's' };
  if (s < 3600) return { n: Math.round(s / 60), u: 'm' };
  if (s < 86400) return { n: Math.round(s / 3600), u: 'h' };
  return { n: Math.round(s / 86400), u: 'd' };
}
export function fmtBytes(n) {
  if (!Number.isFinite(n)) return '';
  const u = ['B', 'KiB', 'MiB', 'GiB', 'TiB']; let i = 0, v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v >= 10 || i === 0 ? Math.round(v) : v.toFixed(1)} ${u[i]}`;
}
export const NAME_RE = /^[a-z0-9][a-z0-9._-]{0,63}$/;
export const validName = (s) => NAME_RE.test(String(s ?? ''));
// Suggest a valid id from a free-text title.
export const slug = (s) => String(s ?? '').toLowerCase().replace(/[^a-z0-9._-]+/g, '-').replace(/^[^a-z0-9]+/, '').replace(/-+$/, '').slice(0, 64);
// Text colour with the higher WCAG contrast on a hex background (client dots carry their initial in this colour).
export function readableText(hex) {
  const m = /^#?([0-9a-f]{6})$/i.exec(String(hex ?? '')); if (!m) return '#0b0f14';
  const [r, g, b] = [0, 2, 4].map((i) => parseInt(m[1].slice(i, i + 2), 16) / 255).map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  const L = 0.2126 * r + 0.7152 * g + 0.0722 * b;
  const dark = (L + 0.05) / (0.0043 + 0.05), light = 1.05 / (L + 0.05);   // vs #0b0f14 (L~0.0043) and white
  return dark >= light ? '#0b0f14' : '#ffffff';
}
