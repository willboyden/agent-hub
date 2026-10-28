// Normalisation of values that are genuinely volatile between two runs of the SAME implementation (random ids,
// secrets, clocks). Both sides run in the same temp paths on the same port, so paths and ports are NOT normalised.
// Every rule is listed here and in backend/PORTING.md "Parity harness"; adding one is a reviewed change.

export const RULES = [
  'N1 secrets: "hc_<43 urlsafe>" -> <SECRET>, "akn_..." -> <SECRET>; key prefixes (8 chars of a secret) -> <PREFIX>',
  'N2 ids: key_<12 hex> -> <KEYID>, plan_<12 hex> -> <PLANID>, apply ids <yyyymmddThhmmssZ-...> -> <APPLYID>, evaluation run ids -> <RUNID>, backup dirs <yyyymmddThhmmss-[rollback-]xxxx> -> <BACKUP>, inbox ids <slug>-<6 hex> -> <slug>-<RND6>',
  'N3b content files: a frontmatter line `created: <float>` (inbox items) -> `created: <TS>` (and the file is compared as text, not by hash)',
  'N3 clocks: a number under a timestamp key (ts, *_at, time, created, updated, started_at, finished_at, ...) -> <TS>',
  'N4 git: 40-hex commit ids -> <SHA>; 12-hex short ids under commit/reverted/head keys -> <SHA12>',
  'N5 headers: etag -> <ETAG>, last-modified -> <DATE>; date and server are not compared',
  'N6 metrics text: values of *_bucket, *_sum and *_created samples -> <V> (latencies and clocks)',
  'N8 openapi: FastAPI picks the knowledge proxy operationId from an unordered method set: proxy_api_v1_knowledge__path__<M>',
  'N9 knowledge: 12-hex token/job ids under id|token_id|job_id|jid -> <HEXID>; Lance `disk_bytes` and `data_dir_free_bytes` -> <BYTES> (manifests carry uuids; free space moves with other disk users)',
  'N10 multipart: the random `multipart/byteranges` boundary (26 hex) in the content type and the body -> <BOUNDARY>',
  'N11 versions: a semver string under a `version` key (health, the OpenAPI info, the knowledge service) -> <VERSION> (the Rust backend is 0.3.0, the Python reference 0.2.0)',
  'N7 file trees: names containing an apply id / backup stamp / plan id normalised as N2; JSON state files normalised as values',
];

const TS_KEYS = new Set(['ts', 'time', 'created', 'updated', 'created_at', 'last_used_at', 'revoked_at', 'applied_at',
  'adopted_at', 'verified_at', 'observed_at', 'started_at', 'finished_at', 'rolled_back_at', 'duration_ms', 'mtime',
  'started', 'finished']);
const SHA_KEYS = new Set(['commit', 'head', 'reverted']);
const HEXID_KEYS = new Set(['id', 'token_id', 'job_id', 'jid']);

export function normString(s) {
  return s
    .replace(/\bhc_[A-Za-z0-9_-]{43}\b/g, '<SECRET>')
    .replace(/\bakn_[A-Za-z0-9_-]{20,}/g, '<SECRET>')
    .replace(/\bkey_[0-9a-f]{12}\b/g, '<KEYID>')
    .replace(/\b\d{8}T\d{6}Z-[A-Za-z0-9_]{1,40}/g, '<APPLYID>')
    .replace(/\bplan_[0-9a-f]{12}\b/g, '<PLANID>')
    .replace(/\b([a-z0-9][a-z0-9._]*(?:-[a-z0-9._]+)*)-[0-9a-f]{6}\b(?=$|[/.'"\s]|\.md)/g, '$1-<RND6>')
    .replace(/\b\d{8}T\d{9}Z-[0-9a-f]{8}\b/g, '<RUNID>')
    .replace(/\b\d{8}T\d{6}-(?:rollback-)?[0-9a-f]{4}\b/g, '<BACKUP>')
    .replace(/\bproxy_api_v1_knowledge__path__(?:get|post|delete)\b/g, 'proxy_api_v1_knowledge__path__<M>')
    .replace(/(--|boundary=)[0-9a-f]{26}\b/g, '$1<BOUNDARY>')
    .replace(/\b[0-9a-f]{40}\b/g, '<SHA>');
}

export function normValue(v, key = '') {
  if (Array.isArray(v)) return v.map((x) => normValue(x, key));
  if (v && typeof v === 'object') {
    const out = {};
    for (const [k, x] of Object.entries(v)) {
      const nk = normString(k);
      out[nk] = normValue(x, k);
    }
    return out;
  }
  if (typeof v === 'number' && TS_KEYS.has(key)) return '<TS>';
  if (typeof v === 'number' && (key === 'disk_bytes' || key === 'data_dir_free_bytes')) return '<BYTES>';
  if (typeof v === 'string' && HEXID_KEYS.has(key) && /^[0-9a-f]{12}$/.test(v)) return '<HEXID>';
  if (typeof v === 'string' && key === 'version' && /^\d+\.\d+\.\d+$/.test(v)) return '<VERSION>';
  if (typeof v === 'string') {
    if (key === 'prefix' && /^hc_/.test(v)) return '<PREFIX>';
    if (SHA_KEYS.has(key) && /^[0-9a-f]{12}$/.test(v)) return '<SHA12>';
    return normString(v);
  }
  return v;
}

export function normMetrics(text) {
  return text.split('\n').map((line) => {
    if (line.startsWith('#') || !line.trim()) return line;
    const m = line.match(/^(\S+?)(\{.*\})? (\S+)$/);
    if (!m) return line;
    const [, name, labels = ''] = m;
    if (/_(bucket|sum|created)$/.test(name)) return `${name}${labels} <V>`;
    return line;
  }).join('\n');
}

/** SSE text: every `data:` line that holds JSON is normalised as a value (N3/N9 apply inside events). */
export function normSse(text) {
  return text.split('\n').map((line) => {
    if (!line.startsWith('data: ')) return normString(line);
    try { return `data: ${JSON.stringify(normValue(JSON.parse(line.slice(6))))}`; } catch { return normString(line); }
  }).join('\n');
}

export function normHeaders(h) {
  const keep = ['content-type', 'cache-control', 'content-security-policy', 'x-content-type-options', 'referrer-policy',
    'x-frame-options', 'permissions-policy', 'cross-origin-resource-policy', 'etag', 'last-modified', 'x-accel-buffering',
    'accept-ranges', 'location', 'retry-after', 'content-range'];
  const out = {};
  for (const k of keep) {
    const v = h.get(k);
    if (v == null) continue;
    out[k] = k === 'etag' ? '<ETAG>' : k === 'last-modified' ? '<DATE>' : k === 'content-type' ? normString(v) : v;
  }
  return out;
}
