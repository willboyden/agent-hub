// Settings > Config files: draft, validate, export. The hub never writes, installs or chmods a config file and runs no
// command for one; this tab only helps the person write a correct file and install it themselves.
//   Load:     a template that ships with the hub, the policy floor (read-only, the only current file served), a local
//             file (read in the browser, never uploaded except to the check), or an empty draft / a paste.
//   Edit:     Form (a typed control and an (i) pop-up per value, config-form.js) and Text (code-area.js), one draft.
//   Validate: POST /config-files/{file}/validate runs the hub's real loader on the draft and returns diagnostics.
//   Export:   the diff against what was loaded, Copy, Download, and the install commands as text.
// Drafts are kept in memory for this tab only (an env draft can hold a secret, so never in browser storage).
import { h, clear, icon, download, copyText, append } from '../dom.js';
import { t } from '../i18n.js';
import { api, onApiKeyDropped } from '../app-context.js';
import { btn, badge, card, errorBox, skeleton, fixBlock, seg } from './ui.js';
import { toast, toastError } from './toast.js';
import { diffView } from './diff-view.js';
import { unifiedText } from '../line-diff.js';
import { codeArea } from './code-area.js';
import { configForm } from './config-form.js';
import { loadFieldHelp, catalogFor } from '../cfg-help.js';
import { shPath, uncheckedResult } from '../cfg-edit.js';
import { paras } from './help-panel.js';

/** Largest file the editor opens (the server refuses bigger drafts too). */
export const MAX_DRAFT = 64 * 1024;
const ID_RE = /^[a-z0-9][a-z0-9._-]{0,63}$/;
export { shq, shPath } from '../cfg-edit.js';
/** The `id:` of a client file draft, when it is a valid id (anything else never reaches a command line). */
export function draftClientId(text) {
  const m = /^id:[ \t]*['"]?([^'"\s#]+)['"]?[ \t]*(?:#.*)?$/m.exec(String(text || ''));
  return m && ID_RE.test(m[1]) ? m[1] : null;
}
/** The file name a draft downloads as. */
export function downloadName(fileId, text) {
  if (fileId === 'hub-env') return 'agent-hub.env';
  if (fileId === 'knowledge') return 'config.yaml';
  if (fileId === 'client-spec') return `${draftClientId(text) || 'client'}.yaml`;
  return 'floor.yaml';
}
/** The install steps for a checked draft: [{label, cmd?}] (text only; nothing is run). `info` is the catalog row. */
export function installSteps(fileId, info, text) {
  const name = downloadName(fileId, text);
  if (fileId === 'hub-env') {
    const p = info?.path || '~/.config/agent-hub/env';
    const q = shPath(p);
    return [
      { label: t('cfgx.inst_backup'), cmd: `test -e ${q} && cp -p ${q} ${shPath(`${p}.bak`)}` },
      { label: t('cfgx.inst_install_secret'), cmd: `install -D -m 600 ${name} ${q}` },
      { label: t('cfgx.inst_restart_hub'), cmd: 'systemctl --user restart agent-hub' },
      { label: t('cfgx.inst_unit_wins') },
    ];
  }
  if (fileId === 'knowledge') {
    return [
      { label: t('cfgx.inst_install_knowledge'), cmd: `install -D -m 600 ${name} ${shPath('~/.config/agent-knowledge/config.yaml')}` },
      { label: t('cfgx.inst_restart_knowledge'), cmd: 'systemctl --user restart agent-knowledge' },
      { label: t('cfgx.inst_knowledge_manual'), cmd: `agent-knowledge --config ${shPath('~/.config/agent-knowledge/config.yaml')}` },
    ];
  }
  if (fileId === 'client-spec') {
    const id = draftClientId(text);
    const dir = info?.path || '~/.local/share/agent-hub/content/clients';
    const target = id ? shPath(`${dir}/${id}.yaml`) : null;
    return [
      id ? { label: t('cfgx.inst_install_client'), cmd: `install -D -m 600 ${name} ${target}` } : { label: t('cfgx.inst_need_id') },
      { label: t('cfgx.inst_commit_client') },
      { label: t('cfgx.inst_wizard') },
    ];
  }
  return [];
}

// Unsaved drafts by file id, for this page load only (memory, never storage).
const drafts = new Map();
let lastFile = null;
// The editors created on this page load ({alive, repaint}); one whose view is gone is dropped when the next is added.
const liveEditors = new Set();
/** Forget every draft (an env draft can hold a secret) and blank any editor on screen: the API key was removed. */
export function clearConfigDrafts() {
  drafts.clear(); lastFile = null;
  for (const e of [...liveEditors]) if (e.alive()) e.repaint(); else liveEditors.delete(e);
}
onApiKeyDropped(clearConfigDrafts);

export function configEditor(view) {
  const root = h('div', { class: 'stack-v cfg' });
  const help = { doc: null, loading: true, failed: false };
  const helpReady = loadFieldHelp().then((d) => { help.doc = d; }, () => { help.failed = true; }).finally(() => { help.loading = false; });
  const st = { files: [], file: null, view: 'form', busy: false };
  const alive = () => view._alive;
  const cur = () => (st.file ? drafts.get(st.file.id) : null);
  for (const e of [...liveEditors]) if (!e.alive()) liveEditors.delete(e);
  liveEditors.add({ alive, repaint: () => { if (st.file) paint(); } });
  const pickerHost = h('div'), aboutHost = h('div', { class: 'stack-v' }), bodyHost = h('div', { class: 'stack-v' });
  root.append(h('p', { class: 'note info' }, icon('shield', 14), ' ', t('cfgx.intro')), pickerHost, aboutHost, bodyHost);

  async function loadList() {
    clear(pickerHost).append(skeleton(1));
    try {
      st.files = await api.configFiles();
      if (!alive()) return;
      const first = st.files.find((f) => f.id === lastFile) || st.files[0];
      clear(pickerHost).append(seg(st.files.map((f) => ({ value: f.id, label: t(`cfgx.file_${f.id.replace('-', '_')}`), icon: f.editable ? null : 'lock' })), first?.id, (id) => choose(id), t('cfgx.pick')));
      if (first) choose(first.id);
    } catch (e) { if (alive()) clear(pickerHost).append(errorBox(e, loadList)); }
  }

  function choose(id) {
    st.file = st.files.find((f) => f.id === id) || null;
    lastFile = id;
    if (!st.file) return;
    const k = st.file.id.replace('-', '_');
    append(clear(aboutHost), [
      paras(t(`cfgx.about_${k}`), 'help-lead'),
      st.file.editable ? h('div', { class: 'note warn cfg-note' }, icon('lock', 14), paras(t(`cfgx.not_served_${k}`))) : h('div', { class: 'note info cfg-note' }, icon('lock', 14), paras(t('cfgx.floor_locked')))]);
    if (!st.file.editable && !cur()) loadCurrent();
    else paint();
  }

  function setDraft(text, { base = text, source }) {
    drafts.set(st.file.id, { base, draft: text, source, result: null, checked: null });
  }

  async function loadTemplate(name) {
    try {
      const r = await api.configTemplate(st.file.id, name);
      if (!alive()) return;
      setDraft(r.text, { source: t('cfgx.src_template', { name: r.path }) });
      paint();
    } catch (e) { toastError(e); }
  }
  async function loadCurrent() {
    clear(bodyHost).append(skeleton(4));
    try {
      const r = await api.configCurrent(st.file.id);
      if (!alive()) return;
      setDraft(r.text, { source: t('cfgx.src_current', { path: r.path }) });
      paint();
    } catch (e) { if (alive()) clear(bodyHost).append(errorBox(e, loadCurrent)); }
  }
  function openLocal(fileObj) {
    if (!fileObj) return;
    if (fileObj.size > MAX_DRAFT) { toast(t('cfgx.too_big', { kb: MAX_DRAFT / 1024 }), { kind: 'bad' }); return; }
    const rd = new FileReader();
    rd.onload = () => { if (!alive()) return; setDraft(String(rd.result || ''), { source: t('cfgx.src_local', { name: fileObj.name }) }); paint(); };
    rd.onerror = () => toast(t('cfgx.read_failed'), { kind: 'bad' });
    rd.readAsText(fileObj);
  }

  /** The load row: templates, open a local file, start empty (editable files only). */
  function loadRow() {
    const f = st.file;
    const tpl = h('select', { id: 'cfgx-tpl', 'aria-label': t('cfgx.template') }, f.templates.map((x) => h('option', { value: x.id }, `${x.id} (${x.path})`)));
    const fileIn = h('input', { type: 'file', class: 'sr-only', id: 'cfgx-file', accept: f.format === 'env' ? '.env,text/plain' : '.yaml,.yml,text/yaml,text/plain', onChange: (e) => { openLocal(e.target.files?.[0]); e.target.value = ''; } });
    return h('div', { class: 'row gap wrap cfg-load' },
      f.templates.length ? [tpl, btn(t('cfgx.load_template'), { icon: 'file', onClick: () => loadTemplate(tpl.value) })] : null,
      fileIn, btn(t('cfgx.open_file'), { icon: 'import', onClick: () => fileIn.click() }),
      btn(t('cfgx.start_empty'), { icon: 'plus', onClick: () => { setDraft('', { source: t('cfgx.src_empty') }); st.view = 'text'; paint(); } }));
  }

  function paint() {
    const f = st.file, d = cur();
    clear(bodyHost);
    if (f.editable) bodyHost.append(card(t('cfgx.load_title'), h('div', { class: 'stack-v' }, h('p', { class: 'hint' }, t('cfgx.load_hint')), loadRow())));
    if (!d) { bodyHost.append(h('p', { class: 'muted' }, t('cfgx.nothing_loaded'))); return; }
    const editable = f.editable;
    let form = null, code = null;
    const editorHost = h('div');
    const diagHost = h('div', { class: 'stack-v' });
    const exportHost = h('div', { class: 'stack-v' });
    const stale = h('p', { class: 'note warn', hidden: true }, t('cfgx.stale'));
    const onText = (v) => { d.draft = v; stale.hidden = !d.result || d.checked === v; paintExport(); };
    const getHelp = () => ({ cat: catalogFor(help.doc, f.id), loading: help.loading, failed: help.failed, file: { locked: editable ? null : t('cfgx.floor_locked') } });
    function showEditor() {
      form?.closePop(); form = null; code = null;
      clear(editorHost);
      if (st.view === 'form') {
        form = configForm({ format: f.format, getText: () => d.draft, setText: (v) => { onText(v); }, baseText: () => d.base, editable,
          onGoToLine: (n) => { st.view = 'text'; viewSeg.querySelector('[data-v=text]')?.click(); setTimeout(() => code?.focusLine(n), 0); }, onSave: () => check(), getHelp });
        editorHost.append(form.el);
        if (!form.build()) { /* unsupported or a syntax error: the box says so */ }
        if (d.result) form.refresh(d.result.diagnostics || []);
        // field help arrives after the first paint: rebuild once so the (i) choices and pop-ups know their entries
        if (help.loading) helpReady.then(() => { if (alive() && form && editorHost.isConnected) { form.build(); if (d.result) form.refresh(d.result.diagnostics || []); } });
      } else {
        code = codeArea({ value: d.draft, label: t('cfgx.text_label', { name: t(`cfgx.file_${f.id.replace('-', '_')}`) }), readOnly: !editable, format: f.format, maxLength: f.max_bytes || 65536, onInput: onText, onSave: () => check() });
        editorHost.append(code.el);
        if (d.result) code.setMarkers(d.result.diagnostics || []);
      }
    }
    const viewSeg = seg([{ value: 'form', label: t('cfgx.view_form'), icon: 'layout' }, { value: 'text', label: t('cfgx.view_text'), icon: 'file' }], st.view, (v) => { st.view = v; showEditor(); }, t('cfgx.view'));
    const checkBtn = editable ? btn(t('cfgx.check'), { icon: 'check', kind: 'primary', onClick: () => check(), title: 'Ctrl S' }) : null;
    const resetBtn = editable ? btn(t('cfgx.reset'), { icon: 'undo', onClick: () => { d.draft = d.base; d.result = null; d.checked = null; paint(); } }) : null;
    bodyHost.append(card(editable ? t('cfgx.draft_title', { source: d.source }) : t('cfgx.view_title', { source: d.source }), h('div', { class: 'stack-v' },
      h('div', { class: 'row gap wrap between cfg-toolbar' }, viewSeg, h('div', { class: 'row gap wrap' }, editable ? badge(t('cfgx.draft_badge'), 'info') : badge(t('cfgx.view_only'), 'muted'), resetBtn, checkBtn)),
      editorHost)));
    if (editable) {
      bodyHost.append(card(t('cfgx.check_title'), h('div', { class: 'stack-v' }, stale, diagHost)));
      bodyHost.append(card(t('cfgx.export_title'), exportHost));
    }
    showEditor();
    paintDiags();
    paintExport();

    async function check() {
      if (!editable || st.busy) return;
      st.busy = true; checkBtn.disabled = true;
      clear(diagHost).append(skeleton(2));
      const text = d.draft;
      try {
        const r = await api.validateConfig(f.id, text);
        if (!alive()) return;
        d.result = r; d.checked = text;
        stale.hidden = d.draft === text;
        paintDiags();
        form?.refresh(r.diagnostics || []);
        code?.setMarkers(r.diagnostics || []);
        if (uncheckedResult(r)) toast(t('cfgx.check_unchecked'), { kind: 'warn' });
        else toast(r.ok ? t('cfgx.check_ok') : t('cfgx.check_bad', { count: (r.diagnostics || []).filter((x) => x.severity === 'error').length }), { kind: r.ok ? 'ok' : 'bad' });
        paintExport();
      } catch (e) { if (alive()) { clear(diagHost).append(errorBox(e, check)); } }
      finally { st.busy = false; if (checkBtn) checkBtn.disabled = false; }
    }

    function paintDiags() {
      if (!editable) return;
      clear(diagHost);
      const r = d.result;
      if (!r) { diagHost.append(h('p', { class: 'muted' }, t('cfgx.not_checked'))); return; }
      const ds = r.diagnostics || [];
      const count = (s) => ds.filter((x) => x.severity === s).length;
      const unchecked = uncheckedResult(r);
      diagHost.append(h('p', { class: `note ${unchecked ? 'warn' : r.ok ? 'ok' : 'bad'}`, role: 'status' }, h('strong', unchecked ? t('cfgx.result_unchecked') : r.ok ? t('cfgx.result_ok') : t('cfgx.result_bad')), ' ',
        t('cfgx.counts', { error: count('error'), warn: count('warn'), info: count('info') })));
      if (ds.length) {
        diagHost.append(h('ul', { class: 'cfg-diags' }, ds.map((x) => h('li', { class: `cfg-diag ${x.severity}` },
          badge(t(`form.sev_${x.severity}`), x.severity === 'error' ? 'bad' : x.severity === 'warn' ? 'warn' : 'info'), ' ',
          x.line ? btn(t('cfgx.line', { n: x.line }), { size: 'sm', kind: 'ghost', onClick: () => { if (form && !form.focusLine(x.line)) { st.view = 'text'; viewSeg.querySelector('[data-v=text]')?.click(); setTimeout(() => code?.focusLine(x.line), 0); } else if (code) code.focusLine(x.line); } }) : null,
          ' ', h('span', x.message)))));
      }
      const sm = r.summary || {};
      const list = sm.settings || sm.keys;
      const more = !sm.settings && Array.isArray(list) && Number.isInteger(sm.key_count) && sm.key_count > list.length ? sm.key_count - list.length : 0;
      if (Array.isArray(list) && list.length) diagHost.append(h('p', { class: 'small muted' }, t(sm.settings ? 'cfgx.sum_settings' : 'cfgx.sum_keys'), ' ', list.map((x, i) => [i ? ', ' : '', h('code', String(x))]), more ? ` ${t('cfgx.sum_more', { n: more })}` : null));
      if (Array.isArray(sm.artifacts) && sm.artifacts.length) diagHost.append(h('div', { class: 'small' }, h('p', { class: 'muted' }, t('cfgx.sum_artifacts')), h('ul', sm.artifacts.map((a) => h('li', h('code', `${a.root}: ${a.path}`))))));
    }

    function paintExport() {
      if (!editable) return;
      clear(exportHost);
      const diff = unifiedText(d.base, d.draft, { from: t('cfgx.diff_from'), to: t('cfgx.diff_to') });
      const name = downloadName(f.id, d.draft);
      const checkedNow = d.result && d.checked === d.draft;
      append(exportHost, [
        !checkedNow ? h('p', { class: 'note warn' }, t('cfgx.export_unchecked'))
          : uncheckedResult(d.result) ? h('p', { class: 'note warn' }, t('cfgx.export_not_checkable'))
            : d.result.ok ? h('p', { class: 'note ok' }, t('cfgx.export_ready')) : h('p', { class: 'note bad' }, t('cfgx.export_errors')),
        h('div', { class: 'row gap wrap' },
          btn(t('cfgx.copy'), { icon: 'copy', onClick: async () => { const ok = await copyText(d.draft); toast(ok ? t('common.copied') : t('common.copy_failed'), { kind: ok ? 'ok' : 'bad' }); } }),
          btn(t('cfgx.download', { name }), { icon: 'download', onClick: () => download(name, d.draft, f.format === 'env' ? 'text/plain' : 'application/yaml') }),
          diff ? btn(t('cfgx.download_diff'), { icon: 'changes', onClick: () => download(`${name}.diff`, diff, 'text/x-diff') }) : null),
        h('h3', { class: 'cfg-sub' }, t('cfgx.diff_title')),
        diff ? diffView(diff, { label: t('cfgx.diff_title') }) : h('p', { class: 'muted small' }, t('cfgx.no_diff')),
        h('h3', { class: 'cfg-sub' }, t('cfgx.install_title')),
        h('p', { class: 'hint' }, t('cfgx.install_hint', { name })),
        h('ol', { class: 'cfg-install' }, installSteps(f.id, f, d.draft).map((s) => h('li', s.cmd ? fixBlock(s.cmd, s.label) : h('span', s.label)))),
        f.id === 'client-spec' ? h('div', { class: 'row gap wrap' }, h('a', { class: 'btn sm', href: '#/changes' }, t('help.open_page', { page: t('nav.changes') })), h('a', { class: 'btn sm', href: '#/clients' }, t('help.open_page', { page: t('nav.clients') }))) : null]);
    }
  }

  loadList();
  return root;
}
