// Markdown editor: textarea + live preview. The preview goes through renderMarkdown (escapes first) and setTrustedHtml only receives its output.
import { h, setTrustedHtml } from '../dom.js';
import { t } from '../i18n.js';
import { renderMarkdown } from '../markdown.js';
import { seg } from './ui.js';
import { debounce } from '../util.js';

export function mdEditor({ value = '', label, onChange, mode = 'split', id = 'md-body', rows = 22, preview = true }) {
  const ta = h('textarea', { id, class: 'md-input', rows, spellcheck: 'false', 'aria-label': label, value });
  const pv = h('div', { class: 'md md-preview', 'aria-label': t('editor.preview'), tabindex: '0' });
  const paint = () => setTrustedHtml(pv, renderMarkdown(ta.value, { copyLabel: t('common.copy') }));
  const slow = debounce(paint, 120);
  ta.addEventListener('input', () => { onChange?.(ta.value); slow(); });
  // Ctrl+B / Ctrl+I wrap the selection; small helpers, not a full editor.
  ta.addEventListener('keydown', (e) => {
    if (!(e.ctrlKey || e.metaKey) || !['b', 'i'].includes(e.key.toLowerCase())) return;
    e.preventDefault(); const m = e.key.toLowerCase() === 'b' ? '**' : '*'; const { selectionStart: a, selectionEnd: b } = ta;
    ta.setRangeText(`${m}${ta.value.slice(a, b)}${m}`, a, b, 'end'); ta.dispatchEvent(new Event('input'));
  });
  const root = h('div', { class: `md-editor mode-${mode}` }, h('div', { class: 'md-pane' }, ta), preview ? h('div', { class: 'md-pane' }, pv) : null);
  const modes = preview ? seg([{ value: 'edit', label: t('editor.mode_edit') }, { value: 'split', label: t('editor.mode_split') }, { value: 'preview', label: t('editor.mode_preview') }], mode, (m) => { root.className = `md-editor mode-${m}`; }, t('editor.mode')) : null;
  paint();
  return { root, modes, textarea: ta, set(v) { ta.value = v; paint(); }, get: () => ta.value };
}
