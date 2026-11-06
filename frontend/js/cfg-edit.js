// In-place value edits for the Form view (ported from Nemo Console; env files added): turn what the person typed into
// source text of the right type and quoting style, then splice it over the old value's span. Everything else in the file (comments, blank lines, key order,
// other values) stays byte for byte. Pure logic, no DOM, so tests import it.
//
//   encodeValue(format, node, input, spec) -> { raw } | { error }
//   spliceValue(text, node, raw) -> new text
//   flattenFields(root) -> [{ path, key, node, depth }]  (every scalar, in document order)
import { yamlPlainType } from './cfg-parse.js';

const TOML_INT = /^[+-]?(?:0|[1-9](?:_?\d)*)$/;
const TOML_FLOAT = /^[+-]?(?:0|[1-9](?:_?\d)*)(?:\.\d(?:_?\d)*)?(?:[eE][+-]?\d(?:_?\d)*)?$|^[+-]?(?:inf|nan)$/;
const TOML_DATE =
  /^\d{4}-\d{2}-\d{2}(?:[Tt ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:[Zz]|[+-]\d{2}:\d{2})?)?$|^\d{2}:\d{2}:\d{2}(?:\.\d+)?$/;
const DEC_INT = /^[-+]?(?:0|[1-9][0-9_]*)$/;
const DEC_FLOAT =
  /^[-+]?(?:\d[\d_]*)?\.\d[\d_]*(?:[eE][-+]?\d+)?$|^[-+]?\d[\d_]*\.(?:[eE][-+]?\d+)?$|^[-+]?\d[\d_]*[eE][-+]?\d+$/;

/**
 * JSON string escaping is valid in TOML basic strings and YAML double-quoted strings alike. JSON leaves DEL, NEL and the
 * Unicode line/paragraph separators raw; YAML folds NEL/LS/PS into spaces and refuses DEL, so they are escaped too.
 */
const dq = (s) =>
  JSON.stringify(s).replace(
    /[\u007f\u0085\u2028\u2029]/g,
    (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, '0')}`
  );
const YAML_SQ_BAD = /[\n\r\t\u0000-\u001f\u007f\u0085\u2028\u2029]/; // what a YAML single-quoted string cannot carry as typed

function tomlString(node, v) {
  if (/[\u0000-\u0008\u000b-\u001f\u007f]/.test(v) && !/\n/.test(v)) return dq(v);
  if (node.style === 'literal' && !/['\n\r]/.test(v)) return `'${v}'`;
  if (node.style === 'ml-literal' && !v.includes("'''") && !/[\u0000-\u0008\u000b-\u001f\u007f]/.test(v))
    return `'''\n${v}'''`;
  if (node.style === 'ml-basic' || v.includes('\n'))
    return `"""\n${v.replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/\t/g, '\\t').replace(/\r/g, '\\r')}"""`;
  return dq(v);
}

/** Can v be written as a YAML plain scalar that PyYAML reads back as exactly this string? */
export function yamlPlainSafe(v) {
  return (
    v !== '' &&
    v !== '=' &&
    v !== '<<' &&
    v === v.trim() &&
    yamlPlainType(v).type === 'string' &&
    !/^(?:[,[\]{}#&*!|>'"%@`]|[-?:](?:\s|$))/.test(v) &&
    !/:(?:\s|$)|\s#|[\t\n\r]|[\u0000-\u001f\u007f\u0085\u2028\u2029\ufeff]/.test(v) &&
    !/^(?:---|\.\.\.)(?:\s|$)/.test(v)
  );
}
function yamlString(node, v) {
  // inside [ ] or { } a plain value also ends at , [ ] { } and :, so only simple words stay unquoted there
  if (node.flow)
    return /^[A-Za-z0-9_./+-]+(?: [A-Za-z0-9_./+-]+)*$/.test(v) && yamlPlainSafe(v)
      ? v
      : node.style === 'single' && !YAML_SQ_BAD.test(v)
        ? `'${v.replace(/'/g, "''")}'`
        : dq(v);
  if ((node.style === 'plain' || node.style === 'empty') && yamlPlainSafe(v)) return v;
  if (node.style === 'single' && !YAML_SQ_BAD.test(v)) return `'${v.replace(/'/g, "''")}'`;
  if (node.style === 'plain' || node.style === 'empty') return yamlPlainSafe(v) ? v : dq(v);
  return dq(v);
}
/** Keep the spelling family of a YAML boolean (true/yes/on) and its case. */
function yamlBool(raw, value) {
  const fam = /^(?:yes|no)$/i.test(raw) ? ['yes', 'no'] : /^(?:on|off)$/i.test(raw) ? ['on', 'off'] : ['true', 'false'];
  const w = value ? fam[0] : fam[1];
  if (/^[A-Z]+$/.test(raw)) return w.toUpperCase();
  if (/^[A-Z]/.test(raw)) return w[0].toUpperCase() + w.slice(1);
  return w;
}

const isNumSpec = (spec) => spec?.t === 'num';

/**
 * Encode `input` (a string from a text field, or a boolean from a switch) as the source text for `node`. The node's
 * own type is kept: a whole-number field takes only whole numbers, a text field always writes text. A field whose
 * schema says "number" takes both. `spec` (optional) is the schema entry from cfg-checks.js specAt().
 */
export function encodeValue(format, node, input, spec = null) {
  if (format === 'env') return encodeEnv(node, input);
  let type = node.type;
  if (type === 'null')
    type = spec?.t === 'int' ? 'int' : spec?.t === 'num' ? 'float' : spec?.t === 'bool' ? 'bool' : 'string';
  if (typeof input === 'boolean') {
    if (type !== 'bool') return { error: 'not a true/false field' };
    return { raw: format === 'toml' ? String(input) : yamlBool(node.type === 'bool' ? node.raw : 'true', input) };
  }
  const s = String(input);
  if (node.type === 'null' && s.trim() === '' && format === 'yaml') return { raw: node.raw }; // still empty
  if (type === 'string') {
    if (format === 'toml') return { raw: tomlString(node, s) };
    if (node.style === 'block') {
      // PyYAML takes a block's indentation from its first non-blank line: an indented first line would lose its
      // indentation (or break the file), so it is refused rather than written.
      if (/^[ \t]/.test(s.split('\n').find((l) => l.trim() !== '') ?? ''))
        return { error: 'the first line of this text cannot start with a space or tab here; use the Text view' };
      return { block: s };
    }
    if (s.includes('\n')) return { raw: dq(s) };
    return { raw: yamlString(node, s) };
  }
  let v = s.trim();
  if (format === 'yaml') v = v.replace(/^([-+])\.(?=\d)/, '$10.'); // PyYAML reads -.5 as text, -0.5 as a number
  if (type === 'int' || type === 'float') {
    const intOk = format === 'toml' ? TOML_INT.test(v) : DEC_INT.test(v);
    const floatOk = format === 'toml' ? TOML_FLOAT.test(v) && /[.eEn]/.test(v) : DEC_FLOAT.test(v);
    if (type === 'int' && !isNumSpec(spec)) {
      if (intOk && Number.isSafeInteger(Number(v.replace(/_/g, '')))) return { raw: v };
      if (/^[-+]?\d+\.0*$/.test(v)) return { error: 'a whole number, without a decimal point' };
      return { error: /^\s*$/.test(v) ? 'a whole number is required' : `'${v}' is not a whole number` };
    }
    if (intOk) return { raw: type === 'float' && !isNumSpec(spec) ? `${v}.0` : v };
    if (floatOk) {
      // PyYAML reads 1e3 (no dot, unsigned exponent) as text: write a form it reads as a number
      if (format === 'yaml' && !/^[-+]?(?:\d[\d_]*)?\.[\d_]*(?:[eE][-+]\d+)?$/.test(v)) {
        const n = Number(v.replace(/_/g, ''));
        if (!Number.isFinite(n)) return { error: `'${v}' is not a number` };
        let out = String(n);
        if (out.includes('e')) {
          out = n.toExponential();
          if (!out.split('e')[0].includes('.')) out = out.replace('e', '.0e');
        } else if (!out.includes('.')) out += '.0';
        return { raw: out };
      }
      return { raw: v };
    }
    return { error: v ? `'${v}' is not a number` : 'a number is required' };
  }
  if (type === 'bool') {
    if (/^(?:true|false)$/i.test(v)) return encodeValue(format, node, /^true$/i.test(v), spec);
    return { error: 'true or false' };
  }
  if (type === 'date') {
    if (format === 'toml' ? TOML_DATE.test(v) : yamlPlainType(v).type === 'date') return { raw: v };
    return { error: 'a date like 2026-11-04' };
  }
  return { error: 'this value cannot be edited here; use the Text view' };
}

/**
 * An env file value: bare when python-dotenv reads it back unchanged (no quote at the start, no ` #`, no edge
 * spaces), else in the node's own quote style when that can carry it, else double quotes with escapes. Only
 * `${NAME}` keeps its meaning: it is interpolated at start in every style.
 */
export function encodeEnv(node, input) {
  const v = typeof input === 'boolean' ? String(input) : String(input);
  if (/[\u0000]/.test(v)) return { error: 'a NUL character cannot be stored in an env file' };
  const bareOk = v !== '' && v === v.trim() && !/^['"]/.test(v) && !/[ \t]#/.test(v) && !/[\r\n]/.test(v);
  const singleOk = !/['\r\n]/.test(v);
  const single = () => `'${v.replace(/\\/g, '\\\\')}'`;
  const double = () =>
    `"${v.replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/\n/g, '\\n').replace(/\r/g, '\\r').replace(/\t/g, '\\t')}"`;
  if (v === '') return { raw: node.style === 'single' ? "''" : node.style === 'double' ? '""' : '' };
  if (node.style === 'double') return { raw: double() };
  if (node.style === 'single' && singleOk) return { raw: single() };
  if (bareOk) return { raw: v };
  return { raw: singleOk ? single() : double() };
}

/** Splice a new raw value over node's span (an empty YAML value gets the separating space it needs). */
export function spliceValue(text, node, raw) {
  const pre =
    node.style === 'empty' && !node.env && raw !== node.raw && !/[ \t]$/.test(text.slice(0, node.start)) ? ' ' : '';
  return text.slice(0, node.start) + pre + raw + text.slice(node.end);
}

/** Replace the lines of a YAML | or > block, re-indented to the block's own indentation. */
export function spliceBlock(text, node, value) {
  const ind = ' '.repeat(node.block.indent);
  const lines = value
    .replace(/\n$/, '')
    .split('\n')
    .map((l) => (l ? ind + l : ''));
  return text.slice(0, node.start) + lines.join('\n') + text.slice(node.end);
}

/** The text the Form shows for a scalar in an input box. */
export function displayValue(node) {
  if (node.style === 'block') return node.block.lines.join('\n');
  if (node.type === 'null') return '';
  if (node.type === 'string') return node.value;
  if (node.type === 'date') return String(node.value);
  return node.raw.trim();
}

/** Every scalar of a parsed document in order, with its path and the depth of its group. */
export function flattenFields(root) {
  const out = [];
  const walk = (node, path) => {
    if (node.kind === 'scalar') {
      out.push({ path, key: path.at(-1), node });
      return;
    }
    if (node.kind === 'map') for (const e of node.entries) walk(e.node, [...path, e.key]);
    else node.items.forEach((it, i) => walk(it, [...path, i]));
  };
  if (root) walk(root, []);
  return out;
}
export const pathKey = (path) => JSON.stringify(path);

/** A path for a shell command shown as text: single-quoted, so spaces and `$` stay literal. */
export const shq = (s) => `'${String(s).replace(/'/g, "'\\''")}'`;
/** A path for a shell command shown as text: `~/x` as `"$HOME"/'x'` (a quoted `~` would not expand), else {@link shq}. */
export const shPath = (p) => (String(p).startsWith('~/') ? `"$HOME"/${shq(String(p).slice(2))}` : shq(p));
/** A check that looked at nothing (`summary.checked: false`) is not a pass, whatever `ok` says. */
export const uncheckedResult = (r) => r?.summary?.checked === false;
