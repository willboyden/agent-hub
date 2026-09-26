// Clients: cards with status, "what can this client do right now" (effective config), verify / drift / audit, manage flags with a diff
// before adopting, and an add-client wizard (pick an adapter or paste a generic spec with live validation and a dry-run preview).
import { AhView } from '../components/base.js';
import { h, icon, clear, append } from '../dom.js';
import { t } from '../i18n.js';
import { toast, toastError } from '../components/toast.js';
import { btn, badge, statusBadge, clientStatusBadge, driftDetails, kindBadge, field, tabs, emptyBox, errorBox, notifyChanged, timeText, chip, skeleton } from '../components/ui.js';
import { openModal, confirmDialog } from '../components/dialog.js';
import { wizard } from '../components/wizard.js';
import { diffView, statsText } from '../components/diff-view.js';
import { mapSpecResult, parseSpec, specErrorResult } from '../spec.js';
import { editHref } from '../data.js';
import { validName, slug, debounce, timeAgo } from '../util.js';
import { driftProblems, clientDoc } from '../adapt.js';

const CONCERNS = ['skills', 'agents', 'instructions', 'mcp', 'permissions', 'memory'];

class AhClients extends AhView {
  setup() {
    this.cid = this.params?.id || null; this.tab = this.query?.tab || 'overview';
    this.host = h('div'); this.append(this.host);
    this.listen(window, 'ah:changed', () => { if (this._alive && !this.busy) this.render(true); });
    this.render();
  }
  async render(quiet) {
    if (quiet && this.cid) return; // detail pages reload themselves on demand
    const fn = () => (this.cid ? this.detail() : this.list());
    if (quiet) { try { clear(this.host).append(...[await fn()].flat()); } catch { /* keep */ } return; }
    this.loadInto(this.host, fn, { skeletonRows: 4 });
  }
  // ---- list ----
  async list() {
    const clients = await this.api.clients();
    return [h('div', { class: 'row between wrap gap page-head' }, h('h1', t('cl.title')), btn(t('cl.add'), { icon: 'plus', kind: 'primary', onClick: () => this.addWizard() })),
      h('p', { class: 'muted' }, t('cl.intro')),
      clients.length ? h('div', { class: 'grid tiles client-grid' }, clients.map((c) => this.card(c))) : emptyBox(t('cl.empty_title'), t('cl.empty_hint'), btn(t('cl.add'), { icon: 'plus', kind: 'primary', onClick: () => this.addWizard() }))];
  }
  card(c) {
    const counts = c.counts || {};
    const out = h('div', { class: 'client-result', role: 'status', 'aria-live': 'polite' });
    const run = async (label, fn) => { clear(out).append(skeleton(1)); try { out.replaceChildren(await fn()); } catch (e) { out.replaceChildren(errorBox(e)); } };
    const countsEl = h('div', { class: 'counts-host' });
    const paintCounts = (n) => countsEl.replaceChildren(h('dl', { class: 'counts' }, ...['skills', 'agents', 'instructions', 'mcp'].map((k) => h('div', h('dt', t(`kind.${k === 'mcp' ? 'mcps' : k}`)), h('dd', c.caps?.[k] === false ? '–' : String(n[k] ?? 0))))));
    if (c.counts) paintCounts(c.counts); else this.api.effective(c.id).then((e) => paintCounts({ skills: e.items.skills.length, agents: e.items.agents.length, instructions: e.items.instructions.length, mcp: e.items.mcp.length })).catch((err) => countsEl.replaceChildren(h('p', { class: 'muted small' }, err.code === 'client_not_committed' ? t('cl.not_committed') : t('cl.counts_unavailable'))));
    const card = h('article', { class: 'card client-card', 'aria-label': c.display_name },
      h('div', { class: 'client-bar' }), // colour set below
      h('div', { class: 'card-body' },
        h('div', { class: 'row between gap wrap' }, h('h2', h('a', { href: `#/clients/${encodeURIComponent(c.id)}` }, c.display_name)), clientStatusBadge(c)),
        driftDetails(c),
        h('p', { class: 'muted small' }, c.description || c.adapter),
        h('div', { class: 'row gap wrap small' }, badge(c.adapter, 'muted', t('cl.adapter')), c.strict ? badge(t('cl.strict'), 'info', t('cl.strict_tip')) : badge(t('cl.lenient'), 'muted', t('cl.lenient_tip')), c.unusable ? badge(t('cl.no_adapter'), 'bad', t('cl.no_adapter_tip')) : null, c.pending_files ? badge(t('cl.pending_files', { count: c.pending_files }), 'warn') : null, c.blocked ? badge(t('cl.blocked'), 'bad', t('cl.blocked_tip')) : null),
        countsEl,
        h('p', { class: 'muted small' }, c.last_applied_at ? t('cl.last_applied', { when: timeText(timeAgo(c.last_applied_at)) }) : c.adopted_at ? t('cl.adopted_at', { when: timeText(timeAgo(c.adopted_at)) }) : (c.status === 'never_applied' ? t('cl.never_applied') : '')),
        h('div', { class: 'row gap wrap' }, btn(t('cl.details'), { icon: 'eye', size: 'sm', onClick: () => { location.hash = `#/clients/${encodeURIComponent(c.id)}`; } }),
          btn(t('cl.verify'), { size: 'sm', onClick: () => run('verify', async () => this.checksView(await this.api.verify(c.id))) }),
          btn(t('cl.drift'), { size: 'sm', onClick: () => run('drift', async () => this.driftView(await this.api.drift(c.id))) }),
          btn(t('cl.audit'), { size: 'sm', onClick: () => run('audit', async () => this.auditView(await this.api.audit(c.id))) })), out));
    if (c.color) card.querySelector('.client-bar').style.background = c.color;
    return card;
  }
  checksView(r) { return h('div', { class: 'stack-v tight' }, h('strong', t(r.ok ? 'cl.verify_ok' : 'cl.verify_bad')), h('ul', { class: 'checks' }, r.checks.map((k) => h('li', { class: k.ok ? 'ok' : 'bad' }, icon(k.ok ? 'check' : 'warn', 14), h('span', h('strong', k.name), k.detail ? ` - ${k.detail}` : ''))))); }
  driftView(r) {
    const bad = driftProblems(r);
    const kind = (st) => (st === 'changed_live' ? 'bad' : st === 'missing' || st === 'orphaned' ? 'warn' : 'info');
    return h('div', { class: 'stack-v tight' }, h('div', statusBadge(r.status), ' ', h('span', { class: 'muted small' }, t('cl.drift_summary', { changed: bad.length, total: r.files.length }))),
      ...bad.map((f) => h('details', { class: 'drift-file' }, h('summary', h('code', f.path), ' ', badge(t(`cl.dstate_${f.state}`), kind(f.state))), f.diff ? diffView(f.diff) : h('p', { class: 'muted small' }, f.reason || t(`cl.dstate_${f.state}_hint`)))));
  }
  auditView(r) { return h('div', { class: 'stack-v tight' }, h('strong', t(r.ok ? 'cl.audit_ok' : 'cl.audit_bad')), h('ul', { class: 'findings' }, r.findings.map((f) => h('li', { class: `note ${f.severity === 'error' ? 'bad' : f.severity === 'warn' ? 'warn' : 'info'}` }, h('strong', t(`common.sev_${f.severity}`)), ' ', f.message, f.path ? h('code', { class: 'small' }, ` ${f.path}`) : null)))); }

  // ---- detail ----
  async detail() {
    const c = await this.api.client(this.cid);
    this.c = c;
    const panel = h('div', { class: 'client-panel', role: 'tabpanel' });
    const paint = () => { clear(panel); if (this.tab === 'manage') this.loadInto(panel, () => this.managePanel(c)); else if (this.tab === 'health') panel.append(this.healthPanel(c)); else this.loadInto(panel, () => this.effectivePanel(c)); };
    const tabBar = tabs([{ id: 'overview', label: t('cl.tab_overview') }, { id: 'manage', label: t('cl.tab_manage') }, { id: 'health', label: t('cl.tab_health') }], this.tab, (id) => { this.tab = id; paint(); }, t('editor.tabs'));
    setTimeout(paint, 0);
    return [h('nav', { class: 'crumbs', 'aria-label': t('editor.breadcrumb') }, h('a', { href: '#/clients' }, t('nav.clients')), ' / ', h('strong', c.display_name)),
      h('div', { class: 'row between wrap gap page-head' }, h('div', { class: 'row gap wrap' }, h('h1', c.display_name), clientStatusBadge(c)),
        h('div', { class: 'row gap' }, btn(t('cl.make_plan'), { icon: 'changes', kind: 'primary', onClick: () => { location.hash = `#/changes?tab=plan&client=${encodeURIComponent(c.id)}`; } }), btn(t('common.delete'), { icon: 'trash', onClick: () => this.removeClient(c) }))),
      h('p', { class: 'muted' }, c.description), driftDetails(c), tabBar, panel];
  }
  async effectivePanel(c) {
    let e, working = false;
    try { e = await this.api.effective(c.id); }
    catch (err) { if (err.code !== 'client_not_committed') throw err; e = await this.api.effective(c.id, 'working'); working = true; }
    const lists = [['skills', 'skill'], ['agents', 'agent'], ['instructions', 'instruction'], ['mcp', 'mcp'], ['memory', 'memory']].filter(([k]) => c.caps?.[k]);
    return h('div', { class: 'stack-v' },
      h('p', { class: 'muted' }, t('cl.effective_intro', { name: c.display_name })), working ? h('p', { class: 'note info' }, t('cl.effective_working')) : null,
      e.warnings?.length ? h('ul', { class: 'issue-list' }, e.warnings.map((w) => h('li', { class: `note ${w.severity === 'error' ? 'bad' : 'warn'}` }, h('strong', t(`common.sev_${w.severity}`)), ' ', w.message))) : h('p', { class: 'note ok' }, t('cl.no_warnings')),
      h('div', { class: 'grid two' }, lists.map(([k, kind]) => h('section', { class: 'card' }, h('header', { class: 'card-head' }, h('h2', t(`kind.${kind === 'mcp' ? 'mcps' : kind + 's'}`)), h('span', { class: 'muted' }, String(e.items[k]?.length ?? 0))),
        h('div', { class: 'card-body' }, e.items[k]?.length ? h('div', { class: 'row gap wrap eff-items' }, e.items[k].map((n) => h('a', { class: 'chip', href: editHref(kind, n) }, n))) : h('p', { class: 'muted' }, t('cl.none_enabled')))))),
      h('div', { class: 'grid two' },
        h('section', { class: 'card' }, h('header', { class: 'card-head' }, h('h2', t('cl.tools'))), h('div', { class: 'card-body' }, c.caps?.tool_map && Object.keys(c.caps.tool_map).length ? h('div', { class: 'tablewrap' }, h('table', { class: 'compact' }, h('thead', h('tr', h('th', { scope: 'col' }, t('cl.capability')), h('th', { scope: 'col' }, t('cl.native')))), h('tbody', e.tools.map((x) => h('tr', h('td', x.capability), h('td', x.native ? h('code', x.native) : h('span', { class: 'muted' }, t('editor.no_native')))))))) : h('p', { class: 'muted' }, t('cl.no_tools')))),
        h('section', { class: 'card' }, h('header', { class: 'card-head' }, h('h2', t('cl.egress'))), h('div', { class: 'card-body' }, h('p', badge(t(`rule.dec_${e.egress.default}`), 'bad'), ' ', h('span', t('cl.egress_default'))), e.egress.allowed.length ? h('div', { class: 'row gap wrap' }, e.egress.allowed.map((x) => chip(x))) : h('p', { class: 'muted' }, t('cl.egress_none'))))),
      h('section', { class: 'card' }, h('header', { class: 'card-head' }, h('h2', t('cl.rules'))), h('div', { class: 'card-body' }, h('div', { class: 'tablewrap' }, h('table', { class: 'compact floor-table' }, h('thead', h('tr', ['', t('rule.col_rule'), t('rule.col_decision'), t('rule.col_why')].map((x, i) => h('th', { scope: 'col' }, i ? x : h('span', { class: 'sr-only' }, t('lib.locked')))))),
        h('tbody', e.rules.map((r) => h('tr', { class: r.source === 'floor' ? 'locked-row' : '' }, h('td', r.source === 'floor' ? h('span', { class: 'lock', title: t('rules.locked_why') }, icon('lock', 16), h('span', { class: 'sr-only' }, t('lib.locked'))) : ''), h('td', h('strong', r.title || r.name), h('div', { class: 'muted small' }, `${t(`rule.kind_${r.kind}`)}: `, h('code', r.match))), h('td', badge(t(`rule.dec_${r.decision}`), r.decision === 'deny' ? 'bad' : r.decision === 'ask' ? 'warn' : 'ok')), h('td', { class: 'small' }, r.reason || '')))))))));
  }
  healthPanel(c) {
    const out = h('div', { class: 'stack-v', role: 'status', 'aria-live': 'polite' });
    const run = async (fn) => { clear(out).append(skeleton(2)); try { out.replaceChildren(await fn()); } catch (e) { out.replaceChildren(errorBox(e)); } };
    return h('div', { class: 'stack-v' }, h('p', { class: 'muted' }, t('cl.health_intro')), h('div', { class: 'row gap wrap' },
      btn(t('cl.verify'), { icon: 'check', onClick: () => run(async () => this.checksView(await this.api.verify(c.id))) }), btn(t('cl.drift'), { icon: 'changes', onClick: () => run(async () => this.driftView(await this.api.drift(c.id))) }), btn(t('cl.audit'), { icon: 'shield', onClick: () => run(async () => this.auditView(await this.api.audit(c.id))) })), out);
  }
  async managePanel(c) {
    const rows = CONCERNS.map((k) => {
      const supported = !!c.caps?.[k]; const on = !!c.manage?.[k];
      const prev = h('div', { class: 'manage-preview' });
      const box = h('input', { type: 'checkbox', role: 'switch', id: `mg-${k}`, checked: on, disabled: !supported, 'aria-describedby': `mg-${k}-d`,
        onChange: (e) => (e.target.checked ? this.adopt(c, k, box, prev) : this.unadopt(c, k, box)) });
      return h('div', { class: 'manage-row' }, h('div', { class: 'row gap between wrap' }, h('div', h('label', { for: `mg-${k}`, class: 'manage-name' }, t(`concern.${k}`)), h('div', { class: 'hint', id: `mg-${k}-d` }, supported ? t(`concern.${k}_desc`) : t('cl.concern_unsupported'))),
        h('div', { class: 'row gap' }, badge(on ? t('cl.managed') : t('cl.advisory'), on ? 'ok' : 'info'), box)), prev);
    });
    return h('div', { class: 'stack-v' }, h('p', { class: 'muted' }, t('cl.manage_intro')), h('div', { class: 'card' }, h('div', { class: 'card-body' }, rows)));
  }
  // Adopting a concern: show what the hub WOULD write. The plan already lists advisory files with their diffs (the hub renders them but does not
  // write them), so no special preview endpoint is needed. Plans are built from committed content.
  async adopt(c, concern, box, prev) {
    clear(prev).append(skeleton(2));
    let files = null, err = null;
    try { const p = await this.api.plan([c.id]); files = p.clients[0]?.files || []; } catch (e) { err = e; }
    const cancel = () => { box.checked = false; clear(prev); };
    const confirm = btn(t('cl.adopt_confirm', { concern: t(`concern.${concern}`) }), { kind: 'primary', icon: 'check', onClick: async () => {
      try { await this.api.updateClient(c.id, clientDoc(c, { manage: { [concern]: true } })); toast(t('cl.adopted', { concern: t(`concern.${concern}`) }), { kind: 'ok' }); notifyChanged(); this.tab = 'manage'; this.render(); } catch (e) { toastError(e); cancel(); }
    } });
    const KIND_OF = { skills: 'skill', agents: 'agent', instructions: 'instruction', mcp: 'mcp', memory: 'memory', permissions: 'rule' };
    const changed = (files || []).filter((f) => f.kind === KIND_OF[concern] && (f.action === 'advisory' || f.managed === false));
    const list = err ? h('p', { class: 'note warn' }, err.status === 404 || err.code === 'client_not_committed' ? t('cl.preview_uncommitted') : t('cl.preview_failed'))
      : changed.length ? changed.map((f) => h('details', { class: 'plan-file' }, h('summary', h('code', f.path), ' ', badge(t('plan.a_advisory'), 'info'), ' ', h('span', { class: 'muted small' }, statsText(f.diff))), f.diff ? diffView(f.diff) : h('p', { class: 'muted small' }, t('diff.none'))))
        : h('p', { class: 'muted' }, t('cl.preview_none'));
    prev.replaceChildren(h('div', { class: 'note info' }, h('strong', t('cl.adopt_title')), ' ', t('cl.adopt_hint')), ...[list].flat(), h('div', { class: 'row gap end' }, btn(t('common.cancel'), { onClick: cancel }), confirm));
    confirm.focus();
  }
  async unadopt(c, concern, box) {
    if (!(await confirmDialog(t('cl.unadopt_confirm', { concern: t(`concern.${concern}`) }), { confirmLabel: t('cl.make_advisory') }))) { box.checked = true; return; }
    try { await this.api.updateClient(c.id, clientDoc(c, { manage: { [concern]: false } })); toast(t('cl.unadopted', { concern: t(`concern.${concern}`) }), { kind: 'ok' }); notifyChanged(); this.render(); } catch (e) { toastError(e); box.checked = true; }
  }
  async removeClient(c) {
    if (!(await confirmDialog(t('cl.delete_confirm', { name: c.display_name }), { danger: true, confirmLabel: t('common.delete') }))) return;
    try { await this.api.deleteClient(c.id); toast(t('cl.deleted', { name: c.display_name }), { kind: 'ok' }); notifyChanged(); location.hash = '#/clients'; } catch (e) { toastError(e); }
  }

  // ---- add-client wizard ----
  async addWizard() {
    let adapters; try { adapters = await this.api.adapters(); } catch (e) { toastError(e); return; }
    const st = { adapter: adapters[0]?.id, id: '', name: '', desc: '', strict: true, roots: {}, spec: '', specResult: null, dry: null };
    let idTouched = false, defaults = {};
    const loadDefaults = async () => { try { const a = await this.api.adapter(st.adapter); defaults = a.default_config || {}; st.roots = { ...(defaults.roots || {}) }; st.strict = defaults.strict ?? true; if (st.adapter === 'generic' && !st.spec) st.spec = defaultSpecText(defaults.spec); } catch { defaults = {}; } };
    const steps = [
      { id: 'adapter', title: t('cl.w_adapter'), render: () => h('div', { class: 'stack-v', role: 'radiogroup', 'aria-label': t('cl.w_adapter') }, adapters.map((a) => h('label', { class: `pick-card${st.adapter === a.id ? ' on' : ''}` }, h('input', { type: 'radio', name: 'ad', value: a.id, checked: st.adapter === a.id, onChange: async () => { st.adapter = a.id; st.spec = ''; await loadDefaults(); w.refresh(); } }),
        h('div', h('strong', a.display_name), h('div', { class: 'muted small' }, a.docs || ''), h('div', { class: 'row gap wrap small' }, ...['skills', 'agents', 'instructions', 'mcp'].map((k) => badge(t(`kind.${k === 'mcp' ? 'mcps' : k}`), a.caps?.[k] ? 'ok' : 'muted'))))))),
        onEnter: async () => { if (!Object.keys(defaults).length) await loadDefaults(); } },
      { id: 'details', title: t('cl.w_details'), render: () => {
        const name = h('input', { type: 'text', id: 'cw-name', value: st.name, autocomplete: 'off', placeholder: t('cl.w_name_ph') });
        const id = h('input', { type: 'text', id: 'cw-cid', value: st.id, autocomplete: 'off', spellcheck: 'false' });
        name.addEventListener('input', () => { st.name = name.value; if (!idTouched) { st.id = slug(name.value); id.value = st.id; } }); id.addEventListener('input', () => { idTouched = true; st.id = id.value; });
        const roots = Object.entries(st.roots).map(([k, v]) => field(t('cl.w_root', { name: k }), h('input', { type: 'text', value: v, autocomplete: 'off', spellcheck: 'false', onInput: (e) => { st.roots[k] = e.target.value; } }), t('cl.w_root_hint')));
        return h('div', { class: 'stack-v' }, field(t('cl.w_display'), name), field(t('cl.f_id'), id, t('coll.f_id_hint')), field(t('coll.f_desc'), h('input', { type: 'text', value: st.desc, onInput: (e) => { st.desc = e.target.value; } })), ...roots,
          h('label', { class: 'check' }, h('input', { type: 'checkbox', checked: st.strict, onChange: (e) => { st.strict = e.target.checked; } }), h('span', t('cl.w_strict'))), h('p', { class: 'hint' }, t('cl.w_strict_hint')));
      }, valid: () => (!st.name.trim() ? t('cl.w_need_name') : !validName(st.id) ? t('new.bad_name') : true) },
      { id: 'spec', get title() { return st.adapter === 'generic' ? t('cl.w_spec') : t('cl.w_review'); }, render: () => (st.adapter === 'generic' ? this.specStep(st) : h('div', { class: 'stack-v' }, h('p', { class: 'muted' }, t('cl.w_review_hint')), h('pre', { class: 'cmd' }, JSON.stringify({ id: st.id, adapter: st.adapter, display_name: st.name, strict: st.strict, roots: st.roots }, null, 2)))),
        valid: () => (st.adapter !== 'generic' || st.specResult?.ok ? true : t('cl.w_spec_invalid')) },
    ];
    const w = wizard({ steps, finishLabel: t('cl.w_create'), onFinish: async () => {
      const body = { id: st.id, adapter: st.adapter, display_name: st.name.trim(), description: st.desc, strict: st.strict, roots: st.roots, ...(st.adapter === 'generic' ? { spec: st.specObject } : {}) };
      const c = await this.api.createClient(body);
      m.close(); toast(t('cl.created', { name: c.display_name }), { kind: 'ok' }); notifyChanged(); location.hash = `#/clients/${encodeURIComponent(c.id)}`;
    } });
    const m = openModal(t('cl.add'), w); await loadDefaults(); w.refresh();
  }
  specStep(st) {
    const ta = h('textarea', { id: 'spec-in', class: 'spec-in', rows: 14, spellcheck: 'false', 'aria-describedby': 'spec-status', 'aria-label': t('cl.w_spec'), value: st.spec });
    const status = h('div', { id: 'spec-status', class: 'spec-status', role: 'status', 'aria-live': 'polite' });
    const preview = h('div', { class: 'spec-preview' });
    let seq = 0;
    const show = (r) => {
      st.specResult = r; ta.setAttribute('aria-invalid', String(!r.ok));
      status.replaceChildren(); append(status, [h('p', { class: `note ${r.ok ? 'ok' : 'bad'}` }, r.ok ? t('cl.spec_ok') : t('cl.spec_errors', { count: Math.max(1, r.errors.length) })),
        r.issues.length ? h('ul', { class: 'issue-list' }, r.issues.map((i) => h('li', { class: `note ${i.severity === 'error' ? 'bad' : 'warn'}` }, h('strong', i.path || t(`common.sev_${i.severity}`)), ' ', i.key ? t(i.key, i.vars) : i.fallback, i.hint ? h('div', { class: 'muted small' }, i.hint) : null))) : null]);
      clear(preview).append(...(r.files.length ? [h('h3', t('cl.spec_preview')), h('p', { class: 'hint' }, t('cl.spec_preview_hint')), ...r.files.map((f) => h('details', { class: 'plan-file', open: true }, h('summary', h('code', `${f.root}: ${f.path}`), ' ', f.kind ? kindBadge(f.kind) : null), h('pre', { class: 'cmd rendered' }, f.content)))] : []));
    };
    const check = async () => {
      const mine = ++seq;
      const p = parseSpec(ta.value);
      if (p.empty) { st.specResult = null; st.specObject = null; status.replaceChildren(h('p', { class: 'muted' }, t('cl.spec_empty'))); clear(preview); return; }
      if (p.error) { st.specObject = null; show(mapSpecResult({ ok: false, errors: [{ code: p.error.code, path: p.error.line ? `line ${p.error.line}` : '', message: p.error.message }] })); return; }
      st.specObject = p.spec;
      status.replaceChildren(h('span', { class: 'muted' }, t('common.loading')));
      try { const r = mapSpecResult(await this.api.validateSpec({ spec: p.spec, roots: st.roots, strict: st.strict })); if (mine === seq) show(r); }
      catch (e) { if (mine !== seq) return; if (e.status === 422) show(specErrorResult(e)); else { st.specResult = null; status.replaceChildren(errorBox(e, check)); } }
    };
    const slow = debounce(check, 350);
    ta.addEventListener('input', () => { st.spec = ta.value; slow(); });
    setTimeout(check, 0);
    return h('div', { class: 'stack-v' }, h('p', { class: 'muted' }, t('cl.spec_intro')), h('div', { class: 'spec-grid' }, h('div', field(t('cl.w_spec'), ta, t('cl.spec_hint')), status), preview));
  }
}
const defaultSpecText = (spec) => (spec ? `skills:\n  root: ${spec.skills?.root || 'home'}\n  dir: ${spec.skills?.dir || 'skills'}\n  layout: ${spec.skills?.layout || 'dir/SKILL.md'}\ninstructions:\n  root: ${spec.instructions?.root || 'home'}\n  target: ${spec.instructions?.target || 'INSTRUCTIONS.md'}\n  merge: ${spec.instructions?.merge || 'block'}\n` : '');
customElements.define('ah-clients', AhClients);
