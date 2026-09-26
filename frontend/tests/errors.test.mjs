import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { setMessages } from '../js/i18n.js';
import { errorKey, describeError } from '../js/errors.js';
import { ApiError } from '../js/api.js';

const en = JSON.parse(fs.readFileSync(new URL('../i18n/en.json', import.meta.url), 'utf8'));
setMessages(en);
const E = (status, code, detail) => new ApiError(status, { code, detail });

test('every hardening status maps to its own actionable message key', () => {
  assert.equal(errorKey(E(401)), 'err.unauthorized');
  assert.equal(errorKey(E(403, 'forbidden')), 'err.forbidden');
  assert.equal(errorKey(E(403, 'csrf_header_required')), 'err.csrf');
  assert.equal(errorKey(E(409, 'plan_stale')), 'err.plan_stale');
  assert.equal(errorKey(E(409, 'conflict')), 'err.conflict');
  assert.equal(errorKey(E(409, 'other')), 'err.conflict_generic');
  assert.equal(errorKey(E(413)), 'err.too_large');
  assert.equal(errorKey(E(415)), 'err.unsupported_media');
  assert.equal(errorKey(E(421)), 'err.bad_host');
  assert.equal(errorKey(E(429)), 'err.rate_limited');
  assert.equal(errorKey(E(503)), 'err.unavailable');
  assert.equal(errorKey(E(0, 'network')), 'err.network');
  assert.equal(errorKey(E(500)), null);
});

test('describeError uses the message text, appends server detail, and never invents one for 401', () => {
  assert.match(describeError(E(409, 'plan_stale')), /Create a fresh plan/);
  assert.match(describeError(E(409, 'conflict', 'skill x changed')), /skill x changed/);
  assert.equal(describeError(E(401, 'unauthorized', 'bad key')), en['err.unauthorized']);
  assert.match(describeError(E(421)), /127\.0\.0\.1/);
  assert.equal(describeError(E(500, null, 'boom')), 'boom');
  assert.equal(describeError(null), en['common.error']);
});

test('422 lists field errors', () => {
  const e = new ApiError(422, { errors: [{ loc: ['body', 'description'], msg: 'must not be empty' }] });
  assert.match(describeError(e), /description: must not be empty/);
});
