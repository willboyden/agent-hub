// Views and components are custom elements (HTMLElement subclasses). A method named like a DOM method replaces it for
// every caller: the router detaches a view with `current.remove()`, so a view's own `remove()` (its delete flow) ran on
// every navigation away. 2026-10-26: leaving the editor opened "Delete <item>?", and leaving Collections threw.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'js');
const DOM_METHODS = [
  'remove', 'append', 'prepend', 'before', 'after', 'replaceWith', 'replaceChildren', 'appendChild', 'removeChild',
  'insertBefore', 'replaceChild', 'cloneNode', 'contains', 'normalize', 'focus', 'blur', 'click', 'closest', 'matches',
  'getAttribute', 'setAttribute', 'removeAttribute', 'hasAttribute', 'toggleAttribute', 'querySelector',
  'querySelectorAll', 'getElementsByClassName', 'getElementsByTagName', 'getBoundingClientRect', 'getClientRects',
  'scroll', 'scrollTo', 'scrollBy', 'scrollIntoView', 'animate', 'attachShadow', 'attachInternals', 'dispatchEvent',
  'addEventListener', 'removeEventListener', 'insertAdjacentElement', 'insertAdjacentHTML', 'insertAdjacentText',
  'requestFullscreen', 'showPopover', 'hidePopover', 'togglePopover', 'checkVisibility',
];
const member = new RegExp(`^\\s+(?:async\\s+|static\\s+)?(${DOM_METHODS.join('|')})\\s*\\([^)]*\\)\\s*\\{`, 'gm');

test('no custom element defines a method that shadows a DOM method', () => {
  const bad = [];
  for (const dir of ['views', 'components']) {
    for (const f of fs.readdirSync(path.join(root, dir)).filter((n) => n.endsWith('.js'))) {
      const src = fs.readFileSync(path.join(root, dir, f), 'utf8');
      if (!/extends\s+(AhView|HTMLElement)\b/.test(src)) continue;
      for (const m of src.matchAll(member)) bad.push(`${dir}/${f}: ${m[1]}()`);
    }
  }
  assert.deepEqual(bad, []);
});
