// Changes / Plan / Apply: pending content diff + commit (the approval of content), per-client plan review with per-file unified diffs,
// floor violations and diagnostics, explicit Apply confirmation with verify results, and history with revert.
import { AhView } from '../components/base.js';
import { h, icon, clear, append } from '../dom.js';
import { t } from '../i18n.js';
import { local } from '../store.js';
import { toast, toastError } from '../components/toast.js';
import { btn, badge, statusBadge, tabs, emptyBox, errorBox, notifyChanged, timeText, skeleton, field } from '../components/ui.js';
import { confirmDialog } from '../components/dialog.js';
import { diffView, statsText } from '../components/diff-view.js';
import { suggestMessage, validMessage, applyTotals, canApply, isBlocked, fileCounts, conflicts, stale } from '../plan.js';
import { timeAgo } from '../util.js';
import { rememberPlanViewed } from '../checklist.js';

const ACTION_KIND = { add: 'ok', change: 'warn', remove: 'bad', unchanged: 'muted', conflict: 'bad', advisory: 'info' };

class AhChanges extends AhView {
  setup() {
    this.tab = ['pending', 'plan', 'history'].includes(this.query?.tab) ? this.query.tab : 'pending';
    this.selClients = new Set(this.query?.client ? [this.query.client] : []);
    this.adopt = new Set(); this.reviewed = false; this.plan = null; this.result = null; this.hideUnchanged = true;
    this.panel = h('div', { class: 'changes-panel', role: 'tabpanel' });
    this.tabBar = tabs([{ id: 'pending', label: t('chg.tab_pending') }, { id: 'plan', label: t('chg.tab_plan') }, { id: 'history', label: t('chg.tab_history') }], this.tab, (id) => { this.tab = id; this.paint(); }, t('editor.tabs'));
    this.append(h('div', { class: 'page-head' }, h('h1', t('chg.title'))), h('p', { class: 'muted' }, t('chg.intro')), this.tabBar, this.panel);
    this.listen(window, 'ah:changed', () => { if (this.tab === 'pending' && !this.busy) this.paint(); });
    // ?tab=plan: build a plan straight away so "Review plan" from the matrix lands on something useful.
    if (this.tab === 'plan' && this.query?.tab === 'plan') this.planTab().then(() => this._alive && this.makePlan()); else this.paint();
  }
  paint() { if (this.tab === 'pending') this.loadInto(this.panel, () => this.pending()); else if (this.tab === 'plan') this.planTab(); else this.loadInto(this.panel, () => this.historyTab()); }

  // ---- pending ----
  async pending() {
    const ch = await this.api.changes();
    const files = ch.files || [];
    if (!files.length) return emptyBox(t('chg.none_title'), t('chg.none_hint'), h('div', { class: 'row gap' }, h('a', { class: 'btn', href: '#/matrix' }, t('nav.matrix')), h('a', { class: 'btn', href: '#/library' }, t('nav.library')), btn(t('chg.go_plan'), { onClick: () => { this.tab = 'plan'; this.tabBar.querySelector('#tab-plan').click(); } })));
    const msg = h('textarea', { id: 'commit-msg', rows: 2, maxlength: 200, placeholder: t('chg.msg_ph'), 'aria-describedby': 'commit-h' }); msg.value = suggestMessage(files);
    const doCommit = async (thenPlan) => {
      if (!validMessage(msg.value)) { msg.focus(); toast(t('chg.msg_bad'), { kind: 'warn' }); return; }
      this.busy = true;
      try { const r = await this.api.commit(msg.value.trim()); toast(t('chg.committed', { commit: String(r.commit).slice(0, 8) }), { kind: 'ok' }); notifyChanged(); this.busy = false; if (thenPlan) { this.tab = 'plan'; this.tabBar.querySelector('#tab-plan').click(); this.makePlan(); } else this.paint(); }
      catch (e) { this.busy = false; toastError(e); }
    };
    const commit = btn(t('chg.commit'), { icon: 'check', onClick: () => doCommit(false) });
    const commitPlan = btn(t('chg.commit_plan'), { icon: 'changes', kind: 'primary', onClick: () => doCommit(true) });
    return h('div', { class: 'stack-v' },
      h('section', { class: 'card' }, h('header', { class: 'card-head' }, h('h2', t('chg.commit_title'))), h('div', { class: 'card-body' }, h('p', { class: 'hint', id: 'commit-h' }, t('chg.commit_hint')), field(t('chg.msg'), msg),
        this.query?.next === 'plan' ? h('p', { class: 'note info' }, t('chg.next_plan_hint')) : null,
        h('div', { class: 'row gap wrap' }, commitPlan, commit, btn(t('chg.discard_all'), { icon: 'trash', onClick: () => this.discard(null, files.length) })))),
      h('h2', { class: 'section' }, t('chg.files', { count: files.length })),
      h('div', { class: 'stack-v tight' }, files.map((f) => h('details', { class: 'plan-file' }, h('summary', badge(t(`chg.s_${f.status}`), f.status === 'added' ? 'ok' : f.status === 'deleted' ? 'bad' : 'warn'), ' ', h('code', f.path), ' ', h('span', { class: 'muted small' }, statsText(f.diff)), ' ', btn(t('chg.discard'), { size: 'sm', kind: 'ghost', onClick: (e) => { e.preventDefault(); this.discard([f.path], 1); } })), diffView(f.diff)))));
  }
  async discard(paths, n) {
    if (!(await confirmDialog(t('chg.discard_confirm', { count: n }), { danger: true, confirmLabel: t('chg.discard') }))) return;
    try { await this.api.discard(paths); toast(t('chg.discarded'), { kind: 'ok' }); notifyChanged(); this.paint(); } catch (e) { toastError(e); }
  }

  // ---- plan ----
  async planTab() {
    clear(this.panel);
    let clients = [];
    this.panel.append(skeleton(2));
    try { clients = await this.api.clients(); } catch (e) { clear(this.panel).append(errorBox(e, () => this.planTab())); return; }
    if (!this._alive) return;
    this.clients = clients;
    const boxes = clients.map((c) => h('label', { class: 'check' }, h('input', { type: 'checkbox', checked: this.selClients.has(c.id), onChange: (e) => { e.target.checked ? this.selClients.add(c.id) : this.selClients.delete(c.id); } }), h('span', c.display_name)));
    this.planHost = h('div', { class: 'stack-v' });
    clear(this.panel).append(h('section', { class: 'card' }, h('div', { class: 'card-body' }, h('p', { class: 'hint' }, t('chg.plan_hint')), h('div', { class: 'row gap wrap' }, ...boxes), h('div', { class: 'row gap wrap' },
      h('button', { type: 'button', class: 'btn primary', dataset: { action: 'make-plan' }, onClick: () => this.makePlan() }, icon('changes', 16), h('span', this.selClients.size ? t('chg.make_plan_n', { count: this.selClients.size }) : t('chg.make_plan_all'))), h('span', { class: 'muted small' }, t('chg.plan_pure'))))), this.planHost);
    if (this.plan) this.paintPlan(); else if (this.result) this.paintResult();
  }
  async makePlan() {
    if (!this.planHost) await this.planTab();
    this.result = null; this.adopt = new Set(); this.reviewed = false;
    clear(this.planHost).append(skeleton(4));
    try { this.plan = await this.api.plan(this.selClients.size ? [...this.selClients] : undefined); rememberPlanViewed(this.plan.content_hash); this.paintPlan(); }
    catch (e) { this.plan = null; clear(this.planHost).append(errorBox(e, () => this.makePlan())); }
  }
  paintPlan() {
    const p = this.plan; const host = this.planHost; clear(host);
    if (!p) return;
    const totals = () => applyTotals(p);
    const isStale = false; void isStale;
    append(host, [
      h('div', { class: 'row between wrap gap' }, h('div', { class: 'muted small' }, t('chg.plan_meta', { id: p.id, hash: String(p.content_hash).slice(0, 8), when: timeText(timeAgo(p.created_at)) })),
        h('label', { class: 'check' }, h('input', { type: 'checkbox', checked: this.hideUnchanged, onChange: (e) => { this.hideUnchanged = e.target.checked; this.paintPlan(); } }), h('span', t('chg.hide_unchanged')))),
      p.pending_changes ? h('p', { class: 'note warn' }, t('chg.uncommitted', { count: p.pending_changes }), ' ', h('a', { href: '#/changes?tab=pending', onClick: () => { this.tab = 'pending'; this.tabBar.querySelector('#tab-pending').click(); } }, t('chg.commit_first'))) : null,
      ...p.clients.map((cp) => this.clientPlan(cp)),
      this.applyBox(totals())]);
  }
  clientPlan(cp) {
    const s = fileCounts(cp); const blocked = isBlocked(cp);
    const shown = cp.files.filter((f) => !(this.hideUnchanged && f.action === 'unchanged'));
    return h('section', { class: `card plan-client${blocked ? ' is-blocked' : ''}`, 'aria-label': cp.display_name || cp.client },
      h('header', { class: 'card-head' }, h('h2', h('a', { href: `#/clients/${encodeURIComponent(cp.client)}` }, cp.display_name || this.clients?.find((c) => c.id === cp.client)?.display_name || cp.client)), h('div', { class: 'row gap wrap' },
        ['add', 'change', 'remove', 'conflict', 'advisory', 'unchanged'].filter((k) => s[k]).map((k) => badge(`${s[k]} ${t(`plan.a_${k}`)}`, ACTION_KIND[k])), blocked ? badge(t('plan.blocked'), 'bad') : null)),
      h('div', { class: 'card-body' },
        (cp.floor_violations || []).length ? h('div', { class: 'note bad', role: 'alert' }, h('strong', h('span', { class: 'lock' }, icon('lock', 14)), ' ', t('plan.floor_violations')), h('ul', cp.floor_violations.map((v) => h('li', v.message, v.reason ? h('div', { class: 'muted small' }, v.reason) : null))), h('p', { class: 'small' }, t('plan.floor_fix'))) : null,
        (cp.diagnostics || []).length ? h('ul', { class: 'issue-list' }, cp.diagnostics.map((d) => h('li', { class: `note ${d.severity === 'error' ? 'bad' : d.severity === 'warn' ? 'warn' : 'info'}` }, h('strong', t(`common.sev_${d.severity}`)), ' ', d.message))) : null,
        blocked && !(cp.floor_violations || []).length ? h('p', { class: 'note bad' }, t('plan.blocked_strict'), (cp.blocked_reasons || []).length ? ` ${cp.blocked_reasons.join('; ')}` : '') : null,
        shown.length ? h('div', { class: 'stack-v tight' }, shown.map((f) => this.fileRow(cp, f))) : h('p', { class: 'muted' }, t('plan.nothing_here'))));
  }
  fileRow(cp, f) {
    const d = h('details', { class: `plan-file a-${f.action}` });
    const sum = h('summary', badge(t(`plan.a_${f.action}`), ACTION_KIND[f.action]), ' ', h('code', f.path), ' ', h('span', { class: 'muted small' }, `${f.root} · ${f.kind}${f.merge && f.merge !== 'own' ? ` · ${f.merge}` : ''}`), ' ', f.diff ? h('span', { class: 'muted small' }, statsText(f.diff)) : null, f.reason ? h('span', { class: 'muted small' }, ` - ${f.reason}`) : null);
    d.append(sum);
    // Diffs are rendered on first open: a plan can contain hundreds of files.
    let done = false;
    d.addEventListener('toggle', () => { if (d.open && !done) { done = true; append(d, [f.diff ? diffView(f.diff) : h('p', { class: 'muted small' }, t('diff.none')), (f.source_ids || []).length ? h('p', { class: 'muted small' }, t('plan.from', { ids: f.source_ids.join(', ') })) : null,
      f.action === 'conflict' ? h('div', { class: 'note warn' }, h('p', t('plan.conflict_hint')), h('label', { class: 'check' }, h('input', { type: 'checkbox', checked: this.adopt.has(f.path), onChange: (e) => { e.target.checked ? this.adopt.add(f.path) : this.adopt.delete(f.path); this.refreshApply(); } }), h('span', t('plan.adopt_path')))) : null]); } });
    return d;
  }
  applyBox(totals) {
    const box = h('section', { class: 'card apply-box' }); this.applyEl = box; this.fillApply(totals); return box;
  }
  refreshApply() { if (this.applyEl && this.plan) this.fillApply(applyTotals(this.plan)); }
  fillApply(totals) {
    const p = this.plan; const gate = canApply(p, { reviewed: this.reviewed, adopt: this.adopt });
    const rev = h('input', { type: 'checkbox', id: 'reviewed', checked: this.reviewed, onChange: (e) => { this.reviewed = e.target.checked; this.refreshApply(); } });
    const go = btn(t('chg.apply'), { icon: 'check', kind: 'primary', disabled: !gate.ok, onClick: () => this.apply(totals) });
    clear(this.applyEl).append(h('header', { class: 'card-head' }, h('h2', t('chg.apply_title'))), h('div', { class: 'card-body' },
      h('p', t('chg.apply_summary', { clients: totals.clients, files: totals.files })),
      totals.blocked ? h('p', { class: 'note bad' }, t('chg.apply_blocked', { count: totals.blocked, ids: totals.blockedIds.join(', ') })) : null,
      totals.conflicts ? h('p', { class: 'note warn' }, t('chg.apply_conflicts', { count: totals.conflicts, adopted: this.adopt.size })) : null,
      h('label', { class: 'check' }, rev, h('span', t('chg.reviewed'))), h('div', { class: 'row gap wrap' }, go, !gate.ok && gate.reason ? h('span', { class: 'muted small' }, t(`chg.gate_${gate.reason}`)) : null)));
  }
  async apply(totals) {
    if (!(await confirmDialog(t('chg.apply_confirm', { clients: totals.clients, files: totals.files }), { confirmLabel: t('chg.apply') }))) return;
    this.busy = true;
    try { this.result = await this.api.apply(this.plan.id, [...this.adopt]); this.plan = null; toast(t(this.result.ok ? 'chg.applied' : 'chg.applied_partial'), { kind: this.result.ok ? 'ok' : 'warn', timeout: 7000 }); notifyChanged(); this.paintResult(); }
    catch (e) {
      if (e.code === 'plan_stale') { toast(t('err.plan_stale'), { kind: 'warn', timeout: 9000, action: { label: t('chg.replan'), run: () => this.makePlan() } }); this.plan = null; clear(this.planHost).append(h('div', { class: 'note warn' }, t('err.plan_stale'), ' ', btn(t('chg.replan'), { onClick: () => this.makePlan() }))); }
      else if (e.code === 'uncommitted_changes') { toast(e.message, { kind: 'warn', timeout: 9000 }); }
      else toastError(e);
    } finally { this.busy = false; }
  }
  paintResult() {
    const r = this.result; const host = this.planHost; if (!host) return; clear(host);
    const stKind = { applied: 'ok', nothing: 'muted', blocked: 'bad', failed: 'bad' };
    append(host, [h('div', { class: `note ${r.ok ? 'ok' : 'warn'}`, role: 'status' }, h('strong', t(r.ok ? 'chg.applied' : 'chg.applied_partial'))),
      ...r.results.map((x) => h('section', { class: 'card' }, h('header', { class: 'card-head' }, h('h2', x.client), badge(t(`status.${x.status}`), stKind[x.status] || 'warn')),
        h('div', { class: 'card-body' }, x.status === 'blocked' ? h('p', { class: 'note bad' }, t('chg.result_blocked', { reason: x.reason || '' })) : x.status === 'failed' ? h('p', { class: 'note bad' }, t('chg.result_failed', { error: x.error || '' })) : x.status === 'nothing' ? h('p', { class: 'muted' }, t('chg.result_nothing')) : h('p', t('chg.result_counts', { written: x.written, removed: x.removed })),
          x.skipped.length ? h('p', { class: 'note warn' }, t('chg.result_skipped', { count: x.skipped.length }), ' ', h('code', x.skipped.join(', '))) : null,
          x.verify.length ? h('div', h('h3', t('chg.verify')), h('ul', { class: 'checks' }, x.verify.map((k) => h('li', { class: k.ok ? 'ok' : 'bad' }, icon(k.ok ? 'check' : 'warn', 14), h('span', h('strong', k.name || t('chg.check')), k.detail ? ` - ${k.detail}` : ''))))) : null,
          x.backup ? h('p', { class: 'muted small' }, t('chg.backup'), ' ', h('code', x.backup)) : null))),
      h('div', { class: 'row gap' }, btn(t('chg.replan'), { onClick: () => this.makePlan() }), h('a', { class: 'btn', href: '#/clients' }, t('nav.clients')))]);
  }

  // ---- history ----
  async historyTab() {
    const list = await this.api.history();
    if (!list.length) return emptyBox(t('chg.hist_none'), t('chg.hist_none_hint'));
    return h('div', { class: 'tablewrap' }, h('table', { class: 'compact' }, h('thead', h('tr', ['chg.h_commit', 'chg.h_message', 'chg.h_author', 'chg.h_when', 'chg.h_files', ''].map((k) => h('th', { scope: 'col' }, k ? t(k) : h('span', { class: 'sr-only' }, t('chg.revert')))))),
      h('tbody', list.map((c) => h('tr', h('td', h('code', String(c.commit).slice(0, 8))), h('td', { class: 'wide' }, c.message), h('td', c.author), h('td', { class: 'nowrap muted' }, timeText(timeAgo(c.ts))), h('td', { class: 'num' }, c.files == null ? '-' : String(c.files)),
        h('td', { class: 'row' }, btn(t('chg.revert'), { size: 'sm', icon: 'undo', onClick: () => this.revert(c) })))))));
  }
  async revert(c) {
    if (!(await confirmDialog(t('chg.revert_confirm', { commit: String(c.commit).slice(0, 8), message: c.message }), { confirmLabel: t('chg.revert'), danger: true }))) return;
    try { await this.api.revert(c.commit); toast(t('chg.reverted', { commit: String(c.commit).slice(0, 8) }), { kind: 'ok' }); notifyChanged(); this.paint(); } catch (e) { toastError(e); }
  }
}
void stale; void conflicts;
customElements.define('ah-changes', AhChanges);
