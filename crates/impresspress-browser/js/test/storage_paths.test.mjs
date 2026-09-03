// Run with: node --import ./js/test/node-hooks.mjs --test js/test/storage_paths.test.mjs
// (see node-hooks.mjs's header comment for why the --import hook is needed).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { splitKey, joinKey, validateSegments } from '../bridge.js';

test('splitKey separates directories from the leaf', () => {
  assert.deepEqual(splitKey('assets/js/app.js'), { dirs: ['assets', 'js'], leaf: 'app.js' });
  assert.deepEqual(splitKey('index.html'), { dirs: [], leaf: 'index.html' });
});

test('splitKey rejects traversal, empty and dot segments', () => {
  for (const bad of ['', '/', 'a//b', '../x', 'a/../b', './a', 'a/.', 'a/']) {
    assert.throws(() => splitKey(bad), TypeError, bad);
  }
});

test('splitKey rejects a metadata sidecar name so a caller cannot forge one', () => {
  assert.throws(() => splitKey('index.html.__meta__'), TypeError);
});

test('joinKey is the inverse of splitKey', () => {
  const key = 'assets/js/app.js';
  const { dirs, leaf } = splitKey(key);
  assert.equal(joinKey(dirs, leaf), key);
});

test('validateSegments accepts unicode file names and spaces, rejects separators and control characters', () => {
  // Spaces are legitimate in a file name (OPFS allows them; the Rust-side
  // native storage path rules in Plan 1 Task 6 allow them too), so
  // 'my file.txt' must NOT throw.
  assert.doesNotThrow(() => validateSegments(['héllo', 'my file.txt']));
  assert.throws(() => validateSegments(['a\\b']), TypeError);
  assert.throws(() => validateSegments(['a/b']), TypeError);
  assert.throws(() => validateSegments(['a\x00b']), TypeError);
  assert.throws(() => validateSegments(['a\x7fb']), TypeError);
});

test('the sidecar suffix is refused on every segment, not just the leaf', () => {
  // A DIRECTORY named `page.html.__meta__` would collide with the sidecar of
  // a sibling file named `page.html`. The Rust producer refuses the suffix on
  // every segment (`paths.rs::validate_path`); so must this.
  assert.throws(() => splitKey('a.__meta__/c'), TypeError);
  assert.throws(() => validateSegments(['a.__meta__', 'c']), TypeError);
  // Only as a suffix, and only on a whole segment.
  assert.doesNotThrow(() => validateSegments(['a.__meta__b', 'c']));
});
