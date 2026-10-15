// SkillEvaluator badge for a skill (Library cards and list, editor). A link to the skill's Evaluation tab, so it is keyboard
// reachable. The visible text names the verdict or the state (colour is only a hint); a screen-reader-only sentence follows it,
// so the accessible name starts with the visible text (WCAG "label in name") and adds the grade, judge score and staleness.
import { h, icon } from '../dom.js';
import { t } from '../i18n.js';
import { badgeModel, badgeText } from '../skilleval.js';

export function evalBadge(entry, name) {
  const m = badgeModel(entry);
  const { short, long } = badgeText(m, t);
  return h('a', { class: `badge eval-badge ${m.tone} ev-${m.state}`, href: `#/edit/skill/${encodeURIComponent(name)}?tab=evaluation`,
    title: long, dataset: { evState: m.state } }, icon(m.icon, 12), h('span', short), h('span', { class: 'sr-only' }, `. ${long}`));
}
