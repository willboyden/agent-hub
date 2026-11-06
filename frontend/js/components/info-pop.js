// The (i) info pop-up shared by the config editor's Form view and the Settings fields. One pop-up per owner, filled
// for the button that opened it: a manual popover in the browser's top layer where the Popover API exists, else a
// fixed box; positioned by script next to its button; Esc, a click outside and the same button again close it, and
// focus goes back to the button. The button carries aria-expanded / aria-controls; the pop-up is a labelled dialog.
// Content is built from text nodes only (help text with `code` spans, never HTML).
import { h, clear, icon } from '../dom.js';
import { t } from '../i18n.js';
import { richRuns } from '../help.js';
import { btn } from './ui.js';

let uid = 0;

/** Help text as paragraphs, `x` as <code>. */
export const richParas = (text) =>
  richRuns(text).map((runs) =>
    h(
      'p',
      runs.map((r) => (r.code ? h('code', r.text) : r.text))
    )
  );
/** A titled block of the pop-up body. */
export const infoBlock = (title, ...body) => h('section', { class: 'fh-block' }, h('h4', title), ...body);

/** The round (i) button. `label` is its accessible name ("About plan_retention"). */
export function infoButton(label, onClick, { text, cls = '' } = {}) {
  return h(
    'button',
    {
      type: 'button',
      class: `ff-info ${cls}`.trim(),
      'aria-label': text ? null : label,
      title: label,
      'aria-haspopup': 'dialog',
      'aria-expanded': 'false',
      onClick: (e) => onClick(e.currentTarget),
    },
    icon('info', 15),
    text ? h('span', text) : null
  );
}

/**
 * createInfoPop() -> { el, toggle(button, fill), close(refocus) }. Append `el` once (anywhere in the owner).
 * fill(head, body, titleId) builds the content: put the title (with id `titleId`) into `head`, blocks into `body`.
 */
export function createInfoPop() {
  const popId = `ffpop${++uid}`;
  const pop = h('div', {
    class: 'ff-pop',
    id: popId,
    role: 'dialog',
    'aria-labelledby': `${popId}-t`,
    tabindex: '-1',
    hidden: true,
  });
  const hasPopover = typeof pop.showPopover === 'function';
  if (hasPopover) {
    pop.setAttribute('popover', 'manual');
    pop.hidden = false;
  }
  let popFor = null; // the button the pop-up belongs to
  const isOpen = () => (hasPopover ? pop.matches(':popover-open') : !pop.hidden);

  function place() {
    if (!popFor || !isOpen()) return;
    const a = popFor.getBoundingClientRect(),
      vw = document.documentElement.clientWidth,
      vh = window.innerHeight,
      m = 8;
    const w = Math.min(420, vw - 2 * m);
    pop.style.width = `${w}px`;
    pop.style.maxHeight = `${Math.max(160, vh - 2 * m)}px`;
    // open toward the side with room: from the button's left edge when it sits in the left half, else ending at its right
    const want = a.left + a.width / 2 < vw / 2 ? a.left : a.right - w;
    const left = Math.min(Math.max(m, want), vw - w - m);
    const ph = pop.offsetHeight;
    const below = a.bottom + 6,
      above = a.top - 6 - ph;
    const top = below + ph <= vh - m || above < m ? Math.min(below, Math.max(m, vh - m - ph)) : above;
    pop.style.left = `${left}px`;
    pop.style.top = `${top}px`;
  }
  const onKey = (e) => {
    if (!pop.isConnected) {
      close(false);
      return;
    }
    if (e.key === 'Escape' && isOpen()) {
      e.preventDefault();
      e.stopPropagation();
      close(true);
    }
  };
  const onDown = (e) => {
    if (!pop.isConnected) {
      close(false);
      return;
    }
    if (isOpen() && !pop.contains(e.target) && e.target !== popFor && !popFor?.contains(e.target)) close(false);
  };
  function close(refocus) {
    if (!popFor) return;
    const b = popFor;
    popFor = null;
    b.setAttribute('aria-expanded', 'false');
    if (hasPopover) {
      if (pop.matches(':popover-open')) pop.hidePopover();
    } else pop.hidden = true;
    document.removeEventListener('keydown', onKey, true);
    document.removeEventListener('pointerdown', onDown, true);
    window.removeEventListener('resize', place);
    window.removeEventListener('scroll', place, true);
    if ((refocus || pop.contains(document.activeElement) || document.activeElement === document.body) && b.isConnected)
      b.focus();
  }
  function toggle(btnEl, fill) {
    if (popFor === btnEl) {
      close(true);
      return;
    }
    close(false);
    clear(pop);
    const head = h('div', { class: 'fh-head' });
    const body = h('div', { class: 'fh-body' });
    fill(head, body, `${popId}-t`);
    head.append(
      btn('', { icon: 'close', size: 'sm', kind: 'ghost', title: t('fh.close'), onClick: () => close(true) })
    );
    pop.append(head, body);
    popFor = btnEl;
    btnEl.setAttribute('aria-expanded', 'true');
    btnEl.setAttribute('aria-controls', popId);
    if (hasPopover) pop.showPopover();
    else pop.hidden = false;
    place();
    document.addEventListener('keydown', onKey, true);
    document.addEventListener('pointerdown', onDown, true);
    window.addEventListener('resize', place);
    window.addEventListener('scroll', place, true);
    pop.focus({ preventScroll: true });
  }
  return { el: pop, toggle, close, isOpen };
}

/**
 * The pop-up content for one catalog entry (i18n/fields.en.json): {what, change, accepts, default, effect}. `title` is
 * the key path shown at the top; `extra` more blocks (the draft's value, a comment from the file).
 */
export function fillEntry(entry, { title, extra = [], missing = t('fh.missing') } = {}) {
  return (head, body, titleId) => {
    head.append(h('h3', { id: titleId, class: 'fh-title' }, h('code', title)));
    if (!entry) {
      body.append(h('p', { class: 'note info' }, missing));
      body.append(...extra.filter(Boolean));
      return;
    }
    if (entry.what) body.append(infoBlock(t('fh.what'), ...richParas(entry.what)));
    if (entry.change) body.append(infoBlock(t('fh.change'), ...richParas(entry.change)));
    if (entry.accepts || entry.default != null) {
      const dflt = entry.default != null ? richParas(t('fh.default_is', { v: entry.default })) : [];
      for (const p of dflt) p.classList.add('muted', 'small');
      body.append(infoBlock(t('fh.accepts'), ...(entry.accepts ? richParas(entry.accepts) : []), ...dflt));
    }
    if (entry.effect) body.append(infoBlock(t('fh.effect'), ...richParas(entry.effect)));
    body.append(...extra.filter(Boolean));
  };
}
