// Knowledge: namespaces, backend health, ingest, query playground, per-client scoped tokens, shared index registry.
// Everything goes through /api/v1/knowledge/* on the hub (same-origin); the browser never talks to the knowledge service directly.
import { AhView } from '../components/base.js';
import { h, icon, clear } from '../dom.js';
import { t } from '../i18n.js';
import { toast, toastError } from '../components/toast.js';
import { btn, badge, tabs, emptyBox, errorBox, field, select, seg, bar, notifyChanged, skeleton, timeText } from '../components/ui.js';
import { confirmDialog, openModal } from '../components/dialog.js';
import { copyText } from '../dom.js';
import { fmtBytes, timeAgo, validName } from '../util.js';

const KEY_CMD = 'echo "HUB_KNOWLEDGE_TOKEN=$(cat ~/.local/share/agent-knowledge/admin.key)" >> ~/.config/agent-hub/env && systemctl --user restart agent-hub';

class AhKnowledge extends AhView {
  setup() {
    this.tab = this.query?.tab || 'namespaces';
    this.head = h('div'); this.panel = h('div', { class: 'k-panel', role: 'tabpanel' });
    const tabBar = tabs(['namespaces', 'ingest', 'query', 'tokens', 'indexes'].map((id) => ({ id, label: t(`k.tab_${id}`) })), this.tab, (id) => { this.tab = id; this.paint(); }, t('editor.tabs'));
    this.append(h('div', { class: 'page-head' }, h('h1', t('k.title'))), h('p', { class: 'muted' }, t('k.intro')), this.head, tabBar, this.panel);
    this.loadHealth(); this.paint();
  }
  // Setup panels for the two knowledge-side failures. They are not the hub's own auth problem, so no key dialog: just say what to do.
  setupPanel(kind) {
    const code = (txt) => h('div', { class: 'row gap wrap' }, h('code', { class: 'ro grow' }, txt), btn(t('common.copy'), { icon: 'copy', size: 'sm', onClick: async () => toast((await copyText(txt)) ? t('common.copied') : t('common.copy_failed'), { kind: 'ok' }) }));
    const retry = btn(t('common.retry'), { icon: 'restart', onClick: () => { this.loadHealth(); this.paint(); } });
    if (kind === 'unauthorized') return emptyBox(t('k.auth_title'), t('k.auth_hint'), h('div', { class: 'stack-v k-down' }, code(KEY_CMD), h('p', { class: 'hint' }, t('k.auth_after')), retry));
    return emptyBox(t('k.down_title'), t('k.down_hint'), h('div', { class: 'stack-v k-down' }, code('cd hub && make run-knowledge'), h('p', { class: 'hint' }, t('k.down_systemd')), code('systemctl --user start agent-knowledge'), retry));
  }
  chip(state) { return h('p', { class: 'k-status' }, badge(t(`k.chip_${state}`), state === 'ok' ? 'ok' : state === 'down' ? 'bad' : 'warn'), ' ', h('span', { class: 'muted small' }, t(`k.chip_${state}_hint`))); }
  async loadHealth() {
    clear(this.head).append(skeleton(1));
    let state = 'ok'; let health = null;
    try { health = await this.api.kHealth(); } catch (e) { if (e.isKnowledgeSetup) health = { reachable: e.code !== 'knowledge_unavailable', authenticated: false }; else if (![404, 405].includes(e.status)) { clear(this.head).append(errorBox(e, () => this.loadHealth())); return; } }
    if (health && 'reachable' in health) state = !health.reachable ? 'down' : !health.authenticated ? 'auth' : 'ok';
    if (state !== 'ok') { this.panel.hidden = true; clear(this.head).append(this.chip(state), this.setupPanel(state === 'down' ? 'down' : 'unauthorized')); return; }
    try {
      const b = await this.api.kBackends(); this.backends = b.items || b; this.panel.hidden = false;
      clear(this.head).append(this.chip('ok'), h('div', { class: 'grid tiles backends' }, this.backends.map((x) => this.backendCard(x))));
    } catch (e) {
      if (e.isKnowledgeSetup || [0, 502, 503].includes(e.status)) { const st = e.code === 'knowledge_upstream_unauthorized' ? 'auth' : 'down'; this.panel.hidden = true; clear(this.head).append(this.chip(st), this.setupPanel(st === 'auth' ? 'unauthorized' : 'down')); return; }
      clear(this.head).append(errorBox(e, () => this.loadHealth()));
    }
  }
  backendCard(b) {
    const total = this.backends.reduce((n, x) => n + (x.disk_bytes || 0), 0) || 1;
    const pct = Math.round(((b.disk_bytes || 0) / total) * 100);
    return h('section', { class: 'card backend-card', 'aria-label': b.name },
      h('header', { class: 'card-head' }, h('h2', b.name), badge(b.ok ? t('k.healthy') : t('k.down'), b.ok ? 'ok' : 'bad')),
      h('div', { class: 'card-body' }, h('p', { class: 'muted small' }, b.kind === 'embedded' ? t('k.embedded', { path: b.path }) : t('k.service', { url: b.url }), b.version ? ` · v${b.version}` : ''),
        (b.dims || []).length ? h('p', { class: 'small' }, t('k.dims', { dims: b.dims.join(', ') })) : null, b.tables != null ? h('p', { class: 'small' }, t('k.tables', { count: b.tables })) : null, bar(pct, t('k.disk_share', { name: b.name, pct, size: fmtBytes(b.disk_bytes || 0) })), h('p', { class: 'muted small' }, t('k.disk_share', { name: b.name, pct, size: fmtBytes(b.disk_bytes || 0) }))));
  }
  paint() { this.loadInto(this.panel, async () => { try { return await this[`${this.tab}Tab`](); } catch (e) { if (e.isKnowledgeSetup) return this.setupPanel(e.code === 'knowledge_upstream_unauthorized' ? 'unauthorized' : 'down'); throw e; } }); }
  async namespaces() { this.ns = await this.api.kNamespaces(); return this.ns; }

  // ---- namespaces ----
  async namespacesTab() {
    const ns = await this.namespaces();
    const create = h('div', { class: 'card' }, h('header', { class: 'card-head' }, h('h2', t('k.ns_create'))), h('div', { class: 'card-body' }, this.nsForm()));
    return h('div', { class: 'stack-v' }, ns.length ? h('div', { class: 'tablewrap' }, h('table', { class: 'compact' }, h('thead', h('tr', ['k.c_name', 'k.c_backend', 'k.c_model', 'k.c_dim', 'k.c_docs', 'k.c_disk', ''].map((k) => h('th', { scope: 'col' }, k ? t(k) : h('span', { class: 'sr-only' }, t('common.delete')))))),
      h('tbody', ns.map((n) => h('tr', h('td', h('strong', n.name), n.description ? h('div', { class: 'muted small' }, n.description) : null), h('td', badge(n.backend, n.backend === 'qdrant' ? 'info' : 'muted')), h('td', n.embedding_model), h('td', { class: 'num' }, String(n.dim)), h('td', { class: 'num' }, n.count.toLocaleString('en-US')), h('td', { class: 'num nowrap' }, fmtBytes(n.disk_bytes)),
        h('td', { class: 'row' }, btn('', { icon: 'trash', kind: 'ghost', size: 'sm', title: t('k.ns_delete', { name: n.name }), onClick: () => this.delNs(n) }))))))) : emptyBox(t('k.ns_empty'), t('k.ns_empty_hint')), create);
  }
  nsForm() {
    const name = h('input', { type: 'text', id: 'ns-name', autocomplete: 'off', placeholder: 'my-docs' }), model = h('input', { type: 'text', id: 'ns-model', value: 'bge-m3' }), dim = h('input', { type: 'number', id: 'ns-dim', value: 1024, min: 1, max: 8192 }), desc = h('input', { type: 'text', id: 'ns-desc' });
    let backend = 'qdrant'; const err = h('div', { class: 'perr', role: 'alert' });
    const be = seg([{ value: 'qdrant', label: 'Qdrant' }, { value: 'lancedb', label: 'LanceDB' }], backend, (v) => { backend = v; }, t('k.c_backend'));
    return h('form', { class: 'stack-v', onSubmit: async (e) => { e.preventDefault(); err.textContent = ''; if (!validName(name.value)) { err.textContent = t('new.bad_name'); return; }
      try { await this.api.kCreateNamespace({ name: name.value, backend, embedding_model: model.value, dim: Number(dim.value), description: desc.value }); toast(t('k.ns_created', { name: name.value }), { kind: 'ok' }); notifyChanged(); this.loadHealth(); this.paint(); } catch (x) { err.textContent = x.message; toastError(x); } } },
    h('div', { class: 'form-grid' }, field(t('k.c_name'), name), h('div', { class: 'field' }, h('span', { class: 'lbl' }, t('k.c_backend')), be), field(t('k.c_model'), model, t('k.model_hint')), field(t('k.c_dim'), dim), field(t('coll.f_desc'), desc)), err,
    h('div', btn(t('k.ns_create'), { icon: 'plus', kind: 'primary', type: 'submit' })));
  }
  async delNs(n) {
    if (!(await confirmDialog(t('k.ns_delete_confirm', { name: n.name, count: n.count }), { danger: true, confirmLabel: t('common.delete') }))) return;
    try { await this.api.kDeleteNamespace(n.name); toast(t('k.ns_deleted', { name: n.name }), { kind: 'ok' }); this.loadHealth(); this.paint(); } catch (e) { toastError(e); }
  }

  // ---- ingest ----
  async ingestTab() {
    const ns = await this.namespaces();
    if (!ns.length) return emptyBox(t('k.ns_empty'), t('k.ns_empty_hint'));
    let mode = 'text';
    const nsSel = select(ns.map((n) => ({ value: n.name, label: `${n.name} (${n.backend})` })), ns[0].name, null, { id: 'ing-ns' });
    const text = h('textarea', { id: 'ing-text', rows: 8, placeholder: t('k.ing_text_ph') }), meta = h('input', { type: 'text', id: 'ing-meta', placeholder: '{"path": "notes/a.md"}' });
    const path = h('input', { type: 'text', id: 'ing-path', placeholder: '~/work/my-repo/docs' }), glob = h('input', { type: 'text', id: 'ing-glob', value: '**/*.md' });
    const textBox = h('div', { class: 'stack-v' }, field(t('k.ing_text'), text), field(t('k.ing_meta'), meta, t('k.ing_meta_hint')));
    const pathBox = h('div', { class: 'stack-v', hidden: true }, field(t('k.ing_path'), path, t('k.ing_path_hint')), field(t('k.ing_glob'), glob));
    const modeSeg = seg([{ value: 'text', label: t('k.ing_mode_text') }, { value: 'path', label: t('k.ing_mode_path') }], mode, (v) => { mode = v; textBox.hidden = v !== 'text'; pathBox.hidden = v !== 'path'; }, t('k.ing_mode'));
    const out = h('div', { class: 'stack-v', role: 'status', 'aria-live': 'polite' }); const err = h('div', { class: 'perr', role: 'alert' });
    const jobsHost = h('div');
    const form = h('form', { class: 'stack-v', onSubmit: async (e) => { e.preventDefault(); err.textContent = '';
      let body;
      if (mode === 'text') { if (!text.value.trim()) { err.textContent = t('k.ing_need_text'); return; } let m = {}; if (meta.value.trim()) { try { m = JSON.parse(meta.value); } catch { err.textContent = t('k.ing_meta_bad'); return; } } body = { documents: [{ text: text.value, metadata: m }] }; }
      else { if (!path.value.trim()) { err.textContent = t('k.ing_need_path'); return; } body = { path: path.value.trim(), glob: glob.value.trim() || '**/*' }; }
      try { const job = await this.api.kIngest(nsSel.value, body); this.watch(job, out); } catch (x) { err.textContent = x.message; toastError(x); } } },
    field(t('k.ing_ns'), nsSel), modeSeg, textBox, pathBox, err, h('div', btn(t('k.ingest'), { icon: 'import', kind: 'primary', type: 'submit' })), out);
    this.loadInto(jobsHost, async () => this.jobsTable(await this.api.kJobs()), { skeletonRows: 1 });
    return h('div', { class: 'stack-v' }, h('div', { class: 'card' }, h('div', { class: 'card-body' }, form)), h('h2', { class: 'section' }, t('k.jobs')), jobsHost);
  }
  watch(job, out) {
    const label = () => t('k.job_progress', { done: job.done, total: job.total });
    const pbar = () => bar(job.total ? (job.done / job.total) * 100 : 0, label());
    const paint = () => out.replaceChildren(h('p', h('strong', t(`k.job_${job.state}`)), ' ', h('span', { class: 'muted' }, label())), pbar());
    paint();
    this.stream(`/knowledge/jobs/${encodeURIComponent(job.id)}/stream`, { reconnect: false, onEvent: (ev) => { if (ev.data && typeof ev.data === 'object') { Object.assign(job, ev.data); paint(); if (job.state === 'done') { toast(t('k.job_done', { count: job.total }), { kind: 'ok' }); this.loadHealth(); } } }, onError: (e) => out.replaceChildren(errorBox(e)) });
  }
  jobsTable(jobs) {
    if (!jobs.length) return h('p', { class: 'muted' }, t('k.jobs_none'));
    return h('div', { class: 'tablewrap' }, h('table', { class: 'compact' }, h('thead', h('tr', ['k.j_id', 'k.c_name', 'k.j_state', 'k.j_progress', 'k.j_when'].map((k) => h('th', { scope: 'col' }, t(k))))),
      h('tbody', jobs.slice(0, 10).map((j) => h('tr', h('td', h('code', j.id)), h('td', j.namespace), h('td', badge(t(`k.job_${j.state}`), j.state === 'done' ? 'ok' : j.state === 'failed' ? 'bad' : 'info')), h('td', bar(j.total ? (j.done / j.total) * 100 : 0, t('k.job_progress', { done: j.done, total: j.total }))), h('td', { class: 'muted nowrap' }, timeText(timeAgo(j.created_at))))))));
  }

  // ---- query playground ----
  async queryTab() {
    const ns = await this.namespaces();
    if (!ns.length) return emptyBox(t('k.ns_empty'), t('k.ns_empty_hint'));
    const nsSel = select(ns.map((n) => ({ value: n.name, label: n.name })), ns[0].name, null, { id: 'q-ns' });
    const q = h('input', { type: 'search', id: 'q-text', placeholder: t('k.q_ph'), autocomplete: 'off' }), k = h('input', { type: 'number', id: 'q-k', value: 5, min: 1, max: 50 }), min = h('input', { type: 'number', id: 'q-min', value: 0, min: 0, max: 1, step: 0.05 });
    const out = h('div', { class: 'stack-v', role: 'region', 'aria-live': 'polite', 'aria-label': t('k.q_results') });
    const run = async (e) => { e?.preventDefault(); if (!q.value.trim()) { q.focus(); return; } clear(out).append(skeleton(3));
      try { const r = await this.api.kQuery(nsSel.value, { text: q.value, k: Number(k.value), min_score: Number(min.value) || undefined }); out.replaceChildren(...this.hits(r)); } catch (x) { out.replaceChildren(errorBox(x)); } };
    return h('div', { class: 'stack-v' }, h('form', { class: 'card', onSubmit: run }, h('div', { class: 'card-body' }, h('div', { class: 'form-grid' }, field(t('k.ing_ns'), nsSel), field(t('k.q_text'), q), field(t('k.q_k'), k), field(t('k.q_min'), min, t('k.q_min_hint'))), h('div', btn(t('k.q_run'), { icon: 'search', kind: 'primary', type: 'submit' })))), out);
  }
  hits(r) {
    if (!r.hits.length) return [emptyBox(t('k.q_none'), t('k.q_none_hint'))];
    return [h('p', { class: 'muted small' }, t('k.q_count', { count: r.hits.length, ms: r.took_ms ?? '?' })), ...r.hits.map((x, i) => h('article', { class: 'card hit' }, h('div', { class: 'card-body' },
      h('div', { class: 'row between gap wrap' }, h('strong', `#${i + 1} `, h('code', x.id)), h('span', { class: 'score' }, bar(x.score * 100, t('k.q_score', { score: x.score.toFixed(3) })), h('span', { class: 'num small' }, x.score.toFixed(3)))),
      h('p', { class: 'hit-text' }, x.text), h('dl', { class: 'meta' }, Object.entries(x.metadata || {}).map(([mk, mv]) => h('div', h('dt', mk), h('dd', String(mv))))))))];
  }

  // ---- tokens ----
  async tokensTab() {
    const [tokens, ns] = await Promise.all([this.api.kTokens(), this.namespaces()]);
    return h('div', { class: 'stack-v' }, h('p', { class: 'muted' }, t('k.tok_intro')),
      tokens.length ? h('div', { class: 'tablewrap' }, h('table', { class: 'compact' }, h('thead', h('tr', ['k.t_client', 'k.t_prefix', 'k.t_scopes', 'k.t_used', ''].map((k) => h('th', { scope: 'col' }, k ? t(k) : h('span', { class: 'sr-only' }, t('common.delete')))))),
        h('tbody', tokens.map((x) => h('tr', h('td', h('strong', x.client)), h('td', h('code', `${x.prefix}…`)), h('td', h('div', { class: 'row gap wrap' }, x.scopes.map((s) => badge(`${s.ns}: ${t(`k.mode_${s.mode}`)}`, s.mode === 'write' ? 'warn' : 'muted', t(`k.mode_${s.mode}_tip`))))), h('td', { class: 'muted nowrap' }, x.last_used_at ? timeText(timeAgo(x.last_used_at)) : t('common.never')),
          h('td', { class: 'row' }, btn('', { icon: 'trash', kind: 'ghost', size: 'sm', title: t('k.tok_revoke', { client: x.client }), onClick: async () => { if (await confirmDialog(t('k.tok_revoke_confirm', { client: x.client }), { danger: true, confirmLabel: t('k.revoke') })) { try { await this.api.kDeleteToken(x.id); notifyChanged(); this.paint(); } catch (e) { toastError(e); } } } }))))))) : emptyBox(t('k.tok_none'), t('k.tok_none_hint')),
      h('div', { class: 'card' }, h('header', { class: 'card-head' }, h('h2', t('k.tok_create'))), h('div', { class: 'card-body' }, this.tokenForm(ns))));
  }
  tokenForm(ns) {
    const client = h('input', { type: 'text', id: 'tok-client', autocomplete: 'off', placeholder: 'hermes' }); const err = h('div', { class: 'perr', role: 'alert' });
    const rows = ns.map((n) => { const use = h('input', { type: 'checkbox', id: `tk-${n.name}`, 'aria-label': t('k.tok_use', { name: n.name }) }); const mode = select([{ value: 'read', label: t('k.mode_read') }, { value: 'write', label: t('k.mode_write') }], 'read', null, { 'aria-label': t('k.tok_mode', { name: n.name }) }); return { n, use, mode, node: h('div', { class: 'scope-row' }, h('label', { class: 'check', for: `tk-${n.name}` }, use, h('span', n.name), badge(n.backend, 'muted')), mode) }; });
    return h('form', { class: 'stack-v', onSubmit: async (e) => { e.preventDefault(); err.textContent = ''; const scopes = rows.filter((r) => r.use.checked).map((r) => ({ ns: r.n.name, mode: r.mode.value })); if (!client.value.trim()) { err.textContent = t('k.tok_need_client'); return; } if (!scopes.length) { err.textContent = t('k.tok_need_scope'); return; }
      try { const tok = await this.api.kCreateToken({ client: client.value.trim(), scopes }); this.showSecret(tok.secret, t('k.tok_created', { client: tok.client })); notifyChanged(); this.paint(); } catch (x) { err.textContent = x.message; toastError(x); } } },
    field(t('k.t_client'), client, t('k.tok_client_hint')), h('div', { class: 'field' }, h('span', { class: 'lbl' }, t('k.t_scopes')), h('div', { class: 'stack-v tight' }, rows.map((r) => r.node)), h('div', { class: 'hint' }, t('k.tok_scope_hint'))), err, h('div', btn(t('k.tok_create'), { icon: 'plus', kind: 'primary', type: 'submit' })));
  }
  // The secret is shown exactly once, in a dialog, and is never stored in the page state or sent anywhere.
  showSecret(secret, title) {
    const code = h('code', { class: 'ro secret' }, secret);
    const m = openModal(title, h('div', { class: 'stack-v' }, h('p', { class: 'note warn' }, t('k.secret_once')), code, h('div', { class: 'row gap end' }, btn(t('common.copy'), { icon: 'copy', onClick: async () => { toast((await copyText(secret)) ? t('common.copied') : t('common.copy_failed'), { kind: 'ok' }); } }), btn(t('common.close'), { kind: 'primary', onClick: () => m.close() }))));
  }

  // ---- indexes ----
  async indexesTab() {
    const list = await this.api.kIndexes();
    const name = h('input', { type: 'text', id: 'ix-name', autocomplete: 'off' }), path = h('input', { type: 'text', id: 'ix-path', placeholder: '~/work/my-repo/codegraph-out', autocomplete: 'off', spellcheck: 'false' }), project = h('input', { type: 'text', id: 'ix-project', autocomplete: 'off' });
    const kind = select(['codegraph', 'symbols', 'other'].map((x) => ({ value: x, label: x })), 'codegraph', null, { id: 'ix-kind' }); const err = h('div', { class: 'perr', role: 'alert' });
    return h('div', { class: 'stack-v' }, h('p', { class: 'muted' }, t('k.ix_intro')),
      list.length ? h('div', { class: 'tablewrap' }, h('table', { class: 'compact' }, h('thead', h('tr', ['k.c_name', 'k.ix_kind', 'k.ix_path', 'k.ix_project', ''].map((k) => h('th', { scope: 'col' }, k ? t(k) : h('span', { class: 'sr-only' }, t('common.delete')))))),
        h('tbody', list.map((x) => h('tr', h('td', h('strong', x.name)), h('td', badge(x.kind, 'muted')), h('td', h('code', x.path)), h('td', x.project), h('td', { class: 'row' }, btn('', { icon: 'trash', kind: 'ghost', size: 'sm', title: t('k.ix_delete', { name: x.name }), onClick: async () => { if (await confirmDialog(t('k.ix_delete_confirm', { name: x.name }), { danger: true, confirmLabel: t('common.delete') })) { try { await this.api.kDeleteIndex(x.id); this.paint(); } catch (e) { toastError(e); } } } }))))))) : emptyBox(t('k.ix_none'), t('k.ix_none_hint')),
      h('div', { class: 'card' }, h('header', { class: 'card-head' }, h('h2', t('k.ix_add'))), h('div', { class: 'card-body' }, h('form', { class: 'stack-v', onSubmit: async (e) => { e.preventDefault(); err.textContent = ''; try { await this.api.kCreateIndex({ name: name.value, kind: kind.value, path: path.value, project: project.value }); toast(t('k.ix_added'), { kind: 'ok' }); this.paint(); } catch (x) { err.textContent = x.message; } } },
        h('div', { class: 'form-grid' }, field(t('k.c_name'), name), field(t('k.ix_kind'), kind), field(t('k.ix_path'), path, t('k.ix_path_hint')), field(t('k.ix_project'), project)), err, h('div', btn(t('k.ix_add'), { icon: 'plus', kind: 'primary', type: 'submit' }))))));
  }
}
void icon;
customElements.define('ah-knowledge', AhKnowledge);
