// Human-readable, actionable text for API failures. Status/code -> i18n key is a pure mapping (unit-tested).
import { t } from './i18n.js';

// Backend `code`s that deserve their own explanation (read from the backend's domain errors), checked before the generic status mapping.
const CODE_KEYS = { knowledge_unavailable: 'err.knowledge_unavailable', knowledge_upstream_unauthorized: 'err.knowledge_unauthorized', cell_blocked: 'err.cell', cell_unsupported: 'err.cell', never_applied: 'err.never_applied', client_not_committed: 'err.client_not_committed',
  content_not_initialised: 'err.content_not_initialised', content_invalid: 'err.content_invalid', dirty_tree: 'err.dirty_tree', already_exists: 'err.exists',
  confirm_required: 'err.confirm', floor_locked: 'err.floor_locked', unknown_adapter: 'err.adapter', adapter_missing: 'err.adapter' };

export function errorKey(e) {
  if (!e) return 'common.error';
  const { status, code } = e;
  if (code && CODE_KEYS[code]) return CODE_KEYS[code];
  if (status === 405) return 'err.method';
  if (status === 0 || code === 'network') return 'err.network';
  if (status === 401) return 'err.unauthorized';
  if (status === 403) return code === 'csrf_header_required' ? 'err.csrf' : 'err.forbidden';
  if (status === 409) return code === 'plan_stale' ? 'err.plan_stale' : code === 'conflict' ? 'err.conflict' : 'err.conflict_generic';
  if (status === 413) return 'err.too_large';
  if (status === 415) return 'err.unsupported_media';
  if (status === 421) return 'err.bad_host';
  if (status === 429) return 'err.rate_limited';
  if (status === 503) return 'err.unavailable';
  return null;
}

export function describeError(e) {
  if (!e) return t('common.error');
  const k = errorKey(e);
  if (k) {
    const base = t(k);
    // Keep the server's own detail when it adds information (e.g. which file conflicted), never for auth problems.
    return e.message && e.status !== 401 && e.status !== 0 && !/^HTTP \d+$/.test(e.message) && e.message !== base ? `${base} (${e.message})` : base;
  }
  if (e.status === 422) {
    const errs = (e.problem?.errors || []).map((x) => `${(x.loc || []).filter((p) => p !== 'body').join('.')}: ${x.msg}`).filter(Boolean);
    return errs.length ? `${t('err.invalid')} ${errs.join('; ')}` : (e.message || t('err.invalid'));
  }
  return e.message || t('common.error');
}
