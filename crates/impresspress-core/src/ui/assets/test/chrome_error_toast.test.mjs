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

test('a page that fans out auto-triggered requests gets ONE toast, not twenty', () => {
  // `blocks/llm/ui.rs` renders a status badge per model with
  // `hx-trigger="load"`, and `routes/models.rs` answers each with an error
  // terminal when the backend is unreachable. Twenty models used to mean twenty
  // identical toasts stacked four seconds deep.
  const page = loadChrome();
  const unreachable = envelope('Internal', 'llm status failed');
  for (let i = 0; i < 20; i += 1) {
    page.respondWithError({ status: 503, responseText: unreachable });
  }

  assert.deepEqual(page.toasts(), [{ kind: 'error', text: 'llm status failed' }]);
});

test('a genuinely different failure in the same burst still toasts', () => {
  // Suppression is on the MESSAGE, not on the burst: two distinct facts are two
  // things the operator needs to know, however close together they arrive.
  const page = loadChrome();
  page.respondWithError({ status: 503, responseText: envelope('Internal', 'llm status failed') });
  page.respondWithError({ status: 503, responseText: envelope('Internal', 'llm status failed') });
  page.respondWithError({ status: 403, responseText: envelope('PermissionDenied', 'Access denied') });

  assert.deepEqual(page.toasts(), [
    { kind: 'error', text: 'llm status failed' },
    { kind: 'error', text: 'Access denied' }
  ]);
});

test('the same failure toasts again once the window has passed', () => {
  // Suppression is a window, not a mute: a failure the operator hit, dismissed
  // and hit again later is a new event and says so.
  const page = loadChrome();
  const body = envelope('Internal', 'llm status failed');
  page.respondWithError({ status: 503, responseText: body });
  page.advance(6000);
  page.respondWithError({ status: 503, responseText: body });

  assert.deepEqual(page.toasts(), [
    { kind: 'error', text: 'llm status failed' },
    { kind: 'error', text: 'llm status failed' }
  ]);
});

test('a steady stream of the same failure stays quiet, because the window slides', () => {
  // A page polling a broken endpoint every two seconds must not re-toast every
  // five: each repeat resets the window, so it says its piece once and waits
  // for the failure to actually stop and start again.
  const page = loadChrome();
  const body = envelope('Internal', 'llm status failed');
  for (let i = 0; i < 10; i += 1) {
    page.respondWithError({ status: 503, responseText: body });
    page.advance(2000);
  }

  assert.deepEqual(page.toasts(), [{ kind: 'error', text: 'llm status failed' }]);
});

test('a request that never reached the server says so, in its own words', () => {
  // `xhr.onerror` fires `htmx:afterRequest` and then `htmx:sendError`, and the
  // `responseInfo` both carry has no `successful` field at all — it is assigned
  // only inside `handleAjaxResponse`. So a dropped connection reaches NEITHER
  // the response-error listener above nor any `if(event.detail.successful)`
  // guard, and before this branch existed it produced nothing.
  //
  // Its own sentence, not the response-error one: there is no status and no
  // body, and what the operator needs to know is that nothing was sent, so
  // retrying is the right move rather than a way to create a second row.
  const page = loadChrome();
  page.fireTransportEvent('htmx:sendError');

  assert.deepEqual(page.toasts(), [
    { kind: 'error', text: 'Could not reach the server. Check your connection and try again.' }
  ]);
});

test('a timeout says so too', () => {
  const page = loadChrome();
  page.fireTransportEvent('htmx:timeout');

  assert.deepEqual(page.toasts(), [
    { kind: 'error', text: 'The server did not answer in time. Try again.' }
  ]);
});

test('an abort is silent, because the page is the one that aborted', () => {
  // `hx-sync` superseding an in-flight request and a navigation away both land
  // here. Toasting them would manufacture noise on exactly the pages that abort
  // most, and nothing in this tree aborts a request a person is waiting on.
  const page = loadChrome();
  page.fireTransportEvent('htmx:sendAbort');

  assert.deepEqual(page.toasts(), []);
});

test('a page with no toast container does not throw', () => {
  // The shipped layout always renders one, but the listener chain must not be
  // the thing that breaks a page that does not.
  const page = loadChrome({ toastContainer: false });
  assert.doesNotThrow(() => page.respondWithError({ status: 409, responseText: '' }));
  assert.deepEqual(page.toasts(), []);
});
