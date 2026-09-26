// Unified diff renderer. Builds one element per line with textContent only (no innerHTML), so file content can never inject markup.
// Add/remove lines carry a +/- sign column as well as colour, so the meaning does not depend on colour alone.
import { h } from '../dom.js';
import { t } from '../i18n.js';
import { parseUnified, diffStats } from '../diff.js';

const SIGN = { add: '+', del: '-', ctx: ' ', hunk: '', meta: '', note: '' };
export function diffView(text, { label } = {}) {
  const lines = parseUnified(text);
  if (!lines.length) return h('p', { class: 'muted small' }, t('diff.none'));
  const st = diffStats(text);
  const box = h('div', { class: 'diff', role: 'group', 'aria-label': label || t('diff.label', { add: st.add, del: st.del }) });
  for (const l of lines) {
    box.append(h('div', { class: `dl ${l.type}` },
      h('span', { class: 'ln', 'aria-hidden': 'true' }, l.oldNo ?? ''), h('span', { class: 'ln', 'aria-hidden': 'true' }, l.newNo ?? ''),
      h('span', { class: 'sign', 'aria-label': l.type === 'add' ? t('diff.added') : l.type === 'del' ? t('diff.removed') : null }, SIGN[l.type]), h('span', { class: 'code' }, l.text)));
  }
  return box;
}
export const statsText = (text) => { const s = diffStats(text); return t('diff.stats', { add: s.add, del: s.del }); };
