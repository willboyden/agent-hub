// Client-spec helpers: parse pasted YAML/JSON into an object (the backend wants `spec` as a mapping), and map validate-spec results to
// display-ready issues. Real backend: 200 {ok, artifacts:[{root,path,kind,merge,size,preview}], diagnostics:[{severity,code,message,item}]},
// 422 problem {errors:[{path,message,fix_hint}]} when the spec does not fit the ClientDoc schema.

const KNOWN = new Set([
  'missing_field',
  'unknown_key',
  'bad_type',
  'bad_value',
  'bad_path',
  'yaml_syntax',
  'yaml_unavailable',
  'unsafe_path',
  'no_skills_root',
  'unsupported',
]);

export function issueMessage(issue) {
  const code = issue?.code || '';
  const vars = { path: issue?.path || '', message: issue?.message || '' };
  if (KNOWN.has(code)) return { key: `spec.code.${code}`, vars, fallback: vars.message };
  return { key: null, vars, fallback: vars.message || code || 'invalid', hint: issue?.fix_hint || '' };
}

// text -> {spec} | {error: {code, message, line}} | {empty: true}. Never throws. `yamlLoad` is hub-core's YAML loader
// (hubwasm.js: the server's own PyYAML port, so `yes`, `null`, `1.5`, anchors and block scalars read exactly as the hub
// reads them); without it (the module did not load) only JSON can be read, and YAML gets code `yaml_unavailable`.
export function parseSpec(text, yamlLoad = null) {
  const s = String(text ?? '').trim();
  if (!s) return { empty: true };
  if (s.startsWith('{')) {
    try {
      return { spec: JSON.parse(s) };
    } catch (e) {
      if (!yamlLoad) return { error: { code: 'yaml_syntax', message: e.message, line: null } };
      // not JSON: a YAML flow mapping, read below
    }
  }
  const r = yamlLoad ? yamlLoad(String(text)) : null;
  if (!r)
    return {
      error: {
        code: 'yaml_unavailable',
        message: 'the YAML reader did not load: reload the page (Ctrl+Shift+R), or paste the spec as JSON',
        line: null,
      },
    };
  if (!r.ok) return { error: { code: 'yaml_syntax', message: r.message, line: r.line ?? null } };
  return { spec: r.value };
}

/** The wizard's starting spec from the generic adapter's defaults (null when it has none). */
export function defaultSpec(spec) {
  if (!spec) return null;
  return {
    skills: {
      root: spec.skills?.root || 'home',
      dir: spec.skills?.dir || 'skills',
      layout: spec.skills?.layout || 'dir/SKILL.md',
    },
    instructions: {
      root: spec.instructions?.root || 'home',
      target: spec.instructions?.target || 'INSTRUCTIONS.md',
      merge: spec.instructions?.merge || 'block',
    },
  };
}
/** That spec as text: YAML when the hub's YAML reader is in the tab, else JSON (which parseSpec reads without it), so
 *  the wizard's own default never blocks Next. */
export function defaultSpecText(spec, yaml = true) {
  const d = defaultSpec(spec);
  if (!d) return '';
  if (!yaml) return `${JSON.stringify(d, null, 2)}\n`;
  const { skills: k, instructions: i } = d;
  return `skills:\n  root: ${k.root}\n  dir: ${k.dir}\n  layout: ${k.layout}\ninstructions:\n  root: ${i.root}\n  target: ${i.target}\n  merge: ${i.merge}\n`;
}

const sev = (d) => (d.severity === 'error' ? 'error' : 'warn');
export function mapSpecResult(resp) {
  const diags = resp?.diagnostics || resp?.preview?.diagnostics || [];
  const errs = [
    ...(resp?.errors || []).map((e) => ({ ...issueMessage(e), severity: 'error', path: e.path || '' })),
    ...diags
      .filter((d) => d.severity === 'error')
      .map((d) => ({ ...issueMessage(d), severity: 'error', path: d.item || '' })),
  ];
  const warns = [
    ...(resp?.warnings || []).map((e) => ({ ...issueMessage(e), severity: 'warn', path: e.path || '' })),
    ...diags
      .filter((d) => d.severity !== 'error')
      .map((d) => ({ ...issueMessage(d), severity: sev(d), path: d.item || '' })),
  ];
  const files = (resp?.artifacts || resp?.preview?.files || []).map((f) => ({
    root: f.root,
    path: f.path,
    kind: f.kind,
    merge: f.merge,
    content: f.preview ?? f.content ?? '',
  }));
  return {
    spec: resp?.spec,
    ok: !!resp?.ok && errs.length === 0,
    issues: [...errs, ...warns],
    errors: errs,
    warnings: warns,
    files,
    diagnostics: diags,
  };
}
// A 422 from validate-spec / create-client carries problem.errors; show them as spec issues.
export const specErrorResult = (e) =>
  mapSpecResult({
    ok: false,
    errors: (e?.problem?.errors || [])
      .map((x) => ({
        code: '',
        path: x.path || (x.loc || []).filter((p) => p !== 'body').join('.'),
        message: x.message || x.msg || e.message,
        fix_hint: x.fix_hint,
      }))
      .concat(e?.problem?.errors?.length ? [] : [{ code: '', path: '', message: e?.message || '' }]),
  });
