// Client-spec helpers: parse pasted YAML/JSON into an object (the backend wants `spec` as a mapping), and map validate-spec results to
// display-ready issues. Real backend: 200 {ok, artifacts:[{root,path,kind,merge,size,preview}], diagnostics:[{severity,code,message,item}]},
// 422 problem {errors:[{path,message,fix_hint}]} when the spec does not fit the ClientDoc schema.
import { parseYamlLite, YamlLiteError } from './yaml-lite.js';

const KNOWN = new Set(['missing_field', 'unknown_key', 'bad_type', 'bad_value', 'bad_path', 'yaml_syntax', 'unsafe_path', 'no_skills_root', 'unsupported']);

export function issueMessage(issue) {
  const code = issue?.code || '';
  const vars = { path: issue?.path || '', message: issue?.message || '' };
  if (KNOWN.has(code)) return { key: `spec.code.${code}`, vars, fallback: vars.message };
  return { key: null, vars, fallback: vars.message || code || 'invalid', hint: issue?.fix_hint || '' };
}

// text -> {spec} | {error: {code:'yaml_syntax', message, line}}. Never throws.
export function parseSpec(text) {
  const s = String(text ?? '').trim();
  if (!s) return { empty: true };
  try {
    if (s.startsWith('{')) { const v = JSON.parse(s); return { spec: v }; }
    const spec = parseYamlLite(s);
    return { spec };
  } catch (e) {
    return { error: { code: 'yaml_syntax', message: e instanceof YamlLiteError || e instanceof SyntaxError ? e.message : String(e), line: e.line ?? null } };
  }
}

const sev = (d) => (d.severity === 'error' ? 'error' : 'warn');
export function mapSpecResult(resp) {
  const diags = resp?.diagnostics || resp?.preview?.diagnostics || [];
  const errs = [...(resp?.errors || []).map((e) => ({ ...issueMessage(e), severity: 'error', path: e.path || '' })), ...diags.filter((d) => d.severity === 'error').map((d) => ({ ...issueMessage(d), severity: 'error', path: d.item || '' }))];
  const warns = [...(resp?.warnings || []).map((e) => ({ ...issueMessage(e), severity: 'warn', path: e.path || '' })), ...diags.filter((d) => d.severity !== 'error').map((d) => ({ ...issueMessage(d), severity: sev(d), path: d.item || '' }))];
  const files = (resp?.artifacts || resp?.preview?.files || []).map((f) => ({ root: f.root, path: f.path, kind: f.kind, merge: f.merge, content: f.preview ?? f.content ?? '' }));
  return { spec: resp?.spec, ok: !!resp?.ok && errs.length === 0, issues: [...errs, ...warns], errors: errs, warnings: warns, files, diagnostics: diags };
}
// A 422 from validate-spec / create-client carries problem.errors; show them as spec issues.
export const specErrorResult = (e) => mapSpecResult({ ok: false, errors: (e?.problem?.errors || []).map((x) => ({ code: '', path: x.path || (x.loc || []).filter((p) => p !== 'body').join('.'), message: x.message || x.msg || e.message, fix_hint: x.fix_hint })) .concat(e?.problem?.errors?.length ? [] : [{ code: '', path: '', message: e?.message || '' }]) });
