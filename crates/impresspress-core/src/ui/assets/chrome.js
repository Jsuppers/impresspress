// The shared admin/portal chrome's browser behaviour, in load order.
//
// One file, one `<script src>`, one manifest hash. These four sections used
// to be four Rust raw strings inlined into the bottom of every rendered page
// (`ui::assets::{palette_js, drawer_js, toast_js, modal_js}`) -- 196 lines
// re-sent uncached on every request, and unreachable to a linter, a
// formatter or a source map. They are concatenated here in exactly the order
// the page used to emit them, so behaviour is unchanged:
//
//   1. command palette  (was emitted by `ui::Page::render`)
//   2. mobile drawer    (was emitted by `ui::Page::render`)
//   3. toasts           (was emitted by `ui::layout::page`)
//   4. modals           (was emitted by `ui::layout::page`)
//
// Sections 1, 2 and 4 are IIFEs with their own idempotence guards. Section 3
// is deliberately NOT wrapped: the toast listener binds `document.body`
// directly and declares nothing. Section 4 was unwrapped too until the pages
// stopped calling `openModal`/`closeModal` from `onclick` attributes; it now
// owns the shared delegated-action listener and exposes no globals. The rule
// and the vocabulary are documented at the head of that section.
//
// The page loads this with `defer` from `<head>`, so the whole file runs
// after parsing and every element these sections look for already exists --
// the same guarantee their old end-of-body placement gave them.
//
// What `defer` does NOT preserve is *when* that happens. The four inline
// scripts ran during parse, in the same document, with no network. This one
// waits on a fetch, and on a cold cache -- or on an `embed-assets`-off build
// streaming it from object storage -- that fetch is a real round trip.
// Nothing in here paints, so the delay is mostly invisible -- the palette
// trigger, the drawer control and the modal handlers are simply inert until
// it lands, where before they worked as soon as the parser passed them --
// with one visible exception: the palette's platform swap in section 1
// rewrites a server-rendered glyph. See the note there.

// --- 1. command palette ---
(function () {
  if (window.__cmdkInit) return;
  window.__cmdkInit = true;
  const el = document.getElementById('cmdk');
  if (!el) return;
  const input = document.getElementById('cmdk-input');
  const list = document.getElementById('cmdk-list');

  const items = () => Array.from(list.querySelectorAll('.palette__item'));
  let selected = 0;

  function open() {
    el.dataset.open = 'true';
    el.setAttribute('aria-hidden', 'false');
    input.value = '';
    apply('');
    requestAnimationFrame(() => input.focus());
  }
  function close() {
    el.dataset.open = 'false';
    el.setAttribute('aria-hidden', 'true');
  }
  function visibleItems() { return items().filter(i => !i.classList.contains('is-hidden')); }

  function apply(query) {
    const q = query.trim().toLowerCase();
    items().forEach(i => {
      const k = (i.dataset.keywords || '').toLowerCase();
      const match = !q || k.includes(q);
      i.classList.toggle('is-hidden', !match);
      i.setAttribute('aria-selected', 'false');
    });
    const vis = visibleItems();
    selected = 0;
    if (vis[0]) vis[0].setAttribute('aria-selected', 'true');
  }

  function move(delta) {
    const vis = visibleItems();
    if (!vis.length) return;
    vis[selected]?.setAttribute('aria-selected', 'false');
    selected = (selected + delta + vis.length) % vis.length;
    vis[selected].setAttribute('aria-selected', 'true');
    vis[selected].scrollIntoView({ block: 'nearest' });
  }

  function activate() {
    const vis = visibleItems();
    const sel = vis[selected];
    if (!sel?.dataset.href) return;
    if (sel.dataset.external === 'true') {
      window.open(sel.dataset.href, '_blank', 'noopener,noreferrer');
    } else {
      window.location.assign(sel.dataset.href);
    }
  }

  // Hotkeys
  document.addEventListener('keydown', (e) => {
    const isMod = e.metaKey || e.ctrlKey;
    if (isMod && e.key.toLowerCase() === 'k') { e.preventDefault(); open(); return; }
    if (el.dataset.open !== 'true') return;
    if (e.key === 'Escape') { e.preventDefault(); close(); }
    else if (e.key === 'ArrowDown') { e.preventDefault(); move(1); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); move(-1); }
    else if (e.key === 'Enter') { e.preventDefault(); activate(); }
  });

  // Click triggers
  document.addEventListener('click', (e) => {
    const t = e.target.closest('[data-action]');
    if (!t) return;
    if (t.dataset.action === 'palette-open') { e.preventDefault(); open(); }
    if (t.dataset.action === 'palette-close') { e.preventDefault(); close(); }
  });

  // The shortcut hint defaults to the Mac glyph; swap to Ctrl elsewhere so
  // the advertised key matches what the keydown handler above accepts.
  //
  // This is the one thing in this file a visitor can see happen. It used to
  // run during parse; it now waits on this file's fetch, so a non-Mac visitor
  // on a cold cache sees the server-rendered `⌘` painted and then replaced.
  // The string grows from one glyph to four characters, so on a narrow
  // viewport the topbar re-lays out rather than merely re-texting. Rendering
  // the label server-side would trade that for either a wrong glyph on Mac or
  // a changed rendered output on every shelled page; the flash is on a
  // once-per-deploy cold cache only, and the button works throughout, so it
  // is documented rather than designed away. If it ever needs to go, the fix
  // is a platform-neutral server-rendered label, not an inline script.
  if (!/Mac|iPhone|iPad|iPod/.test(navigator.platform || '')) {
    document.querySelectorAll('.topbar__palette-cmd').forEach((n) => { n.textContent = 'Ctrl'; });
    document.querySelectorAll('.shell__palette-icon').forEach((n) => { n.textContent = 'Ctrl K'; });
  }

  // Linked table rows (`.data-table__row--linked`) style as clickable; make
  // the whole row actually navigate via its row-href anchor, unless the
  // click landed on an interactive element of its own.
  document.addEventListener('click', (e) => {
    const row = e.target.closest('.data-table__row--linked');
    if (!row || e.target.closest('a, button, input, select, label, textarea')) return;
    const anchor = row.querySelector('.data-table__row-href a');
    if (anchor) anchor.click();
  });

  // Item click → navigate
  list.addEventListener('click', (e) => {
    const item = e.target.closest('.palette__item');
    if (!item?.dataset.href) return;
    if (item.dataset.external === 'true') {
      window.open(item.dataset.href, '_blank', 'noopener,noreferrer');
    } else {
      window.location.assign(item.dataset.href);
    }
  });

  input.addEventListener('input', (e) => apply(e.target.value));

  // Keyboard scrolling for the app shell. The document never scrolls (the
  // .shell grid is 100vh; .shell__body is the real scroller), so with no
  // focused element PageDown/PageUp/Space/Home/End/arrows would silently do
  // nothing. Registered after the palette handler above, so an open palette
  // (which preventDefaults its own keys) wins. Only fires when the event
  // target is the page itself — typing in fields and focused widgets keep
  // their native behavior.
  document.addEventListener('keydown', (e) => {
    if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.altKey) return;
    if (e.target !== document.body && e.target !== document.documentElement) return;
    const scroller = document.querySelector('.shell__body');
    if (!scroller) return;
    const pageStep = scroller.clientHeight * 0.9;
    const lineStep = 40;
    let dy;
    switch (e.key) {
      case 'PageDown': dy = pageStep; break;
      case 'PageUp': dy = -pageStep; break;
      case ' ': dy = e.shiftKey ? -pageStep : pageStep; break;
      case 'ArrowDown': dy = lineStep; break;
      case 'ArrowUp': dy = -lineStep; break;
      case 'Home': scroller.scrollTo({ top: 0 }); e.preventDefault(); return;
      case 'End': scroller.scrollTo({ top: scroller.scrollHeight }); e.preventDefault(); return;
      default: return;
    }
    scroller.scrollBy({ top: dy });
    e.preventDefault();
  });
})();

// --- 2. mobile sidebar drawer ---
(function () {
  if (window.__drawerInit) return;
  window.__drawerInit = true;
  var body = document.body;
  function open() { body.setAttribute('data-drawer-open', 'true'); }
  function close() { body.removeAttribute('data-drawer-open'); }
  document.addEventListener('click', function (e) {
    var t = e.target;
    if (!(t instanceof Element)) return;
    var actEl = t.closest('[data-action]');
    var action = actEl ? actEl.getAttribute('data-action') : null;
    if (action === 'drawer-open') { open(); e.preventDefault(); return; }
    if (action === 'drawer-close') { close(); e.preventDefault(); return; }
    if (body.hasAttribute('data-drawer-open') && t.closest('.sidebar a')) {
      close();
    }
  });
  document.addEventListener('keydown', function (e) {
    if (e.key === 'Escape' && body.hasAttribute('data-drawer-open')) {
      close();
    }
  });
})();

// --- 3. toast notifications (htmx HX-Trigger channel) ---
document.body.addEventListener("showToast", function(e) {
    var d = e.detail || {};
    var c = document.getElementById("toast-container");
    if (!c) return;
    var t = document.createElement("div");
    var kind = ["success", "error", "warning", "info"].indexOf(d.type) >= 0 ? d.type : "info";
    t.className = "toast toast-" + kind;
    var message = document.createElement("span");
    message.textContent = String(d.message || "");
    var dismiss = document.createElement("button");
    dismiss.className = "toast-dismiss";
    dismiss.type = "button";
    dismiss.setAttribute("aria-label", "Dismiss");
    dismiss.textContent = "×";
    dismiss.addEventListener("click", function() { t.remove(); });
    t.appendChild(message);
    t.appendChild(dismiss);
    c.appendChild(t);
    setTimeout(function() { t.remove(); }, 4000);
});

// --- 4. modals, and the shared delegated-action listener ---
//
// Everything a modal does lives in this one IIFE, so `openModal`/`closeModal`
// are no longer globals. They had to be, because pages spelled their controls
// as `onclick="openModal('create-role')"` attributes. Those are gone; the only
// other caller was the htmx `HX-Trigger` response-header channel, handled here.
//
// ## The delegated-action rule
//
// Page markup carries no `on*=` attribute. A control declares WHAT it does with
// `data-action="<verb>"` plus whatever `data-*` operands the verb needs, and a
// delegated listener on `document` reads them back. The reason is written out at
// `blocks/admin/pages/network.rs`: maud escapes an attribute VALUE as HTML, but
// an `onclick` value is not HTML, it is JavaScript source, so a page that ever
// interpolates request-shaped text into one has a script-execution site with no
// escaping in the way. A `data-*` operand read with `getAttribute` is inert text
// whatever it holds. It also collapses the same few behaviours — open a modal,
// close a modal, reveal a password field — from a hundred hand-written copies
// down to one.
//
// `data-action` is one namespace shared by every script in the tree, so a verb
// is prefixed by whoever owns it. The verbs below are chrome's; a block's own
// script owns verbs named for that block's page and ignores the rest. A listener
// that does not recognise a verb MUST fall through silently — more than one
// delegated listener sees every click.
//
// Chrome's verbs:
//   modal-open    + data-modal-target="<id>"   reveal that modal overlay
//   modal-close   + data-modal-target="<id>"   hide it (omit the operand to
//                                              close the enclosing overlay)
//   reveal-toggle + data-reveal-target="<id>"  swap a password field between
//                                              masked and plain, and swap the
//                                              button's label when it carries
//                                              data-reveal-show/-hide
//   mirror-value  + data-mirror-target="<id>"  on change, copy this control's
//                                              value into that field (the
//                                              colour swatch beside its hex box)
//   copy-text     + data-copy-source="<id>"    put that element's text on the
//                                              clipboard and flash "Copied" on
//                                              the button for 1.5s
//   drawer-open / drawer-close                 section 2 above
//
// Plus two attributes with no verb, because they describe the element rather
// than a control acting on it:
//   .modal-overlay[data-modal-dismiss]   a click on the backdrop closes it
//   [data-stop-propagation]              a click inside it reaches no ancestor
//                                        listener — the escape hatch for a link
//                                        nested in a clickable card
//   [data-submit-on-enter]               a textarea where Enter submits the
//                                        enclosing form and Shift+Enter keeps
//                                        inserting a newline (chat composers)
(function () {
    if (window.__modalInit) return;
    window.__modalInit = true;

    // `data-stop-propagation` has to run in the CAPTURE phase: the listener it
    // exists to silence (htmx's, bound on the enclosing card) sits between
    // `document` and the link, so a bubbling listener would fire too late.
    // Stopping propagation does not cancel the default action, so the link
    // still navigates — which is exactly what the inline
    // `event.stopPropagation()` it replaced did.
    document.addEventListener("click", function (e) {
        var t = e.target;
        if (t instanceof Element && t.closest("[data-stop-propagation]")) {
            e.stopPropagation();
        }
    }, true);

    function openModal(id) {
        var m = document.getElementById(id);
        if (m) m.removeAttribute("hidden");
    }
    function closeModal(id) {
        var m = document.getElementById(id);
        if (m) m.setAttribute("hidden", "");
    }

    function revealToggle(btn) {
        var input = document.getElementById(btn.getAttribute("data-reveal-target") || "");
        if (!input) return;
        var masked = input.type === "password";
        input.type = masked ? "text" : "password";
        // A button with no label operands keeps the label it was rendered with
        // — the auth pages use one static "Toggle password visibility" for both
        // states, and did before this was delegated.
        var label = btn.getAttribute(masked ? "data-reveal-hide" : "data-reveal-show");
        if (label === null) return;
        btn.title = label;
        btn.setAttribute("aria-label", label + " value");
    }

    // The text is read out of the DOM rather than carried in the operand: a
    // secret that is only shown once should not also be written into an
    // attribute, and reading `innerText` needs no escaping at all.
    function copyText(btn) {
        var src = document.getElementById(btn.getAttribute("data-copy-source") || "");
        var text = src ? src.innerText : "";
        if (!text || !navigator.clipboard) return;
        navigator.clipboard.writeText(text).then(function () {
            var was = btn.textContent;
            btn.textContent = "Copied";
            setTimeout(function () { btn.textContent = was; }, 1500);
        });
    }

    document.addEventListener("click", function (e) {
        var t = e.target;
        if (!(t instanceof Element)) return;

        // Backdrop dismissal: only a click that landed on the overlay itself,
        // never one that bubbled out of the dialog inside it.
        if (t.matches(".modal-overlay[data-modal-dismiss]")) {
            closeModal(t.id);
            return;
        }

        var el = t.closest("[data-action]");
        if (!el) return;
        var action = el.getAttribute("data-action");
        if (action === "modal-open") {
            openModal(el.getAttribute("data-modal-target") || "");
            e.preventDefault();
        } else if (action === "modal-close") {
            var target = el.getAttribute("data-modal-target");
            if (target === null) {
                var overlay = el.closest(".modal-overlay");
                if (overlay) closeModal(overlay.id);
            } else {
                closeModal(target);
            }
            e.preventDefault();
        } else if (action === "reveal-toggle") {
            revealToggle(el);
            e.preventDefault();
        } else if (action === "copy-text") {
            copyText(el);
            e.preventDefault();
        }
    });

    document.addEventListener("change", function (e) {
        var el = e.target;
        if (!(el instanceof Element)) return;
        if (el.getAttribute("data-action") !== "mirror-value") return;
        var target = document.getElementById(el.getAttribute("data-mirror-target") || "");
        if (target) target.value = el.value;
    });

    document.addEventListener("keydown", function (e) {
        if (e.key === "Escape") {
            var m = document.querySelector(".modal-overlay:not([hidden])");
            if (m) m.setAttribute("hidden", "");
            return;
        }
        // Two chat composers had the same nine-word `onkeydown` attribute.
        if (e.key !== "Enter" || e.shiftKey) return;
        var box = e.target;
        if (!(box instanceof Element) || !box.hasAttribute("data-submit-on-enter")) return;
        var form = box.closest("form");
        if (!form) return;
        e.preventDefault();
        form.requestSubmit();
    });

    // The htmx response-header channel, both directions. A handler that
    // answers with a modal's contents says so in `HX-Trigger-After-Swap`
    // rather than appending a script that reveals the overlay itself — four
    // copies of that script existed, one of them built by `format!` with a
    // record id interpolated into JavaScript source.
    document.body.addEventListener("closeModal", function (e) {
        var d = e.detail || {};
        if (d.id) closeModal(d.id);
    });
    document.body.addEventListener("openModal", function (e) {
        var d = e.detail || {};
        if (d.id) openModal(d.id);
    });
})();
