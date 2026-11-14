// hub-core's own checks, compiled to WebAssembly (backend/crates/hub-wasm, built by `make web` into frontend/wasm/;
// docs/adr/0013). The config editor, the client wizard and the item editor call the SAME Rust functions the server
// calls, in the tab: a hub env draft (which can hold secrets) is checked here and never sent anywhere, a pasted client
// spec is read by the server's own YAML loader (a PyYAML port), and the editor's blocking errors are the server's
// model rules. The server stays authoritative: it checks again on every write.
//
//   hubWasm() -> Promise<Api | null>   loads the module once (never rejects); null = unavailable (see wasmStatus()).
//                                     After a failed load a later call tries again, at most every RETRY_MS.
//   hubWasmWithin(ms) -> Promise<Api | null>   the same, but null after `ms` (a stalled fetch never holds a view)
//   loadHubWasm(bytes) -> Promise<Api | null>   the same from bytes (Node: the mock server and the tests)
//
// Api (every call is synchronous and returns null if the module traps; the next hubWasm() then loads a fresh copy):
//   version                      the workspace version the module was built from (the files are served no-cache
//                                with an ETag, and a glue file without the whole contract below is refused)
//   checkConfig(file, text)      the body POST /config-files/{file}/validate answers (a problem body: {status, code})
//   yamlLoad(text)               {ok: true, value} | {ok: false, message, line}
//   yamlScalarType(s)            'null' | 'bool' | 'int' | 'float' | 'timestamp' | 'str' | 'merge' | 'value'
//   validateMeta(kind, fields)   [{loc, msg, type}] from the item's content model ([] = valid)
const GLUE = '../wasm/hub_wasm.js';
/** The export contract (backend/crates/hub-wasm/src/lib.rs). */
const EXPORTS = ['version', 'check_config', 'yaml_load', 'yaml_scalar_type', 'validate_meta'];

let current = null; // Promise<Api | null> of the module in use
let generation = 0;
let failedAt = 0; // when the last load failed (0 = it did not)
/** A failed load (a missing or stale file, a network error) is tried again on a later call, not more often than this. */
export const RETRY_MS = 5000;
let source = null; // the module's bytes when loaded from bytes (Node), reused after a trap
const status = { state: 'idle', reason: '' };

/** 'idle' | 'loading' | 'ready' | 'failed' | 'trapped', and why (for the UI's message and the tests). */
export const wasmStatus = () => ({ ...status });

/** The Api over a loaded glue module (exported for the tests: a module whose call throws behaves like a trap). */
export function wrapModule(mod) {
  let dead = false;
  const call = (fn, ...args) => {
    if (dead) return null;
    try {
      return fn(...args);
    } catch (e) {
      // A Rust panic aborts (panic = "abort"): the instance is unusable. Drop it; the next hubWasm() loads a new one.
      dead = true;
      current = null;
      status.state = 'trapped';
      status.reason = String(e?.message || e);
      return null;
    }
  };
  const json = (s) => (s == null ? null : JSON.parse(s));
  return {
    version: mod.version(),
    get alive() {
      return !dead;
    },
    checkConfig: (file, text) => json(call(mod.check_config, String(file), String(text))),
    yamlLoad: (text) => json(call(mod.yaml_load, String(text))),
    yamlScalarType: (s) => call(mod.yaml_scalar_type, String(s)),
    validateMeta: (kind, fields) => json(call(mod.validate_meta, String(kind), JSON.stringify(fields ?? {}))),
  };
}

async function load(bytes) {
  const gen = ++generation;
  status.state = 'loading';
  status.reason = '';
  try {
    // a fresh module record after a trap (the glue keeps its instance in a module-level variable)
    const mod = await import(gen === 1 ? GLUE : `${GLUE}?instance=${gen}`);
    // a glue file from another build (a stale cache) that lacks part of the contract is refused rather than used
    const missing = EXPORTS.filter((n) => typeof mod[n] !== 'function');
    if (missing.length) throw new Error(`the module lacks ${missing.join(', ')} (rebuild it with make web)`);
    await mod.default(bytes ? { module_or_path: bytes } : undefined);
    const api = wrapModule(mod);
    status.state = 'ready';
    return api;
  } catch (e) {
    status.state = 'failed';
    status.reason = String(e?.message || e);
    failedAt = Date.now();
    return null;
  }
}

/** The module, loaded once per page (or again after a trap). Never rejects. */
export async function hubWasm() {
  // a failed load is not kept forever: once RETRY_MS has passed, the next call loads again (a new module URL)
  if (current && status.state === 'failed' && Date.now() - failedAt >= RETRY_MS) current = null;
  if (!current) current = load(source);
  const api = await current;
  if (!api) return null;
  return api.alive ? api : hubWasm();
}

/** hubWasm(), but null when it has not answered within `ms` (the load goes on and later calls get it). */
export function hubWasmWithin(ms) {
  let timer;
  const late = new Promise((resolve) => {
    timer = setTimeout(() => resolve(null), ms);
  });
  return Promise.race([hubWasm(), late]).finally(() => clearTimeout(timer));
}

/** Load from the module's bytes (Node has no fetch for file URLs). Replaces the module in use. */
export async function loadHubWasm(bytes) {
  source = bytes;
  current = load(bytes);
  return current;
}
