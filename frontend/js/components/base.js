// Base class for view web components (light DOM so global CSS/tokens apply). Owns cleanup of listeners/streams/timers.
import { stream, api } from '../app-context.js';
import { clear } from '../dom.js';
import { errorBox, skeleton } from './ui.js';

export class AhView extends HTMLElement {
  constructor() { super(); this._cleanups = []; this._alive = false; }
  connectedCallback() { this._alive = true; this.classList.add('view'); this.setup?.(); }
  disconnectedCallback() { this._alive = false; for (const f of this._cleanups.splice(0)) { try { f(); } catch { /* ignore */ } } }
  own(fn) { this._cleanups.push(fn); return fn; }
  listen(target, ev, fn, o) { target.addEventListener(ev, fn, o); this.own(() => target.removeEventListener(ev, fn, o)); }
  stream(path, opts) { const s = stream(path, opts); this.own(() => s.close()); return s; }
  get api() { return api; }
  // Load async data into `el` with skeleton -> content | error states. `fn` returns a Node or array of Nodes.
  async loadInto(el, fn, { skeletonRows = 3 } = {}) {
    clear(el).append(skeleton(skeletonRows));
    try { const out = await fn(); if (!this._alive) return; clear(el).append(...[out].flat().filter(Boolean)); }
    catch (e) { if (!this._alive) return; clear(el).append(errorBox(e, () => this.loadInto(el, fn, { skeletonRows }))); }
  }
}
