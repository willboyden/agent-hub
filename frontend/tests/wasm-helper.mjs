// The hub-core WebAssembly module (backend/crates/hub-wasm) for the node tests and the mock server: the same files
// `make web` builds into frontend/wasm/ and the browser loads. When they are not built, the tests that need them are
// skipped with SKIP_WASM as the reason (every other test still runs).
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadHubWasm } from '../js/hubwasm.js';

const FRONTEND = dirname(dirname(fileURLToPath(import.meta.url)));
export const WASM_FILE = join(FRONTEND, 'wasm', 'hub_wasm_bg.wasm');
export const HAVE_WASM = existsSync(WASM_FILE) && existsSync(join(FRONTEND, 'wasm', 'hub_wasm.js'));
export const SKIP_WASM = HAVE_WASM
  ? false
  : 'frontend/wasm/ is not built: run `make web` (hub-core compiled to WebAssembly) to run this test';
/** The module's API (hubwasm.js), loaded from the built bytes. */
export const loadWasm = () => loadHubWasm(readFileSync(WASM_FILE));
