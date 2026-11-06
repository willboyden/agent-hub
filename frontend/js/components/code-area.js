// A plain textarea with a line-number gutter that marks the lines with problems. No contenteditable, no HTML from the
// file: the gutter holds numbers only, and the text stays in a native textarea (undo, IME, screen readers all work).
//   Tab / Shift+Tab indent and outdent (two spaces); Esc then Tab leaves the editor; Enter keeps the indentation;
//   Ctrl/Cmd+S calls onSave (the Check button) instead of the browser's "save page".
import { h, clear } from '../dom.js';
import { t } from '../i18n.js';

const INDENT = '  ';

/** Insert text at the selection with the browser's own editing command, so Ctrl+Z still undoes it. */
function insert(ta, text) {
  ta.focus();
  if (!document.execCommand?.('insertText', false, text)) {
    ta.setRangeText(text, ta.selectionStart, ta.selectionEnd, 'end');
    ta.dispatchEvent(new Event('input', { bubbles: true }));
  }
}

export function codeArea({ value = '', label, readOnly = false, onInput, onSave, format, maxLength = null } = {}) {
  const gutter = h('div', { class: 'code-gutter', 'aria-hidden': 'true' });
  const ta = h('textarea', {
    class: 'code-edit',
    rows: 22,
    spellcheck: 'false',
    autocomplete: 'off',
    autocapitalize: 'off',
    wrap: 'off',
    'aria-label': label,
    readonly: readOnly ? true : null,
    maxlength: maxLength,
    value,
  });
  const el = h('div', { class: `code-area${readOnly ? ' ro' : ''}` }, gutter, ta);
  let lines = 0,
    marked = [],
    escaped = false;

  function renderGutter() {
    const n = ta.value.split('\n').length;
    if (n === lines) return;
    lines = n;
    clear(gutter);
    const frag = document.createDocumentFragment();
    for (let i = 1; i <= n; i++) frag.append(h('div', { class: 'gl' }, String(i)));
    frag.append(h('div', { class: 'gl-pad' }));
    gutter.append(frag);
    marked = [];
  }
  const sync = () => {
    gutter.scrollTop = ta.scrollTop;
  };

  /** diagnostics: [{severity, line, message}]: the worst severity per line colours its number; the title lists them. */
  function setMarkers(diags) {
    renderGutter();
    for (const g of marked) {
      g.classList.remove('err', 'warn', 'info');
      g.removeAttribute('title');
    }
    marked = [];
    const byLine = new Map();
    for (const d of diags || []) if (d.line) (byLine.get(d.line) || byLine.set(d.line, []).get(d.line)).push(d);
    for (const [line, ds] of byLine) {
      const g = gutter.children[line - 1];
      if (!g || !g.classList.contains('gl')) continue;
      const worst = ds.some((d) => d.severity === 'error')
        ? 'err'
        : ds.some((d) => d.severity === 'warn')
          ? 'warn'
          : 'info';
      g.classList.add(worst);
      g.title = ds.map((d) => d.message).join('\n');
      marked.push(g);
    }
  }

  const lineHeight = () => parseFloat(getComputedStyle(ta).lineHeight) || 19;
  /** Select line n (1-based) and scroll it into the middle of the editor. */
  function focusLine(n) {
    const v = ta.value;
    let s = 0;
    for (let i = 1; i < n; i++) {
      const k = v.indexOf('\n', s);
      if (k < 0) break;
      s = k + 1;
    }
    let e = v.indexOf('\n', s);
    if (e < 0) e = v.length;
    ta.focus({ preventScroll: true });
    ta.setSelectionRange(s, e);
    ta.scrollTop = Math.max(0, (n - 1) * lineHeight() - ta.clientHeight / 2 + lineHeight());
    sync();
  }

  function lineBounds() {
    const v = ta.value,
      a = ta.selectionStart,
      b = ta.selectionEnd;
    const s = v.lastIndexOf('\n', a - 1) + 1;
    let e = v.indexOf('\n', b > a && v[b - 1] === '\n' ? b - 1 : b);
    if (e < 0) e = v.length;
    return { v, a, b, s, e };
  }
  function indent(out) {
    const { v, a, b, s, e } = lineBounds();
    if (!out && a === b) {
      insert(ta, INDENT);
      return;
    }
    const block = v.slice(s, e).split('\n');
    const next = block.map((l) => (out ? l.replace(/^ {1,2}/, '') : l ? INDENT + l : l)).join('\n');
    if (next === v.slice(s, e)) return;
    ta.setSelectionRange(s, e);
    insert(ta, next);
    ta.setSelectionRange(s, s + next.length);
  }
  function newline() {
    const { v, a } = lineBounds();
    const s = v.lastIndexOf('\n', a - 1) + 1;
    const cur = v.slice(s, a);
    let lead = /^[ ]*/.exec(cur)[0];
    if (format === 'yaml' && /:\s*(?:#.*)?$/.test(cur)) lead += INDENT; // `key:` opens a block
    insert(ta, `\n${lead}`);
  }

  if (!readOnly) {
    ta.addEventListener('keydown', (e) => {
      if ((e.ctrlKey || e.metaKey) && (e.key === 's' || e.key === 'S')) {
        e.preventDefault();
        onSave?.();
        return;
      }
      if (e.key === 'Escape') {
        escaped = true;
        return;
      }
      if (e.key === 'Tab' && !escaped && !e.ctrlKey && !e.altKey && !e.metaKey) {
        e.preventDefault();
        indent(e.shiftKey);
        return;
      }
      if (e.key === 'Enter' && !e.shiftKey && !e.ctrlKey && !e.altKey && !e.metaKey && !e.isComposing) {
        e.preventDefault();
        newline();
      }
      escaped = false;
    });
    ta.addEventListener('input', () => {
      renderGutter();
      sync();
      onInput?.(ta.value);
    });
  } else {
    ta.addEventListener('keydown', (e) => {
      if ((e.ctrlKey || e.metaKey) && (e.key === 's' || e.key === 'S')) e.preventDefault();
    });
  }
  ta.addEventListener('scroll', sync, { passive: true });
  renderGutter();
  const hint = readOnly ? null : h('p', { class: 'hint code-hint' }, t('cfgx.keys_hint'));
  return {
    el: h('div', { class: 'stack-v code-wrap' }, el, hint),
    ta,
    setMarkers,
    focusLine,
    setValue(v) {
      if (ta.value !== v) {
        ta.value = v;
        renderGutter();
        sync();
      }
    },
  };
}
