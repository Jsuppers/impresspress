// Run with: node --test crates/impresspress-core/src/blocks/dev/assets/test/dev_conflict.test.mjs
//
// Pins `isFileConflict` (M3): `/b/dev/api/files/write` answers 409 for two
// unrelated reasons that share a status — a real hash conflict
// (`FileConflict`, `files.rs::conflict`) and the block-count quota refusal
// (`QuotaError::TooManyBlocks::into_response`, a bare `{error, message}`) —
// and `save`/`create` must tell them apart from the body shape, not assume
// every 409 is a hash conflict.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { instantiate } from './harness.mjs';

test('a FileConflict body (current_sha256 present, even when null) is a hash conflict', () => {
  const { handle } = instantiate();
  assert.equal(
    handle.isFileConflict({ path: 'site/index.html', current_sha256: 'abc123', current_size: 42 }),
    true
  );
  // A conflict against a path with no file at all still carries the key,
  // just with a `null` value — `FileConflict`'s fields are never omitted.
  assert.equal(
    handle.isFileConflict({ path: 'site/gone.html', current_sha256: null, current_size: null }),
    true
  );
});

test('the quota refusal body ({error, message}) is NOT a hash conflict', () => {
  const { handle } = instantiate();
  assert.equal(
    handle.isFileConflict({
      error: 'AlreadyExists',
      message: 'the workspace already defines 32 blocks, which is the limit'
    }),
    false
  );
});
