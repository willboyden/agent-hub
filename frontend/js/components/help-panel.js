// The Getting started and FAQ tabs every page gets (help-tabs.js). `help` = { key: the entry in i18n/help.en.json,
// name: the page's display name, nav, extra? }. Content comes from i18n/help.en.json (see help.js); all of it is
// rendered as text nodes, `code` spans included. A step or answer may carry a command (shown with a Copy button,
// never run), a link to another tab of the same page (shown only when the page has that tab) and a link to another
// page. `alive()` says whether the view is still mounted (a late fetch must not paint into a detached view).
import { h, clear, append } from '../dom.js';
import { t } from '../i18n.js';
import { loadHelp, richRuns, filterFaq, HUB_KEY } from '../help.js';
import { card, btn, emptyBox, errorBox, skeleton, fixBlock } from './ui.js';

const inline = (runs) => runs.map((r) => (r.code ? h('code', r.text) : r.text));
/** Text with `code` spans as one <p> per paragraph. */
export function paras(text, cls) {
  return richRuns(text).map((runs) => h('p', { class: cls }, inline(runs)));
}

/** Command, tab link and page link of a step or answer. */
function extras(item, nav) {
  const links = [
    item.tab && nav.hasTab(item.tab)
      ? btn(t('help.open_tab', { tab: nav.tabLabel(item.tab) }), { size: 'sm', onClick: () => nav.goTab(item.tab) })
      : null,
    item.route
      ? h(
          'a',
          { class: 'btn sm', href: `#${item.route}` },
          t('help.open_page', { page: t(`nav.${item.route.slice(1).split(/[/?]/)[0] || 'home'}`) })
        )
      : null,
  ].filter(Boolean);
  return [
    item.cmd ? fixBlock(item.cmd, t('help.command')) : null,
    links.length ? h('div', { class: 'row gap wrap' }, links) : null,
  ];
}

/** Shared loader: skeleton, then build(doc), or an error box with Retry. */
function helpHost(alive, build) {
  const host = h('div', { class: 'stack-v' }, skeleton(3));
  async function load() {
    try {
      const doc = await loadHelp();
      if (!alive()) return;
      append(clear(host), [build(doc)]);
    } catch (e) {
      if (alive())
        clear(host).append(
          errorBox(e, () => {
            clear(host).append(skeleton(3));
            load();
          })
        );
    }
  }
  load();
  return host;
}

export function gettingStartedPanel(alive, { key, name, nav, extra }) {
  return helpHost(alive, (doc) => {
    const e = doc[key];
    if (!e?.steps?.length) return emptyBox(t('help.missing'));
    return [
      extra ? extra() : null,
      card(
        t('help.start_title', { name }),
        h(
          'div',
          { class: 'stack-v' },
          paras(e.lead, 'help-lead'),
          h(
            'ol',
            { class: 'help-steps' },
            e.steps.map((s) =>
              h(
                'li',
                { class: 'help-step' },
                h('h3', { class: 'help-step-title' }, s.title),
                h('div', { class: 'help-step-body' }, paras(s.body), extras(s, nav))
              )
            )
          )
        )
      ),
      h(
        'div',
        { class: 'row gap wrap' },
        h('span', { class: 'hint' }, t('help.start_more')),
        nav.hasTab('faq')
          ? btn(t('help.open_tab', { tab: nav.tabLabel('faq') }), { size: 'sm', onClick: () => nav.goTab('faq') })
          : null
      ),
    ];
  });
}

const faqItem = (it, nav) =>
  h(
    'details',
    { class: 'faq-item' },
    h('summary', inline(richRuns(it.q).flat())),
    h('div', { class: 'faq-a' }, paras(it.a), extras(it, nav))
  );

export function faqPanel(alive, { key, name, nav }) {
  return helpHost(alive, (doc) => {
    // The hub-wide questions follow, minus any whose only link is to this very page.
    const own = doc[key]?.faq || [],
      common = (doc[HUB_KEY]?.faq || []).filter((it) => it.route !== `/${key}`);
    const input = h('input', {
      type: 'search',
      class: 'help-filter',
      placeholder: t('help.filter_ph'),
      'aria-label': t('help.filter'),
      autocomplete: 'off',
    });
    const status = h('p', { class: 'muted small', role: 'status', 'aria-live': 'polite' });
    const ownList = h('div', { class: 'faq' }),
      commonList = h('div', { class: 'faq' });
    const ownCard = card(t('help.faq_title', { name }), ownList);
    const commonCard = card(t('help.faq_hub'), commonList);
    const render = () => {
      const q = input.value.trim();
      const a = filterFaq(own, q),
        b = filterFaq(common, q);
      append(
        clear(ownList),
        a.length ? a.map((it) => faqItem(it, nav)) : [emptyBox(q ? t('help.no_match') : t('help.missing'))]
      );
      append(
        clear(commonList),
        b.map((it) => faqItem(it, nav))
      );
      if (q) for (const d of [...ownList.children, ...commonList.children]) if (d.tagName === 'DETAILS') d.open = true;
      commonCard.hidden = !b.length;
      status.textContent = q ? t('help.match_count', { count: a.length + b.length }) : '';
    };
    input.addEventListener('input', render);
    const toggle = (open) => {
      for (const d of [...ownList.children, ...commonList.children]) if (d.tagName === 'DETAILS') d.open = open;
    };
    render();
    return [
      h(
        'div',
        { class: 'row gap wrap between' },
        input,
        h(
          'div',
          { class: 'row gap' },
          btn(t('help.expand_all'), { size: 'sm', onClick: () => toggle(true) }),
          btn(t('help.collapse_all'), { size: 'sm', onClick: () => toggle(false) })
        )
      ),
      status,
      ownCard,
      commonCard,
    ];
  });
}
