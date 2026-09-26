// Client-side validation that mirrors the server's rules, so the editor can explain problems before the round-trip.
// The server stays authoritative; these only produce early, friendly hints. Messages are i18n keys with variables.
import { NAME_RE } from './util.js';

export const CAPABILITIES = ['read', 'write', 'edit', 'shell', 'web_fetch', 'web_search', 'mcp', 'subagent', 'browser', 'notebook', 'todo', 'image'];
export const MODEL_TIERS = ['fast', 'standard', 'deep'];
const issue = (severity, key, vars = {}, field = '') => ({ severity, key, vars, field });

export function validateSkill({ name, description, body }) {
  const out = [];
  if (!NAME_RE.test(name || '')) out.push(issue('error', 'val.name_bad', {}, 'name'));
  const d = String(description ?? '').trim();
  if (!d) out.push(issue('error', 'val.desc_required', {}, 'description'));
  else if (d.length < 20) out.push(issue('error', 'val.desc_short', { min: 20, n: d.length }, 'description'));
  if (String(description ?? '').length > 1024) out.push(issue('error', 'val.desc_long', { max: 1024 }, 'description'));
  if (/\n/.test(String(description ?? '').trim())) out.push(issue('warn', 'val.desc_multiline', {}, 'description'));
  if (!String(body ?? '').trim()) out.push(issue('warn', 'val.body_empty', {}, 'body'));
  // A body that starts with '---' would look like a second frontmatter block to some clients.
  if (/^\s*---\s*$/m.test(String(body ?? '').split('\n', 1)[0] || '')) out.push(issue('warn', 'val.body_frontmatter', {}, 'body'));
  return out;
}
export function validateAgent({ name, description, capabilities, model_tier }) {
  const out = [];
  if (!NAME_RE.test(name || '')) out.push(issue('error', 'val.name_bad', {}, 'name'));
  if (!String(description ?? '').trim()) out.push(issue('error', 'val.desc_required', {}, 'description'));
  if (!capabilities?.length) out.push(issue('warn', 'val.agent_no_caps', {}, 'capabilities'));
  if (model_tier && !MODEL_TIERS.includes(model_tier)) out.push(issue('error', 'val.tier_bad', {}, 'model_tier'));
  return out;
}
export function validateFilePath(p, existing = []) {
  const s = String(p ?? '');
  if (!s || s.length > 200) return issue('error', 'val.file_empty');
  if (/^\/|\\|\0|(^|\/)\.\.(\/|$)/.test(s)) return issue('error', 'val.file_unsafe');
  if (s === 'SKILL.md') return issue('error', 'val.file_reserved');
  if (existing.includes(s)) return issue('error', 'val.file_dup');
  return null;
}
// What the file on disk will start with (the shared SKILL.md standard). Shown read-only next to the form.
export function skillFrontmatter({ name, description }) {
  const oneLine = String(description ?? '').replace(/\s*\n\s*/g, ' ').trim();
  return `---\nname: ${name}\ndescription: ${oneLine}\n---`;
}
// Per-client native tool mapping for an agent's capabilities. tool_map: {capability: native | null}.
export function mapCapabilities(capabilities, toolMap) {
  return capabilities.map((c) => ({ capability: c, native: toolMap && c in toolMap ? toolMap[c] : null, supported: !!(toolMap && toolMap[c]) }));
}
export function agentClientPreview(agent, client) {
  const rows = mapCapabilities(agent.capabilities || [], client.caps?.tool_map || {});
  const missing = rows.filter((r) => !r.supported).map((r) => r.capability);
  const model = agent.model_tier ? client.caps?.model_tiers?.[agent.model_tier] ?? null : null;
  return { client: client.id, supportsAgents: !!client.caps?.agents, rows, missing, model, modelMissing: !!agent.model_tier && !model,
    severity: !client.caps?.agents ? 'na' : missing.length ? (client.strict ? 'error' : 'warn') : 'ok' };
}
