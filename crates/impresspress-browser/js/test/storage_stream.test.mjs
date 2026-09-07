// Run with: node --import ./js/test/node-hooks.mjs --test js/test/storage_stream.test.mjs
// (see node-hooks.mjs's header comment for why the --import hook is needed).
//
// The chunked OPFS read/write bridge behind `storage.rs`'s `get_streaming` and
// `put_streaming`. Neither can be exercised from Rust — the bridge functions
// are wasm-bindgen extern imports and `wasm-pack test --node` has no OPFS — so
// the File System Access surface bridge.js actually uses is faked here and the
// real bridge functions are driven against it.
//
// What the fake has to be right about is small: directory/file handles that
// create on demand, `createWritable()` accepting repeated `write()` calls, and
// `getFile()` answering a `Blob`-shaped object with `size`, `text()` and
// `stream()`. Node's own `Blob` supplies the last three, so the round trip
// below reads the same bytes back through a real `ReadableStream`.
import { test, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import {
    storageGetStream,
    storagePut,
    storagePutStreamStart,
    storagePutStreamChunk,
    storagePutStreamFinish,
    storagePutStreamAbort,
    readerNextChunk,
    readerCancel,
} from '../bridge.js';

// ─── An in-memory stand-in for OPFS ──────────────────────────────────────────

function makeDir() {
    const dirs = new Map();
    const files = new Map();
    const handle = {
        async getDirectoryHandle(name, opts = {}) {
            if (!dirs.has(name)) {
                if (!opts.create) {
                    throw notFound(name);
                }
                dirs.set(name, makeDir());
            }
            return dirs.get(name);
        },
        async getFileHandle(name, opts = {}) {
            if (!files.has(name)) {
                if (!opts.create) {
                    throw notFound(name);
                }
                files.set(name, makeFile());
            }
            return files.get(name);
        },
        async removeEntry(name) {
            if (!files.delete(name) && !dirs.delete(name)) {
                throw notFound(name);
            }
        },
    };
    return handle;
}

function makeFile() {
    let bytes = new Uint8Array(0);
    return {
        async getFile() {
            // A `Blob` gives `size`, `text()` and a real `stream()`, which is
            // what the bridge reads.
            return new Blob([bytes]);
        },
        async createWritable() {
            const parts = [];
            let closed = false;
            return {
                async write(chunk) {
                    if (closed) throw new Error('write after close');
                    parts.push(
                        typeof chunk === 'string'
                            ? new TextEncoder().encode(chunk)
                            : new Uint8Array(chunk),
                    );
                },
                async close() {
                    closed = true;
                    const total = parts.reduce((n, p) => n + p.byteLength, 0);
                    const out = new Uint8Array(total);
                    let at = 0;
                    for (const p of parts) {
                        out.set(p, at);
                        at += p.byteLength;
                    }
                    bytes = out;
                },
                async abort() {
                    closed = true;
                },
            };
        },
    };
}

function notFound(name) {
    const err = new Error(`no such entry: ${name}`);
    err.name = 'NotFoundError';
    return err;
}

beforeEach(() => {
    const root = makeDir();
    // Node defines `navigator` as a getter-only global, so it is redefined
    // rather than assigned.
    Object.defineProperty(globalThis, 'navigator', {
        configurable: true,
        value: { storage: { getDirectory: async () => root } },
    });
});

async function drain(streamId) {
    const chunks = [];
    for (;;) {
        const chunk = await readerNextChunk(streamId);
        if (chunk === null) break;
        chunks.push(...chunk);
    }
    return chunks;
}

// ─── Tests ───────────────────────────────────────────────────────────────────

test('a chunked write is readable as one object, with the size it actually received', async () => {
    const id = await storagePutStreamStart('docs', 'nested/report.bin');
    await storagePutStreamChunk(id, new Uint8Array([1, 2, 3]));
    await storagePutStreamChunk(id, new Uint8Array([4, 5]));
    await storagePutStreamFinish(id, 'application/octet-stream');

    const started = await storageGetStream('docs', 'nested/report.bin');
    assert.equal(started.meta.content_type, 'application/octet-stream');
    assert.equal(
        started.meta.size,
        5,
        'the sidecar size must be the bytes that arrived, not a length declared up front',
    );
    assert.deepEqual(await drain(started.stream_id), [1, 2, 3, 4, 5]);
});

test('an aborted chunked write leaves no metadata, so the object is not served as complete', async () => {
    const id = await storagePutStreamStart('docs', 'half.bin');
    await storagePutStreamChunk(id, new Uint8Array([1, 2, 3]));
    await storagePutStreamAbort(id);

    // Abort is called from every Rust error path and must not itself throw,
    // including when it is called twice or on an id that never existed.
    await storagePutStreamAbort(id);
    await storagePutStreamAbort('write-does-not-exist');

    // A chunk against the abandoned id must be refused rather than silently
    // dropped — dropped bytes would produce a short object reporting success.
    await assert.rejects(
        () => storagePutStreamChunk(id, new Uint8Array([9])),
        /unknown writer id/,
    );
    await assert.rejects(
        () => storagePutStreamFinish(id, 'text/plain'),
        /unknown writer id/,
    );
});

test('a buffered put is readable through the streaming path, and the reverse', async () => {
    // The two write paths and the two read paths write and read the same
    // object and the same sidecar; nothing about an object records which one
    // produced it.
    await storagePut('docs', 'a.txt', new TextEncoder().encode('hello'), 'text/plain');

    const started = await storageGetStream('docs', 'a.txt');
    assert.equal(started.meta.content_type, 'text/plain');
    assert.equal(started.meta.size, 5);
    assert.deepEqual(await drain(started.stream_id), [104, 101, 108, 108, 111]);
});

test('streaming a missing object rejects as NotFoundError, like the buffered read', async () => {
    await assert.rejects(() => storageGetStream('docs', 'absent.txt'), (err) => {
        // `storage.rs::map_rejection` keys on exactly this name to answer
        // `StorageError::NotFound` rather than `Internal`.
        assert.equal(err.name, 'NotFoundError');
        return true;
    });
});

test('an empty object streams as zero chunks, not as an error', async () => {
    const id = await storagePutStreamStart('docs', 'empty.bin');
    await storagePutStreamFinish(id, 'application/octet-stream');

    const started = await storageGetStream('docs', 'empty.bin');
    assert.equal(started.meta.size, 0);
    assert.deepEqual(await drain(started.stream_id), []);
});

test('abandoning a read releases the OPFS reader', async () => {
    await storagePut('docs', 'a.txt', new TextEncoder().encode('hello'), 'text/plain');
    const started = await storageGetStream('docs', 'a.txt');

    await readerCancel(started.stream_id);
    await assert.rejects(() => readerNextChunk(started.stream_id), /unknown stream id/);
});
