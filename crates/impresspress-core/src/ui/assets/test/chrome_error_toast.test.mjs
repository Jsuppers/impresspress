// Run with: node --test crates/impresspress-core/src/ui/assets/test/chrome_error_toast.test.mjs
//
// Pins that a REFUSED htmx request reaches the operator.
//
// htmx 2.0.4's default `responseHandling` ends
// `{code:"[45]..", swap:false, error:true}`: a 4xx is deliberately not swapped
// and htmx raises `htmx:responseError` instead. Until `chrome.js` grew the
// listener these cover, nothing in the tree listened for that event — so a
// refusal produced no swap, no message and no change of any kind. The admin
// Variables modal just sat there.
//
// That is why this is not a cosmetic test. The 2026-09-10 live-server audit
// found that creating a variable with a key that already exists answered 500;
// it answers 409 now, and a 409 nobody can see is the same experience the 500
// was. The status code is only half the fix.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { loadChrome } from './chrome_harness.mjs';

/** The body `wafer_block::http_codec` renders for every error terminal. */
const envelope = (code, message) => JSON.stringify({ error: code, message });

test('a 409 surfaces the message the server wrote, as an error toast', () => {
  const page = loadChrome();
  page.respondWithError({
    status: 409,
    responseText: envelope(
      'AlreadyExists',
      'A variable named "SITE_MOTTO" already exists. Edit that variable, or pick another key.'
    )
  });

  assert.deepEqual(page.toasts(), [
    {
      kind: 'error',
      text: 'A variable named "SITE_MOTTO" already exists. Edit that variable, or pick another key.'
    }
  ]);
});

test('every other refusal comes through the same listener', () => {
  const page = loadChrome();
  page.respondWithError({ status: 403, responseText: envelope('PermissionDenied', 'Access denied') });
  page.respondWithError({ status: 400, responseText: envelope('InvalidArgument', 'Key is required') });

  assert.deepEqual(page.toasts(), [
    { kind: 'error', text: 'Access denied' },
    { kind: 'error', text: 'Key is required' }
  ]);
});

test('a body with no parseable message still says something, naming the status', () => {
  // The silence this listener exists to remove is not improved by an empty
  // toast, so every branch has to end in text.
  const page = loadChrome();
  page.respondWithError({ status: 502, responseText: '' });
  page.respondWithError({ status: 500, responseText: 'not json at all' });
  page.respondWithError({ status: 409, responseText: '{"error":"AlreadyExists"' });
  page.respondWithError({ status: 404, responseText: envelope('NotFound', '') });

  assert.deepEqual(page.toasts(), [
    { kind: 'error', text: 'Request failed (502)' },
    { kind: 'error', text: 'Request failed (500)' },
    { kind: 'error', text: 'Request failed (409)' },
    { kind: 'error', text: 'Request failed (404)' }
  ]);
});

test('an HTML error page is not poured into the toast', () => {
  // A refusal rendered as a full error page is also a 4xx. Its markup is not a
  // message, and `textContent` would print the whole document as one line.
  const page = loadChrome();
  page.respondWithError({
    status: 403,
    responseText: '<!doctype html><html><body><h1>Forbidden</h1></body></html>'
  });

  assert.deepEqual(page.toasts(), [{ kind: 'error', text: 'Request failed (403)' }]);
});

test('a request that failed before any response still toasts', () => {
  // `htmx:sendError` aside, an `xhr` with neither status nor body reaches this
  // listener whenever the detail is incomplete; it must not produce `(0)` or an
  // empty string.
  const page = loadChrome();
  page.respondWithError({});

  assert.deepEqual(page.toasts(), [{ kind: 'error', text: 'Request failed' }]);
});

test('a page with no toast container does not throw', () => {
  // The shipped layout always renders one, but the listener chain must not be
  // the thing that breaks a page that does not.
  const page = loadChrome({ toastContainer: false });
  assert.doesNotThrow(() => page.respondWithError({ status: 409, responseText: '' }));
  assert.deepEqual(page.toasts(), []);
});
