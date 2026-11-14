// Editor checks. Blocking ERRORS come only from the hub's own content models: hub-core compiled to WebAssembly
// (hubwasm.js `validateMeta`, the same rules the server applies when the item is saved). Without the module there are
// none here, and Save shows the server's answer. WARNINGS are UX hints with no server rule: they never block Save.
// Messages are i18n keys with variables.
export const CAPABILITIES = [
  'read',
  'write',
  'edit',
  'shell',
  'web_fetch',
  'web_search',
  'mcp',
  'subagent',
  'browser',
  'notebook',
  'todo',
  'image',
];
export const MODEL_TIERS = ['fast', 'standard', 'deep'];
/** A description shorter than this gets a (non-blocking) suggestion: the server accepts 1 to 1024 characters. */
export const DESC_HINT_MIN = 20;
const issue = (severity, key, vars = {}, field = '') => ({ severity, key, vars, field });

/** The model's errors ([{loc, msg, type}], FastAPI's shape) as editor issues, with the editor's own words where it
 *  has them (`val.model` with the field and pydantic's message otherwise). */
export function modelIssues(errors) {
  return (Array.isArray(errors) ? errors : []).map((e) => {
    const field = String(e?.loc?.[0] ?? '');
    const where = (e?.loc || []).join('.');
    const type = String(e?.type || '');
    if (field === 'name' && type !== 'missing') return issue('error', 'val.name_bad', {}, 'name');
    if (field === 'description' && (type === 'missing' || type === 'string_too_short'))
      return issue('error', 'val.desc_required', {}, 'description');
    if (field === 'description' && type === 'string_too_long')
      return issue('error', 'val.desc_long', { max: 1024 }, 'description');
    if (field === 'model_tier' && type === 'literal_error') return issue('error', 'val.tier_bad', {}, 'model_tier');
    return issue('error', 'val.model', { field: where || '-', msg: String(e?.msg || type) }, field);
  });
}

/** `validateMeta` is hubwasm's (or null when the module is unavailable). */
export function validateSkill({ name, description, body }, validateMeta = null) {
  const out = validateMeta ? modelIssues(validateMeta('skill', { name, description: description ?? '' })) : [];
  const d = String(description ?? '').trim();
  if (d && d.length < DESC_HINT_MIN)
    out.push(issue('warn', 'val.desc_short', { min: DESC_HINT_MIN, n: d.length }, 'description'));
  if (/\n/.test(String(description ?? '').trim())) out.push(issue('warn', 'val.desc_multiline', {}, 'description'));
  if (!String(body ?? '').trim()) out.push(issue('warn', 'val.body_empty', {}, 'body'));
  // A body that starts with '---' would look like a second frontmatter block to some clients.
  if (/^\s*---\s*$/m.test(String(body ?? '').split('\n', 1)[0] || ''))
    out.push(issue('warn', 'val.body_frontmatter', {}, 'body'));
  return out;
}
export function validateAgent(
  { name, description, capabilities, model_tier, mode, read_only, tags },
  validateMeta = null
) {
  const out = validateMeta
    ? modelIssues(
        validateMeta('agent', {
          name,
          description: description ?? '',
          capabilities: capabilities ?? [],
          model_tier: model_tier || null,
          mode,
          read_only,
          tags,
        })
      )
    : [];
  if (!capabilities?.length) out.push(issue('warn', 'val.agent_no_caps', {}, 'capabilities'));
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
  const oneLine = String(description ?? '')
    .replace(/\s*\n\s*/g, ' ')
    .trim();
  return `---\nname: ${name}\ndescription: ${oneLine}\n---`;
}
// Per-client native tool mapping for an agent's capabilities. tool_map: {capability: native | null}.
export function mapCapabilities(capabilities, toolMap) {
  return capabilities.map((c) => ({
    capability: c,
    native: toolMap && c in toolMap ? toolMap[c] : null,
    supported: !!(toolMap && toolMap[c]),
  }));
}
export function agentClientPreview(agent, client) {
  const rows = mapCapabilities(agent.capabilities || [], client.caps?.tool_map || {});
  const missing = rows.filter((r) => !r.supported).map((r) => r.capability);
  const model = agent.model_tier ? (client.caps?.model_tiers?.[agent.model_tier] ?? null) : null;
  return {
    client: client.id,
    supportsAgents: !!client.caps?.agents,
    rows,
    missing,
    model,
    modelMissing: !!agent.model_tier && !model,
    severity: !client.caps?.agents ? 'na' : missing.length ? (client.strict ? 'error' : 'warn') : 'ok',
  };
}
