// Getting started + FAQ on every page. A page with its own tab bar appends helpTabItems() to it and, when one of
// those tabs is picked, calls renderHelp() instead of painting its own panel. A page without tabs gets one bar from
// helpFrame(): "<Page>" | Getting started | FAQ; its own content stays mounted (only hidden) while a help tab shows,
// so filters and scroll position survive. `?tab=start` / `?tab=faq` in the route opens a help tab directly.
import { h, clear } from '../dom.js';
import { t } from '../i18n.js';
import { tabs } from './ui.js';
import { gettingStartedPanel, faqPanel } from './help-panel.js';

export const HELP_TABS = ['start', 'faq'];
export const isHelpTab = (id) => HELP_TABS.includes(id);
export const helpTabItems = () => HELP_TABS.map((id) => ({ id, label: t(`tab.${id}`) }));

/**
 * Paint a help tab into `panel`. `bar` is the page's tab bar (help links click its tabs); `key` the help entry;
 * `extra()` an optional node shown above the guide (Home's live checklist).
 */
export function renderHelp(view, panel, id, { key, name, bar, extra }) {
  const button = (x) => bar?.querySelector(`[role=tab][data-id="${x}"]`);
  const nav = {
    hasTab: (x) => !!button(x),
    tabLabel: (x) => button(x)?.firstChild?.textContent ?? x,
    goTab: (x) => button(x)?.click(),
  };
  const alive = () => view._alive;
  clear(panel).append(
    id === 'start' ? gettingStartedPanel(alive, { key, name, nav, extra }) : faqPanel(alive, { key, name, nav })
  );
}

/** The tab bar and the two panels for a page that has no tabs of its own. Append `bar`, `own`, `help`; fill `own`. */
export function helpFrame(view, { key, name, extra }) {
  const own = h('div', { role: 'tabpanel', class: 'page-own', 'aria-labelledby': 'tab-page' });
  const help = h('div', { role: 'tabpanel', class: 'help-pane', hidden: true });
  const active = isHelpTab(view.query?.tab) ? view.query.tab : 'page';
  const show = (id) => {
    const mine = id === 'page';
    own.hidden = !mine;
    help.hidden = mine;
    if (mine) {
      clear(help);
      return;
    }
    help.setAttribute('aria-labelledby', `tab-${id}`);
    renderHelp(view, help, id, { key, name, bar, extra });
  };
  const bar = tabs([{ id: 'page', label: name }, ...helpTabItems()], active, show, t('help.page_tabs', { name }));
  if (active !== 'page') show(active);
  return { bar, own, help };
}
