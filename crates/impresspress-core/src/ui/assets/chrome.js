// The shared admin/portal chrome's browser behaviour, in load order.
//
// One file, one `<script src>`, one manifest hash. These four sections used
// to be four Rust raw strings inlined into the bottom of every rendered page
// (`ui::assets::{palette_js, drawer_js, toast_js, modal_js}`) -- 202 lines
// re-sent uncached on every request, and unreachable to a linter, a
// formatter or a source map. They are concatenated here in exactly the order
// the page used to emit them, so behaviour is unchanged:
//
//   1. command palette  (was emitted by `ui::Page::render`)
//   2. mobile drawer    (was emitted by `ui::Page::render`)
//   3. toasts           (was emitted by `ui::layout::page`)
//   4. modals           (was emitted by `ui::layout::page`)
//
// Sections 1 and 2 are IIFEs with their own idempotence guards. Sections 3
// and 4 are deliberately NOT wrapped: `openModal`/`closeModal` are called
// from `onclick` attributes and from `components::modal`, so they must stay
// top-level function declarations to remain globals, and the toast listener
// binds `document.body` directly.
//
// The page loads this with `defer` from `<head>`, so the whole file runs
// after parsing and every element these sections look for already exists --
// the same guarantee their old end-of-body placement gave them.

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

// --- 4. modal open/close ---
document.addEventListener("keydown", function(e) {
    if (e.key === "Escape") {
        var m = document.querySelector('.modal-overlay:not([hidden])');
        if (m) m.setAttribute("hidden", "");
    }
});
function openModal(id) {
    var m = document.getElementById(id);
    if (m) m.removeAttribute("hidden");
}
function closeModal(id) {
    var m = document.getElementById(id);
    if (m) m.setAttribute("hidden", "");
}
document.body.addEventListener("closeModal", function(e) {
    var d = e.detail || {};
    if (d.id) closeModal(d.id);
});
