/**
 * Thin wrapper around the `zellij-ansi-clip` wasm module.
 *
 * The module is loaded once per page and reused for every r/o viewer
 * instance on the tab. `createClipper` returns handles that own a
 * per-viewer `ClipState` — one grid, one cursor, one SGR attribute
 * stream — allocated inside the shared wasm memory.
 */

let wasmExports = null;

async function loadWasm(baseUrl) {
    if (wasmExports) return wasmExports;
    // `clip.wasm` lives alongside the other static assets under `assets/`
    // on both the local web server and the relay.
    const resp = await fetch(`${baseUrl}assets/clip.wasm`);
    if (!resp.ok) {
        throw new Error(`clip.wasm fetch failed: ${resp.status}`);
    }
    const buf = await resp.arrayBuffer();
    const mod = await WebAssembly.instantiate(buf, {});
    wasmExports = mod.instance.exports;
    return wasmExports;
}

/**
 * Instantiate a new clipper at the given session viewport.
 * @param {string} baseUrl - page base, used to resolve `clip.wasm`.
 * @param {number} sessionRows
 * @param {number} sessionCols
 * @returns {Promise<{apply: (Uint8Array) => void, emit: (number, number) => Uint8Array, resizeSession: (number, number) => void, free: () => void}>}
 */
export async function createClipper(baseUrl, sessionRows, sessionCols) {
    const w = await loadWasm(baseUrl);
    let state = w.clip_new(sessionRows, sessionCols);

    return {
        apply(bytes) {
            if (!bytes || bytes.length === 0) return;
            const ptr = w.clip_alloc(bytes.length);
            new Uint8Array(w.memory.buffer, ptr, bytes.length).set(bytes);
            w.clip_apply(state, ptr, bytes.length);
            w.clip_free(ptr, bytes.length);
        },
        emit(viewerRows, viewerCols) {
            const outLenPtr = w.clip_alloc(4);
            const resultPtr = w.clip_emit(state, viewerRows, viewerCols, outLenPtr);
            const outLen = new Uint32Array(w.memory.buffer, outLenPtr, 1)[0];
            // Copy out of wasm memory before freeing so the caller is free
            // to re-enter the wasm module without clobbering the slice.
            const out = new Uint8Array(w.memory.buffer, resultPtr, outLen).slice();
            w.clip_free(resultPtr, outLen);
            w.clip_free(outLenPtr, 4);
            return out;
        },
        resizeSession(rows, cols) {
            w.clip_resize_session(state, rows, cols);
        },
        free() {
            if (state !== null) {
                w.clip_free_state(state);
                state = null;
            }
        },
    };
}
