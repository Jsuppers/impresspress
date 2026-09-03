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

test('a body that is not an object at all is not a hash conflict — and does not throw', () => {
  const { handle } = instantiate();
  // `refusalBody` hands `null` to these readers whenever the response was
  // not the block's own JSON (an intermediary's error page, a truncated
  // body). `'current_sha256' in null` is a `TypeError`, which would escape
  // `save`/`create`/`remove` into the log in place of the server's own
  // explanation and leave the human staring at a closed dialog.
  assert.equal(handle.isFileConflict(null), false);
});

test("refusalBody parses the block's JSON refusals and rejects every other shape", async () => {
  const { handle } = instantiate();
  const body = (text) => handle.refusalBody({ text: async () => text });

  assert.deepEqual(await body('{"error":"InvalidArgument","message":"bad path"}'), {
    error: 'InvalidArgument',
    message: 'bad path'
  });
  // Not JSON at all — an intermediary's HTML error page.
  assert.equal(await body('<!doctype html><title>502</title>'), null);
  // Valid JSON, but not a shape either reader can be asked about.
  assert.equal(await body('"just a string"'), null);
  assert.equal(await body('null'), null);
  assert.equal(await body(''), null);
});

test("refusalMessage prefers the server's own words and falls back otherwise", () => {
  const { handle } = instantiate();
  assert.equal(
    handle.refusalMessage(
      { error: 'ResourceExhausted', message: 'the limit is 1048576 bytes' },
      'x'
    ),
    'the limit is 1048576 bytes'
  );
  // No body, no message, or a message that is not a non-empty string: the
  // caller's fallback names what the human just tried and the status.
  assert.equal(handle.refusalMessage(null, 'Save refused (400).'), 'Save refused (400).');
  assert.equal(
    handle.refusalMessage({ error: 'Internal' }, 'Save refused (500).'),
    'Save refused (500).'
  );
  assert.equal(handle.refusalMessage({ message: '' }, 'Save refused (400).'), 'Save refused (400).');
  assert.equal(handle.refusalMessage({ message: 42 }, 'Save refused (400).'), 'Save refused (400).');
});
