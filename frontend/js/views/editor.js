// Editor: one item at a time. Skill (form + markdown + file tree), agent (capabilities + native tool mapping), MCP (scan, egress, per-client
// enable), rule (guided, floor-aware), instruction (ordering), memory. A "Rendered for <client>" tab shows the exact artifact.
import { AhView } from '../components/base.js';
import { h, icon, clear } from '../dom.js';
import { t } from '../i18n.js';
import { toast, toastError } from '../components/toast.js';
import { btn, badge, kindBadge, field, chip, tabs, notifyChanged, emptyBox, errorBox, select } from '../components/ui.js';
import { collectionChip } from '../components/parts.js';
import { pickCollections } from '../components/picker.js';
import { confirmDialog, openModal } from '../components/dialog.js';
import { mdEditor } from '../components/md-editor.js';
import { enabledCard } from '../components/enabled-card.js';
import { validateSkill, validateAgent, validateFilePath, skillFrontmatter, agentClientPreview, CAPABILITIES, MODEL_TIERS } from '../validate.js';
import { moveBy, moveRelative, dropPosition, renumber } from '../reorder.js';
import { validName } from '../util.js';
import { parseUnified } from '../diff.js';
import { detailDraft, toPutBody, validTag, floorRows, isFloorKey } from '../adapt.js';
import { diffView } from '../components/diff-view.js';

const CONCERN = { skill: 'skills', agent: 'agents', instruction: 'instructions', mcp: 'mcp', memory: 'memory', rule: 'permissions' };
const DEFAULTS = {
  skill: (name, title) => ({ name, description: title ? `${title}. Use when ` : '', body: `# ${title || name}\n\n## When to use\n\n- \n\n## Steps\n\n1. \n`, tags: [], notes: '', files: [], groups: [] }),
  agent: (name) => ({ name, description: '', body: 'You are a focused subagent. Work step by step and report only what you ran.\n', capabilities: ['read'], model_tier: 'standard', mode: 'subagent', read_only: false, tags: [], notes: '', groups: [] }),
  instruction: (name, title) => ({ name, title: title || name, body: `# ${title || name}\n\n`, order: 100, applies_to: [], groups: [] }),
  mcp: (name) => ({ name, description: '', transport: 'stdio', command: '', args: [], url: null, env_names: [], sandbox_profile: 'srt', egress_hosts: [], pinned_ref: null, tags: [], notes: '', groups: [] }),
  rule: (name, title) => ({ name, title: title || name, rule_kind: 'tool', match: '', decision: 'ask', reason: '', applies_to: [], groups: [] }),
  memory: (name, title) => ({ name, title: title || name, type: 'project', body: '', groups: [] }),
};

class AhEditor extends AhView {
  setup() {
    this.kind = this.params.kind; this.name = this.params.name; this.creating = this.query?.create === '1';
    this.tab = 'edit'; this.file = 'SKILL.md'; this.dirty = false; this.serverIssues = [];
    this.body = h('div', { class: 'editor-body' });
    this.append(this.body);
    this.listen(window, 'keydown', (e) => { if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 's') { e.preventDefault(); this.save(); } });
    this.listen(window, 'beforeunload', (e) => { if (this.dirty) { e.preventDefault(); e.returnValue = ''; } });
    this.load();
  }
  async load() {
    await this.loadInto(this.body, async () => {
      const [clients, collections, item] = await Promise.all([this.api.clients(), this.api.collections(), this.creating ? null : this.api.item(this.kind, this.name)]);
      this.clients = clients; this.collections = collections;
      this.item = item;
      this.d = item ? detailDraft(this.kind, item) : DEFAULTS[this.kind](this.name, this.query?.title);
      this.locked = false;
      this.dirty = this.creating; this.serverIssues = [];
      return this.frame();
    }, { skeletonRows: 6 });
  }
  toBody() { return toPutBody(this.kind, this.d); }
  touch() { this.dirty = true; this.paintHead(); this.paintIssues(); }
  frame() {
    this.headEl = h('div', { class: 'editor-head' });
    this.issuesEl = h('div', { class: 'issues', role: 'status', 'aria-live': 'polite' });
    this.panel = h('div', { class: 'editor-panel', role: 'tabpanel' });
    const tabBar = tabs([{ id: 'edit', label: t('editor.tab_edit') }, { id: 'rendered', label: t('editor.tab_rendered') }, { id: 'used', label: t('editor.tab_used') }], this.tab, (id) => { this.tab = id; this.paintPanel(); }, t('editor.tabs'));
    this.paintHead(); this.paintIssues(); this.paintPanel();
    return [this.headEl, this.issuesEl, tabBar, this.panel];
  }
  paintHead() {
    clear(this.headEl).append(
      h('nav', { class: 'crumbs', 'aria-label': t('editor.breadcrumb') }, h('a', { href: '#/library' }, t('nav.library')), ' / ', kindBadge(this.kind), ' ', h('strong', this.name)),
      h('div', { class: 'row gap wrap' }, this.dirty ? badge(t('editor.unsaved'), 'warn') : null,
        this.kind === 'skill' && !this.creating ? btn(t('editor.duplicate'), { icon: 'copy', onClick: () => this.duplicate() }) : null,
        !this.creating && !this.locked ? btn(t('common.delete'), { icon: 'trash', onClick: () => this.remove() }) : null,
        !this.locked ? btn(this.creating ? t('editor.create') : t('common.save'), { icon: 'check', kind: 'primary', disabled: !this.dirty || this.blocking().length > 0, onClick: () => this.save(), title: 'Ctrl S' }) : badge(t('lib.locked'), 'muted')));
  }
  // ---- validation ----
  localIssues() {
    const d = this.d;
    if (this.kind === 'skill') return validateSkill(d);
    if (this.kind === 'agent') return validateAgent(d);
    if (this.kind === 'rule') return [...(!d.match.trim() ? [{ severity: 'error', key: 'val.rule_match', vars: {} }] : []), ...(this.floorIssue ? [this.floorIssue] : [])];
    if (this.kind === 'mcp') return [...(d.transport === 'stdio' && !d.command?.trim() ? [{ severity: 'error', key: 'val.mcp_command', vars: {} }] : []), ...(d.transport !== 'stdio' && !d.url ? [{ severity: 'error', key: 'val.mcp_url', vars: {} }] : []), ...(d.egress_hosts.includes('*') ? [{ severity: 'error', key: 'val.mcp_wildcard', vars: {} }] : [])];
    if (this.kind === 'instruction') return d.title.trim() ? [] : [{ severity: 'error', key: 'val.title_required', vars: {} }];
    return d.title?.trim() ? [] : [{ severity: 'error', key: 'val.title_required', vars: {} }];
  }
  blocking() { return this.localIssues().filter((i) => i.severity === 'error'); }
  paintIssues() {
    this.paintIssuesHook();
    const all = [...this.localIssues(), ...this.serverIssues];
    clear(this.issuesEl);
    if (!all.length) return;
    this.issuesEl.append(h('ul', { class: 'issue-list' }, all.map((i) => h('li', { class: `note ${i.severity === 'error' ? 'bad' : 'warn'}` }, h('strong', t(`common.sev_${i.severity}`)), ' ', i.text || t(i.key, i.vars)))));
    const b = this.headEl?.querySelector('.btn.primary'); if (b) b.disabled = !this.dirty || this.blocking().length > 0;
  }
  // ---- panel switching ----
  paintPanel() {
    clear(this.panel);
    if (this.tab === 'rendered') { this.panel.append(this.renderedPanel()); return; }
    if (this.tab === 'used') { this.loadInto(this.panel, () => this.usedPanel()); return; }
    this.panel.append(this[`${this.kind}Panel`]());
  }
  // Tags, collections and (for kinds that have them) notes. Collection membership lives in the collections themselves, so it is written through
  // the collection endpoints immediately (not as part of the item PUT); on a not-yet-created item it is applied right after the first save.
  commonSide({ notes = true } = {}) {
    const tags = tagInput(this.d.tags || [], { label: t('editor.tags'), placeholder: t('editor.tags_ph'), validate: validTag, onInvalid: (v) => toast(t('editor.tag_bad', { tag: v }), { kind: 'warn' }), onChange: (v) => { this.d.tags = v; this.touch(); } });
    const colls = h('div', { class: 'row gap wrap' }, ...this.d.groups.map((id) => { const c = this.collections.find((x) => x.id === id); return c ? collectionChip(c) : null; }),
      btn(t('editor.collections_edit'), { icon: 'collections', size: 'sm', onClick: async () => { const r = await pickCollections({ title: t('editor.collections'), collections: this.collections, current: new Set(this.d.groups), confirmLabel: t('common.apply_change') }); if (r) this.setCollections(r); } }));
    const out = [field(t('editor.tags'), tags, t('editor.tags_hint')), h('div', { class: 'field' }, h('span', { class: 'lbl' }, t('editor.collections')), colls, this.creating ? h('div', { class: 'hint' }, t('editor.collections_after_save')) : null)];
    if (notes) out.push(field(t('editor.notes'), h('textarea', { rows: 3, id: 'ed-notes', value: this.d.notes || '', onInput: (e) => { this.d.notes = e.target.value; this.touch(); } }), t('editor.notes_hint')));
    return out;
  }
  async setCollections(chosen) {
    if (this.creating) { this.d.groups = chosen; this.touch(); this.paintPanel(); return; }
    const before = new Set(this.d.groups); const ref = { kind: this.kind, name: this.name };
    try {
      for (const id of chosen) if (!before.has(id)) await this.api.addMembers(id, [ref]);
      for (const id of before) if (!chosen.includes(id)) await this.api.removeMember(id, this.kind, this.name);
      this.d.groups = chosen; toast(t('coll.membership_saved'), { kind: 'ok' }); notifyChanged(); this.paintPanel();
    } catch (e) { toastError(e); }
  }
  descField(rows = 3) {
    const max = 1024; const counter = h('span', { class: 'hint counter' });
    const ta = h('textarea', { id: 'ed-desc', rows, value: this.d.description || '', 'aria-describedby': 'ed-desc-h' });
    const upd = () => { counter.textContent = t('editor.chars', { n: ta.value.length, max }); };
    ta.addEventListener('input', () => { this.d.description = ta.value; upd(); this.touch(); }); upd();
    const f = field(this.kind === 'skill' ? t('editor.skill_desc') : t('editor.desc'), ta, this.kind === 'skill' ? t('editor.skill_desc_hint') : null); f.append(counter); return f;
  }
  // ---- skill ----
  skillPanel() {
    const d = this.d;
    const hasBinary = d.files.some((f) => f.binary);
    const tree = h('ul', { class: 'filetree', role: 'listbox', 'aria-label': t('editor.files') });
    const editorHost = h('div', { class: 'file-editor' });
    const paintTree = () => {
      tree.replaceChildren(...['SKILL.md', ...d.files.map((f) => f.path)].map((p) => h('li', { role: 'option', 'aria-selected': String(p === this.file) },
        h('button', { type: 'button', class: `file-btn${p === this.file ? ' on' : ''}`, onClick: () => { this.file = p; paintTree(); paintEditor(); } }, icon('file', 14), h('span', { class: 'ellipsis' }, p)),
        p !== 'SKILL.md' && !hasBinary ? btn('', { icon: 'trash', kind: 'ghost', size: 'sm', title: t('editor.file_delete', { name: p }), onClick: () => { d.files = d.files.filter((f) => f.path !== p); if (this.file === p) this.file = 'SKILL.md'; this.touch(); paintTree(); paintEditor(); } }) : null)));
    };
    const paintEditor = () => {
      clear(editorHost);
      if (this.file === 'SKILL.md') {
        const fm = h('pre', { class: 'frontmatter', 'aria-label': t('editor.frontmatter') }, skillFrontmatter(d)); this.fm = fm;
        const md = mdEditor({ value: d.body, label: t('editor.body'), mode: 'split', onChange: (v) => { d.body = v; this.touch(); } });
        editorHost.append(h('div', { class: 'row between wrap gap' }, h('h3', t('editor.body')), md.modes), h('p', { class: 'hint' }, t('editor.frontmatter_hint')), fm, md.root);
      } else {
        const f = d.files.find((x) => x.path === this.file);
        const ta = h('textarea', { rows: 18, class: 'md-input', 'aria-label': f.path, spellcheck: 'false', readonly: hasBinary || f.binary, value: f.binary ? t('editor.binary_file') : f.content, onInput: (e) => { f.content = e.target.value; this.touch(); } });
        editorHost.append(h('h3', f.path), ta);
      }
    };
    const addFile = () => {
      const input = h('input', { type: 'text', id: 'nf-path', placeholder: 'references/notes.md', autocomplete: 'off', spellcheck: 'false' }); const err = h('div', { class: 'perr', role: 'alert' });
      const form = h('form', { onSubmit: (e) => { e.preventDefault(); const bad = validateFilePath(input.value, d.files.map((f) => f.path)); if (bad) { err.textContent = t(bad.key); return; } d.files.push({ path: input.value, content: '' }); this.file = input.value; this.touch(); m.close(); paintTree(); paintEditor(); } },
        field(t('editor.file_path'), input, t('editor.file_hint')), err, h('div', { class: 'row gap end' }, btn(t('common.cancel'), { onClick: () => m.close() }), h('button', { type: 'submit', class: 'btn primary' }, t('common.add'))));
      const m = openModal(t('editor.file_add'), form); input.focus();
    };
    const nameField = field(t('editor.name'), h('input', { type: 'text', id: 'ed-name', value: d.name, readonly: true, 'aria-describedby': 'ed-name-h' }), this.creating ? t('editor.name_fixed_new') : t('editor.name_fixed'));
    paintTree(); paintEditor();
    return h('div', { class: 'editor-grid' }, h('div', { class: 'card' }, h('div', { class: 'card-body' }, nameField, this.descField(), ...this.commonSide())),
      h('div', { class: 'card' }, h('div', { class: 'card-body editor-files' }, h('div', { class: 'file-side' }, h('div', { class: 'row between' }, h('h3', t('editor.files')), hasBinary ? null : btn('', { icon: 'plus', size: 'sm', title: t('editor.file_add'), onClick: addFile })), hasBinary ? h('p', { class: 'note info' }, t('editor.binary_note')) : null, tree), editorHost)));
  }
  // Keep the read-only frontmatter preview live while the description changes.
  paintIssuesHook() { if (this.fm) this.fm.textContent = skillFrontmatter(this.d); }
  // ---- agent ----
  agentPanel() {
    const d = this.d;
    const chips = h('div', { class: 'cap-chips', role: 'group', 'aria-label': t('editor.capabilities') });
    const table = h('div', { class: 'tablewrap' });
    const paint = () => {
      chips.replaceChildren(...CAPABILITIES.map((c) => h('button', { type: 'button', class: `cap-chip${d.capabilities.includes(c) ? ' on' : ''}`, 'aria-pressed': String(d.capabilities.includes(c)), title: t(`cap.${c}`),
        onClick: () => { d.capabilities = d.capabilities.includes(c) ? d.capabilities.filter((x) => x !== c) : [...d.capabilities, c]; this.touch(); paint(); } }, d.capabilities.includes(c) ? icon('check', 12) : null, h('span', c))));
      const cs = this.clients.filter((c) => c.caps?.agents !== undefined);
      const prev = cs.map((c) => agentClientPreview({ capabilities: d.capabilities, model_tier: d.model_tier }, c));
      clear(table).append(h('table', { class: 'compact' }, h('thead', h('tr', h('th', { scope: 'col' }, t('editor.client')), d.capabilities.map((c) => h('th', { scope: 'col' }, c)), h('th', { scope: 'col' }, t('editor.model')), h('th', { scope: 'col' }, t('editor.result')))),
        h('tbody', prev.map((p, i) => h('tr', h('th', { scope: 'row' }, cs[i].display_name),
          p.supportsAgents ? p.rows.map((r) => h('td', r.supported ? h('code', r.native) : h('span', { class: 'bad-text', title: t('editor.no_native') }, icon('minus', 14), h('span', { class: 'sr-only' }, t('editor.no_native'))))) : h('td', { colspan: Math.max(1, d.capabilities.length), class: 'muted' }, t('editor.agents_unsupported')),
          h('td', p.supportsAgents && d.model_tier ? (p.model ? h('code', p.model) : h('span', { class: 'muted' }, t('editor.model_default'))) : ''),
          h('td', badge(t(`editor.sev_${p.severity}`), p.severity === 'error' ? 'bad' : p.severity === 'warn' ? 'warn' : p.severity === 'ok' ? 'ok' : 'muted', p.missing.join(', '))))))));
    };
    paint();
    const md = mdEditor({ value: d.body, label: t('editor.body'), mode: 'edit', onChange: (v) => { d.body = v; this.touch(); } });
    return h('div', { class: 'stack-v' }, h('div', { class: 'editor-grid' },
      h('div', { class: 'card' }, h('div', { class: 'card-body' }, field(t('editor.name'), h('input', { type: 'text', value: d.name, readonly: true })), this.descField(),
        field(t('editor.tier'), select([{ value: '', label: t('editor.tier_none') }, ...MODEL_TIERS.map((x) => ({ value: x, label: t(`tier.${x}`) }))], d.model_tier || '', (v) => { d.model_tier = v; this.touch(); paint(); }, { id: 'ed-tier' }), t('editor.tier_hint')),
        field(t('editor.mode_label'), select(['subagent', 'primary'].map((x) => ({ value: x, label: t(`agentmode.${x}`) })), d.mode, (v) => { d.mode = v; this.touch(); }, { id: 'ed-mode' })),
        h('label', { class: 'check' }, h('input', { type: 'checkbox', checked: d.read_only, onChange: (e) => { d.read_only = e.target.checked; this.touch(); } }), h('span', t('editor.read_only'))), ...this.commonSide({ notes: false }))),
      h('div', { class: 'card' }, h('div', { class: 'card-body' }, h('h3', t('editor.capabilities')), h('p', { class: 'hint' }, t('editor.capabilities_hint')), chips, h('h3', t('editor.mapping')), h('p', { class: 'hint' }, t('editor.mapping_hint')), table))),
      h('div', { class: 'card' }, h('div', { class: 'card-body' }, h('h3', t('editor.agent_prompt')), md.root)));
  }
  // ---- instruction ----
  instructionPanel() {
    const d = this.d;
    const md = mdEditor({ value: d.body, label: t('editor.body'), mode: 'split', onChange: (v) => { d.body = v; this.touch(); } });
    const applies = h('div', { class: 'enable-list' }, this.clients.map((c) => h('label', { class: 'check' }, h('input', { type: 'checkbox', checked: d.applies_to.includes(c.id), onChange: (e) => { d.applies_to = e.target.checked ? [...d.applies_to, c.id] : d.applies_to.filter((x) => x !== c.id); this.touch(); } }), h('span', c.display_name))));
    const order = h('div', { class: 'card' }, h('div', { class: 'card-body' }, h('h3', t('editor.order')), h('p', { class: 'hint' }, t('editor.order_hint'))));
    this.orderList(order.firstChild);
    return h('div', { class: 'stack-v' }, h('div', { class: 'editor-grid' },
      h('div', { class: 'card' }, h('div', { class: 'card-body' }, field(t('editor.name'), h('input', { type: 'text', value: d.name, readonly: true })), field(t('editor.title'), h('input', { type: 'text', id: 'ed-title', value: d.title, onInput: (e) => { d.title = e.target.value; this.touch(); } })),
        h('div', { class: 'field' }, h('span', { class: 'lbl' }, t('editor.applies_to')), applies, h('div', { class: 'hint' }, t('editor.applies_hint'))), ...this.commonSide({ notes: false }))), order),
      h('div', { class: 'card' }, h('div', { class: 'card-body' }, h('div', { class: 'row between wrap gap' }, h('h3', t('editor.body')), md.modes), md.root)));
  }
  async orderList(host) {
    const holder = h('div'); host.append(holder);
    await this.loadInto(holder, async () => {
      const shells = await this.api.items('instruction');
      let list = (await Promise.all(shells.map(async (x) => { const dd = detailDraft('instruction', await this.api.item('instruction', x.name)); return { name: x.name, title: dd.title, order: dd.order }; }))).sort((a, b) => a.order - b.order);
      if (!list.some((x) => x.name === this.name)) list = [...list, { name: this.name, title: this.d.title, order: this.d.order }];
      const ul = h('ol', { class: 'order-list', 'aria-label': t('editor.order') });
      let drag = null;
      const save = async (ids) => {
        try { const map = Object.fromEntries(list.map((x) => [x.name, x])); for (const { id, order } of renumber(ids)) { if (map[id].order !== order && id !== this.name) { const fd = detailDraft('instruction', await this.api.item('instruction', id)); await this.api.saveItem('instruction', id, toPutBody('instruction', { ...fd, order })); } } this.d.order = renumber(ids).find((x) => x.id === this.name).order; list = ids.map((id) => ({ ...map[id], order: renumber(ids).find((x) => x.id === id).order })); paint(); if (!this.creating) this.touch(); notifyChanged(); toast(t('editor.order_saved'), { kind: 'ok' }); } catch (e) { toastError(e); }
      };
      const paint = () => ul.replaceChildren(...list.map((x, i) => {
        const li = h('li', { class: `order-row${x.name === this.name ? ' current' : ''}`, draggable: 'true', dataset: { id: x.name } },
          h('span', { class: 'grip', 'aria-hidden': 'true' }, icon('grip', 14)), h('span', { class: 'order-n' }, String(i + 1)), h('span', { class: 'ellipsis grow' }, x.title || x.name), x.name === this.name ? badge(t('editor.this_one'), 'info') : null,
          btn('', { icon: 'up', kind: 'ghost', size: 'sm', title: t('coll.member_up', { name: x.name }), disabled: i === 0, onClick: () => save(moveBy(list.map((y) => y.name), x.name, -1)) }),
          btn('', { icon: 'down', kind: 'ghost', size: 'sm', title: t('coll.member_down', { name: x.name }), disabled: i === list.length - 1, onClick: () => save(moveBy(list.map((y) => y.name), x.name, 1)) }));
        li.addEventListener('dragstart', (e) => { drag = x.name; e.dataTransfer.setData('text/plain', x.name); li.classList.add('dragging'); });
        li.addEventListener('dragend', () => { drag = null; ul.querySelectorAll('.dragging,.drop-before,.drop-after').forEach((n) => n.classList.remove('dragging', 'drop-before', 'drop-after')); });
        li.addEventListener('dragover', (e) => { if (!drag) return; e.preventDefault(); ul.querySelectorAll('.drop-before,.drop-after').forEach((n) => n.classList.remove('drop-before', 'drop-after')); const r = li.getBoundingClientRect(); li.classList.add(dropPosition(r.top, r.height, e.clientY) === 'before' ? 'drop-before' : 'drop-after'); });
        li.addEventListener('drop', (e) => { if (!drag) return; e.preventDefault(); const r = li.getBoundingClientRect(); const ids = moveRelative(list.map((y) => y.name), drag, x.name, dropPosition(r.top, r.height, e.clientY)); drag = null; save(ids); });
        return li;
      }));
      paint();
      return ul;
    });
  }
  // ---- mcp ----
  mcpPanel() {
    const d = this.d; const it = this.item;
    const scan = it?.scan || { status: 'unscanned', findings: [] };
    const scanCard = h('div', { class: 'card' }, h('div', { class: 'card-body' }, h('div', { class: 'row between wrap gap' }, h('h3', t('mcp.scan')), badge(t(`status.${scan.status}`), scan.status === 'clean' ? 'ok' : scan.status === 'findings' ? 'bad' : 'warn')),
      scan.status === 'clean' ? h('p', { class: 'muted' }, t('mcp.scan_clean', { ref: d.pinned_ref || '?' })) : h('p', { class: 'note warn' }, t(scan.status === 'findings' ? 'mcp.scan_findings_hint' : 'mcp.scan_unscanned_hint')),
      scan.findings?.length ? h('ul', { class: 'findings' }, scan.findings.map((f) => h('li', { class: `note ${f.severity === 'error' ? 'bad' : 'warn'}` }, h('strong', t(`common.sev_${f.severity}`)), ' ', f.message))) : null,
      h('p', { class: 'hint' }, t('mcp.scan_cmd')), h('code', { class: 'ro' }, `hub scan-mcp ${this.name}`)));
    const enabled = h('div', { class: 'card' }, h('div', { class: 'card-body' }, h('h3', t('mcp.enable_title')), h('p', { class: 'hint' }, t('mcp.enable_hint')))); if (!this.creating) this.loadInto(enabled.firstChild.appendChild(h('div')), () => enabledCard('mcp', this.name));
    const args = h('textarea', { rows: 3, id: 'ed-args', 'aria-describedby': 'ed-args-h', value: d.args.join('\n'), onInput: (e) => { d.args = e.target.value.split('\n').map((x) => x.trim()).filter(Boolean); this.touch(); } });
    return h('div', { class: 'editor-grid' },
      h('div', { class: 'card' }, h('div', { class: 'card-body' }, field(t('editor.name'), h('input', { type: 'text', value: d.name, readonly: true })),
        field(t('mcp.transport'), select(['stdio', 'http', 'sse'].map((x) => ({ value: x, label: x })), d.transport, (v) => { d.transport = v; this.touch(); this.paintPanel(); }, { id: 'ed-transport' })),
        d.transport === 'stdio' ? [field(t('mcp.command'), h('input', { type: 'text', id: 'ed-cmd', value: d.command || '', autocomplete: 'off', onInput: (e) => { d.command = e.target.value; this.touch(); } })), field(t('mcp.args'), args, t('mcp.args_hint'))]
          : field(t('mcp.url'), h('input', { type: 'text', id: 'ed-url', value: d.url || '', autocomplete: 'off', onInput: (e) => { d.url = e.target.value; this.touch(); } })),
        field(t('mcp.env_names'), tagInput(d.env_names, { label: t('mcp.env_names'), placeholder: 'GITHUB_TOKEN', onChange: (v) => { d.env_names = v; this.touch(); } }), t('mcp.env_hint')),
        field(t('mcp.egress'), tagInput(d.egress_hosts, { label: t('mcp.egress'), placeholder: 'api.example.com', onChange: (v) => { d.egress_hosts = v; this.touch(); } }), t('mcp.egress_hint')),
        field(t('mcp.sandbox'), h('input', { type: 'text', id: 'ed-sandbox', value: d.sandbox_profile || '', onInput: (e) => { d.sandbox_profile = e.target.value; this.touch(); } })),
        field(t('mcp.pinned'), h('input', { type: 'text', id: 'ed-pin', value: d.pinned_ref || '', onInput: (e) => { d.pinned_ref = e.target.value; this.touch(); } }), t('mcp.pinned_hint')), ...this.commonSide())),
      h('div', { class: 'stack-v' }, scanCard, enabled));
  }
  // ---- rule (guided) ----
  rulePanel() {
    const d = this.d;
    const KINDS = { tool: ['read', 'write', 'edit', 'shell', 'web_fetch', 'web_search', 'mcp', 'subagent', 'browser'], command: null, path_read: null, path_write: null, egress_host: null, mcp_server: null };
    const matchInput = h('input', { type: 'text', id: 'ed-match', value: d.match, autocomplete: 'off', spellcheck: 'false', 'aria-describedby': 'ed-match-h', onInput: (e) => { d.match = e.target.value; this.touch(); this.checkFloor(); } });
    const kindSel = select(Object.keys(KINDS).map((k) => ({ value: k, label: t(`rule.kind_${k}`) })), d.rule_kind, (v) => { d.rule_kind = v; this.touch(); this.checkFloor(); this.paintPanel(); }, { id: 'ed-rkind' });
    const decSel = select(['allow', 'ask', 'deny'].map((k) => ({ value: k, label: t(`rule.dec_${k}`) })), d.decision, (v) => { d.decision = v; this.touch(); this.checkFloor(); }, { id: 'ed-decision' });
    const applies = h('div', { class: 'enable-list' }, this.clients.map((c) => h('label', { class: 'check' }, h('input', { type: 'checkbox', checked: d.applies_to.includes(c.id), onChange: (e) => { d.applies_to = e.target.checked ? [...d.applies_to, c.id] : d.applies_to.filter((x) => x !== c.id); this.touch(); } }), h('span', c.display_name))));
    const floorHost = h('div');
    this.loadInto(floorHost, async () => this.floorTable(floorRows(await this.api.floor())));
    return h('div', { class: 'editor-grid' },
      h('div', { class: 'card' }, h('div', { class: 'card-body' }, h('p', { class: 'rule-sentence' }, t('rule.sentence')),
        field(t('editor.title'), h('input', { type: 'text', id: 'ed-rtitle', value: d.title, onInput: (e) => { d.title = e.target.value; this.touch(); } })),
        field(t('rule.when'), kindSel), field(t('rule.match'), matchInput, t(`rule.match_hint_${d.rule_kind}`)), field(t('rule.then'), decSel),
        field(t('rule.reason'), h('textarea', { rows: 2, id: 'ed-reason', value: d.reason, onInput: (e) => { d.reason = e.target.value; this.touch(); } }), t('rule.reason_hint')),
        h('div', { class: 'field' }, h('span', { class: 'lbl' }, t('editor.applies_to')), applies, h('div', { class: 'hint' }, t('editor.applies_hint'))))),
      h('div', { class: 'card' }, h('div', { class: 'card-body' }, h('h3', t('rule.floor_title')), h('p', { class: 'hint' }, t('rule.floor_hint')), floorHost)));
  }
  floorTable(rules) {
    return h('div', { class: 'tablewrap' }, h('table', { class: 'compact floor-table' }, h('thead', h('tr', ['', t('rule.col_rule'), t('rule.col_decision'), t('rule.col_why')].map((c, i) => h('th', { scope: 'col' }, i ? c : h('span', { class: 'sr-only' }, t('lib.locked')))))),
      h('tbody', rules.map((r) => h('tr', { class: 'locked-row' }, h('td', h('span', { class: 'lock', title: t('rules.locked_why') }, icon('lock', 16))), h('td', h('strong', isFloorKey(r.title) ? t(r.title) : r.title), h('div', { class: 'muted small' }, `${t(`rule.kind_${r.kind}`)}: `, h('code', r.match))), h('td', badge(t(`rule.dec_${r.decision}`), r.decision === 'deny' ? 'bad' : r.decision === 'ask' ? 'warn' : 'ok'), r.extra ? h('div', { class: 'muted small' }, t('floor.tool_default', { value: t(`rule.dec_${r.extra}`) })) : null), h('td', { class: 'small' }, isFloorKey(r.reason) ? t(r.reason) : r.reason))))));
  }
  async checkFloor() {
    const d = this.d; clearTimeout(this._ft);
    this._ft = setTimeout(async () => {
      try { const r = await this.api.checkPolicy({ rules: [{ kind: d.rule_kind, match: d.match, decision: d.decision }] });
        this.floorIssue = r.violations?.length ? { severity: 'error', key: null, text: `${t('rule.floor_violation')} ${r.violations[0].message}`, vars: {} } : null; this.paintIssues(); } catch { /* the server re-checks on save */ }
    }, 250);
  }
  // ---- memory ----
  memoryPanel() {
    const d = this.d; const md = mdEditor({ value: d.body, label: t('editor.body'), mode: 'split', onChange: (v) => { d.body = v; this.touch(); } });
    return h('div', { class: 'stack-v' }, h('div', { class: 'card' }, h('div', { class: 'card-body' }, h('p', { class: 'note info' }, t('editor.memory_note')),
      field(t('editor.title'), h('input', { type: 'text', id: 'ed-mtitle', value: d.title, onInput: (e) => { d.title = e.target.value; this.touch(); } })),
      field(t('inbox.type'), select(['user', 'feedback', 'project', 'reference'].map((x) => ({ value: x, label: t(`memtype.${x}`) })), d.type, (v) => { d.type = v; this.touch(); }, { id: 'ed-mtype' })))),
      h('div', { class: 'card' }, h('div', { class: 'card-body' }, h('div', { class: 'row between wrap gap' }, h('h3', t('editor.body')), md.modes), md.root)));
  }
  // ---- rendered + used-by ----
  renderedPanel() {
    const supp = this.clients.filter((c) => c.caps?.[CONCERN[this.kind]]);
    if (this.creating) return emptyBox(t('editor.rendered_save_first'), t('editor.rendered_save_hint'));
    if (!supp.length) return emptyBox(t('editor.rendered_none'), t('editor.rendered_none_hint'));
    const out = h('div', { class: 'rendered-out' });
    const sel = select(supp.map((c) => ({ value: c.id, label: c.display_name })), this.renderClient || supp[0].id, (v) => { this.renderClient = v; go(); }, { id: 'rendered-client' });
    const go = () => this.loadInto(out, async () => this.renderedFor(sel.value || supp[0].id));
    setTimeout(go, 0);
    return h('div', { class: 'stack-v' }, h('div', { class: 'row gap wrap bottom' }, field(t('editor.rendered_for'), sel), h('p', { class: 'hint grow' }, t('editor.rendered_hint'))), out);
  }
  async renderedFor(clientId) {
    const cname = this.clients.find((c) => c.id === clientId)?.display_name;
    let files, diags = [];
    try { const r = await this.api.rendered(clientId, this.kind, this.name); files = r.files; diags = r.diagnostics || []; }
    catch (e) {
      if (e.status !== 404 && e.status !== 405) throw e;
      // Fallback for backends without /rendered: rebuild what we can from the plan (built from COMMITTED content). A file that is new to the
      // client shows its full content; a changed one shows the diff; an unchanged one is already exactly what the client has.
      const p = await this.api.plan([clientId]); const cp = p.clients[0];
      files = cp.files.filter((f) => (f.source_ids || []).includes(this.name)).map((f) => (f.action === 'add' ? { root: f.root, path: f.path, merge: f.merge, managed: f.managed !== false, content: parseUnified(f.diff).filter((l) => l.type === 'add').map((l) => l.text).join('\n') }
        : { root: f.root, path: f.path, merge: f.merge, managed: f.managed !== false, diff: f.diff, note: f.action }));
      diags = (cp.diagnostics || []).filter((x) => x.item === this.name);
    }
    if (!files.length) return emptyBox(t('editor.rendered_empty', { client: cname }), t('editor.rendered_empty_hint'), h('a', { class: 'btn', href: '#/matrix' }, t('nav.matrix')));
    return [h('p', { class: 'note info' }, t('editor.rendered_committed')), diags.length ? h('ul', { class: 'issue-list' }, diags.map((x) => h('li', { class: `note ${x.severity === 'error' ? 'bad' : 'warn'}` }, h('strong', t(`common.sev_${x.severity}`)), ' ', x.message))) : null,
      ...files.map((f) => h('section', { class: 'card' }, h('header', { class: 'card-head' }, h('h3', h('code', `${f.root}: ${f.path}`)), h('div', { class: 'row gap' }, badge(f.merge || 'own', 'muted', t('editor.merge_tip')), f.managed === false ? badge(t('plan.a_advisory'), 'info') : badge(t('editor.managed'), 'ok'))),
        h('div', { class: 'card-body' }, f.content != null ? h('pre', { class: 'cmd rendered', tabindex: '0', 'aria-label': f.path }, f.content) : [h('p', { class: 'muted small' }, t(f.note === 'unchanged' ? 'editor.rendered_same' : 'editor.rendered_diff')), f.diff ? diffView(f.diff) : null])))];
  }
  async usedPanel() {
    const c = await enabledCard(this.kind, this.name).catch(() => null);
    return h('div', { class: 'stack-v' }, h('div', { class: 'card' }, h('div', { class: 'card-body' }, h('h3', t('editor.in_collections')), this.d.groups.length ? h('div', { class: 'row gap wrap' }, this.d.groups.map((id) => { const co = this.collections.find((x) => x.id === id); return co ? collectionChip(co) : null; })) : h('p', { class: 'muted' }, t('editor.in_no_collection')))),
      h('div', { class: 'card' }, h('div', { class: 'card-body' }, h('h3', t('mcp.enable_title')), h('p', { class: 'hint' }, t('editor.used_hint')), c || h('p', { class: 'muted' }, t('editor.enable_na')))));
  }
  // ---- actions ----
  async save() {
    if (this.locked || !this.dirty || this.blocking().length) return;
    try {
      this.serverIssues = [];
      const saved = await this.api.saveItem(this.kind, this.name, this.toBody());
      toast(t(this.creating ? 'editor.created' : 'editor.saved', { name: this.name }), { kind: 'ok' });
      this.dirty = false; notifyChanged();
      if (this.creating) { for (const id of this.d.groups) { try { await this.api.addMembers(id, [{ kind: this.kind, name: this.name }]); } catch (e) { toastError(e); } } location.hash = `#/edit/${this.kind}/${encodeURIComponent(this.name)}`; return; }
      this.item = saved; this.paintHead(); this.paintIssues();
    } catch (e) {
      if (e.status === 422 && e.problem?.errors) { this.serverIssues = e.problem.errors.map((x) => ({ severity: 'error', text: `${(x.loc || []).filter((p) => p !== 'body').join('.')}: ${x.msg}` })); this.paintIssues(); }
      else toastError(e);
    }
  }
  async duplicate() {
    const input = h('input', { type: 'text', id: 'dup-name', value: `${this.name}-copy`, autocomplete: 'off', spellcheck: 'false' }); const err = h('div', { class: 'perr', role: 'alert' });
    const form = h('form', { onSubmit: async (e) => { e.preventDefault(); if (!validName(input.value)) { err.textContent = t('new.bad_name'); return; } try { await this.api.duplicateSkill(this.name, input.value); m.close(); notifyChanged(); location.hash = `#/edit/skill/${encodeURIComponent(input.value)}`; } catch (x) { toastError(x); } } },
      field(t('editor.duplicate_name'), input), err, h('div', { class: 'row gap end' }, btn(t('common.cancel'), { onClick: () => m.close() }), h('button', { type: 'submit', class: 'btn primary' }, t('editor.duplicate'))));
    const m = openModal(t('editor.duplicate'), form); input.select();
  }
  async remove() {
    if (!(await confirmDialog(t('editor.delete_confirm', { name: this.name }), { danger: true, confirmLabel: t('common.delete') }))) return;
    try { await this.api.deleteItem(this.kind, this.name); this.dirty = false; toast(t('editor.deleted', { name: this.name }), { kind: 'ok' }); notifyChanged(); location.hash = '#/library'; } catch (e) { toastError(e); }
  }
}

// Chips + text input. Enter or comma adds; Backspace on an empty input removes the last chip.
export function tagInput(values, { label, placeholder, onChange, validate = () => true, onInvalid = () => {} }) {
  let list = [...values];
  const wrap = h('div', { class: 'tag-input' });
  const input = h('input', { type: 'text', placeholder, 'aria-label': label, autocomplete: 'off', spellcheck: 'false' });
  const add = (v) => { v = v.trim().replace(/,$/, '').trim(); if (v && !validate(v)) { onInvalid(v); input.value = ''; return; } if (v && !list.includes(v)) { list = [...list, v]; onChange(list); paint(); wrap.querySelector('input').focus(); } else input.value = ''; };
  input.addEventListener('keydown', (e) => { if (e.key === 'Enter' || e.key === ',') { e.preventDefault(); add(input.value); } else if (e.key === 'Backspace' && !input.value && list.length) { list = list.slice(0, -1); onChange(list); paint(); wrap.querySelector('input').focus(); } });
  input.addEventListener('blur', () => { if (input.value.trim()) add(input.value); });
  const paint = () => wrap.replaceChildren(...list.map((v) => chip(v, { onRemove: () => { list = list.filter((x) => x !== v); onChange(list); paint(); } })), input);
  paint();
  return wrap;
}
void errorBox;
customElements.define('ah-editor', AhEditor);
