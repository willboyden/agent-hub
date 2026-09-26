// Minimal observable store + safe web-storage wrappers (storage can throw in private mode).
export function createStore(initial = {}) {
  let state = { ...initial };
  const subs = new Set();
  return {
    get: () => state,
    set(patch) {
      const prev = state;
      state = { ...state, ...(typeof patch === 'function' ? patch(state) : patch) };
      for (const s of [...subs]) {
        const next = s.sel ? s.sel(state) : state;
        if (!s.sel || !Object.is(next, s.last)) { s.last = next; s.fn(next, prev); }
      }
    },
    // selector optional: callback only fires when the selected value changes (Object.is).
    subscribe(fn, sel) {
      const s = { fn, sel, last: sel ? sel(state) : undefined };
      subs.add(s);
      return () => subs.delete(s);
    },
  };
}

export function safeStorage(getter) {
  const store = () => { try { return getter(); } catch { return null; } };
  return {
    get(k, fallback = null) { try { const v = store()?.getItem(k); return v == null ? fallback : v; } catch { return fallback; } },
    set(k, v) { try { const s = store(); if (!s) return false; s.setItem(k, v); return true; } catch { return false; } },
    remove(k) { try { store()?.removeItem(k); } catch { /* ignore */ } },
  };
}
export const session = safeStorage(() => globalThis.sessionStorage);
export const local = safeStorage(() => globalThis.localStorage);
