// "Import into hub..." for observed skills (F8/F11): per-item name, on_conflict, enable (OFF by default, with the reason), an override
// reason that appears only when some item's gate is `blocked`, then a results table. Nothing is sent until the form is valid.
import { h, clear, append } from '../dom.js';
import { t } from '../i18n.js';
import { openModal } from './dialog.js';
import { btn, badge, field } from './ui.js';
import { toastError } from './toast.js';
import { gateBadge } from './eval-parts.js';
import { CONFLICTS, importDraft, needsOverride, validateImport, importBody, importResults, importTally, IMPORT_TONE, OVERRIDE_MIN, OVERRIDE_MAX, gateStatus } from '../evalview.js';

// rows: selected evaluation rows of ONE client. gateMode: off|advisory|require|null (settings, best effort).
export function openImportDialog({ api, client, clientName, rows, gateMode, onDone }) {
  const items = importDraft(rows);
  const body = h('div', { class: 'stack-v' });
  const m = openModal(t('ev.imp_title', { client: clientName || client }), body, { onClose: () => { if (done) onDone?.(); } });
  let done = false;

  const nameInputs = new Map(); const whyBoxes = new Map();
  const conflict = h('select', { id: 'imp-conflict' }, CONFLICTS.map((c) => h('option', { value: c }, t(`ev.imp_conflict_${c}`))));
  const conflictHint = h('div', { class: 'hint', id: 'imp-conflict-h' });
  const paintHint = () => { conflictHint.textContent = t(`ev.imp_conflict_${conflict.value}_hint`); };
  conflict.addEventListener('change', paintHint); conflict.setAttribute('aria-describedby', 'imp-conflict-h'); paintHint();
  const enable = h('input', { type: 'checkbox', id: 'imp-enable', 'aria-describedby': 'imp-enable-h' });
  const reason = h('textarea', { id: 'imp-reason', rows: 3, maxlength: OVERRIDE_MAX, 'aria-describedby': 'imp-reason-h', onInput: () => { err.textContent = ''; reason.removeAttribute('aria-invalid'); } });
  const reasonBox = h('div', { class: 'field ev-override' }, h('label', { for: 'imp-reason' }, t('ev.imp_reason')), reason,
    h('div', { class: 'hint', id: 'imp-reason-h' }, t('ev.imp_reason_hint', { min: OVERRIDE_MIN, max: OVERRIDE_MAX })));
  const err = h('div', { class: 'perr', role: 'alert' });

  const syncDraft = () => { for (const it of items) it.finalName = nameInputs.get(it.subject)?.value.trim() ?? it.finalName; reasonBox.hidden = !needsOverride(items); };
  const tbody = h('tbody', items.map((it) => {
    const input = h('input', { type: 'text', id: `imp-n-${it.key}`, value: it.name, spellcheck: 'false', autocomplete: 'off', 'aria-label': t('ev.imp_name_for', { name: it.name }), onInput: () => { err.textContent = ''; } });
    nameInputs.set(it.subject, input);
    const why = h('ul', { class: 'ev-why small', 'aria-label': t('ev.imp_why') }); whyBoxes.set(it.subject, why);
    return h('tr', h('td', h('strong', it.name), h('div', { class: 'muted small' }, it.path), why), h('td', gateBadge(it.gate)), h('td', input));
  }));
  const table = h('div', { class: 'tablewrap' }, h('table', { class: 'compact ev-imp-table' }, h('caption', { class: 'sr-only' }, t('ev.imp_items')),
    h('thead', h('tr', ['ev.col_name', 'ev.col_gate', 'ev.imp_name'].map((k) => h('th', { scope: 'col' }, t(k))))), tbody));
  const missing = items.filter((i) => i.gate === 'missing').length;
  const notes = [
    gateMode ? h('p', { class: 'hint ev-effective' }, t('ev.imp_effective', { mode: t(`ev.mode_${gateMode}`) })) : null,
    gateMode === 'require' && missing ? h('p', { class: 'note warn' }, t('ev.imp_require', { count: missing })) : gateMode === 'off' ? h('p', { class: 'hint' }, t('ev.gate_off_note')) : null,
    h('p', { class: 'hint' }, t('ev.imp_intro', { count: items.length })),
  ];
  const submit = h('button', { type: 'submit', class: 'btn primary' }, t('ev.imp_submit', { count: items.length }));
  const form = h('form', { class: 'stack-v', novalidate: true, onSubmit: async (e) => {
    e.preventDefault(); syncDraft();
    const errs = validateImport(items, { override: reason.value });
    if (errs.length) {
      err.textContent = errs.map((x) => t(x.key, { min: x.min, max: x.max, name: items.find((i) => i.subject === x.subject)?.finalName || '' })).join(' ');
      if (errs.some((x) => x.key === 'ev.imp_err_reason')) reason.setAttribute('aria-invalid', 'true');
      return;
    }
    submit.disabled = true;
    try {
      const resp = await api.importObserved(client, importBody(items, { onConflict: conflict.value, enable: enable.checked, override: reason.value }));
      done = true; showResults(importResults(resp));
    } catch (x) { submit.disabled = false; toastError(x); err.textContent = ''; }
  } },
  ...notes, table,
  h('div', { class: 'form-grid' }, field(t('ev.imp_conflict'), conflict), h('div', { class: 'field' }, h('span', { class: 'lbl' }, t('ev.imp_enable_label')),
    h('label', { class: 'check', for: 'imp-enable' }, enable, h('span', t('ev.imp_enable'))), h('div', { class: 'hint', id: 'imp-enable-h' }, t('ev.imp_enable_hint')))),
  conflictHint, reasonBox, err,
  h('div', { class: 'row gap end' }, btn(t('common.cancel'), { onClick: () => m.close() }), submit));
  reasonBox.hidden = !needsOverride(items);
  body.append(form);

  function showResults(results) {
    const tally = importTally(results);
    clear(body).append(
      h('p', { role: 'status', class: 'ev-imp-summary' }, Object.entries(tally).map(([s, n]) => `${t(`ev.imp_status_${s}`)}: ${n}`).join(' · ') || t('ev.imp_none')),
      h('div', { class: 'tablewrap' }, h('table', { class: 'compact ev-imp-results' }, h('caption', { class: 'sr-only' }, t('ev.imp_results')),
        h('thead', h('tr', ['ev.col_name', 'ev.imp_col_result', 'ev.col_gate', 'ev.imp_col_detail'].map((k) => h('th', { scope: 'col' }, t(k))))),
        h('tbody', results.map((r) => h('tr', h('td', h('strong', r.finalName)), h('td', badge(t(`ev.imp_status_${r.status}`), IMPORT_TONE[r.status] || 'muted')),
          h('td', gateStatus(r.gate) ? gateBadge(r.gate, { withTip: false }) : h('span', { class: 'muted' }, '-')), h('td', { class: 'small' }, detailText(r))))))),
      h('p', { class: 'hint' }, t('ev.imp_after')),
      h('div', { class: 'row gap end' }, h('a', { class: 'btn', href: '#/library' }, t('ev.imp_open_library')), btn(t('common.close'), { kind: 'primary', onClick: () => m.close() })));
  }
  // Why each item is blocked or warned (the gate reasons live on the detail, not on the list row).
  for (const it of items.filter((i) => i.gate === 'blocked' || i.gate === 'warn')) {
    api.evalDetail(it.subject).then((d) => { const box = whyBoxes.get(it.subject); const rs = d?.gate?.reasons || []; if (box) append(box, rs.length ? rs.map((x) => h('li', x)) : [h('li', t('ev.imp_why_none'))]); }).catch(() => {});
  }
  m.el.querySelector('input[type=text]')?.focus();
  return m;
}
const KNOWN = new Set(['eval_required', 'eval_blocked']);
const detailText = (r) => (KNOWN.has(r.code) ? t(`ev.imp_detail_${r.code}`) : r.detail || '');
