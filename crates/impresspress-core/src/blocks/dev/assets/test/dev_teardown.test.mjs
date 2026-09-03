// Run with: node --test crates/impresspress-core/src/blocks/dev/assets/test/dev_teardown.test.mjs
//
// Pins the ONE thing that separates "this page is going away" from "this
// page might come back": `pagehide`'s `event.persisted`. Aborting the
// workspace is irreversible — an `AbortController` cannot be reset, and the
// abort handler unregisters every tool, stops the progress poller and logs
// that the session expired. Doing that for a bfcache-eligible navigation
// leaves the restored document looking alive with nothing behind it.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { instantiate } from './harness.mjs';

test('pagehide into the bfcache (persisted) does NOT abort the workspace', () => {
  const { handle, fireWindow } = instantiate({ hasModelContext: true });
  fireWindow('pagehide', { persisted: true });
  assert.equal(
    handle.abort.signal.aborted,
    false,
    'the document can come back on Back with its state intact; aborting here is permanent'
  );
});

test('pagehide for a real unload (not persisted) aborts the workspace', () => {
  const { handle, fireWindow } = instantiate({ hasModelContext: true });
  fireWindow('pagehide', { persisted: false });
  assert.equal(handle.abort.signal.aborted, true);
});
