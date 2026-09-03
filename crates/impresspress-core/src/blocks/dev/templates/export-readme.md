# {{TITLE}}

An ImpressPress site, exported from the development sandbox on
{{DATE}} — generation `{{GENERATION_ID}}`.

Everything here runs in your browser. There is no server to deploy, no
database to provision and nothing to configure: the runtime is a WebAssembly
service worker, the database is SQLite compiled to wasm, and the files it
serves live in this folder.

## Serve it

A static file server is all it needs — but it must be *served*, not opened
from the filesystem: a service worker cannot be registered from a `file://`
URL.

    python3 -m http.server 8000
    # or
    npx serve -l 8000

Then open <http://localhost:8000/>. The first load compiles the runtime and
imports `seed/`, so it takes a few seconds; every load after that is instant.

## What is in here

    index.html loader.js sw.js *.js *.wasm snippets/ vendor/
        The runtime shell — {{SHELL_FILES}} files, copied from the sandbox
        that exported this. Development mode is OFF in this copy
        (`const DEV_ENABLED = false;` in `sw.js`), so there is no `/b/dev`
        workspace, no in-browser compiler and no agent tooling here. This is
        the site, not the sandbox that built it.

    seed/manifest.json
        What the runtime imports on its first boot: every file below, with
        its SHA-256 and size. The runtime verifies all of them and refuses
        the whole import if any disagrees — so editing a file under `seed/`
        without updating its hash here stops the site from seeding at all.

    seed/site/**
        Your site's own files — {{SITE_FILES}} of them.

    seed/blocks/<name>.wasm
        Each compiled backend block ({{BLOCKS}} in total), plus its full Rust
        source under `seed/blocks/<name>/`. The source is included so this
        export can be edited and recompiled, not just run.

    seed/data.json
        A snapshot of the data the sandbox held: products, offers and their
        components, product groups, types and presets, non-sensitive site
        settings, and the user accounts — {{TABLE_ROWS}} rows in total.

## What is *not* in here, and what is

Deliberately left out: sessions, refresh and verification tokens, the audit
log, purchases, refunds, payment links, provider operations, Stripe events
and webhook leases, and every setting marked sensitive. Stripe linkage on the
exported products and offers is reset to "not synced" — the ids belonged to
the exporting instance's Stripe account, not to yours.

Deliberately included, and worth knowing about: **`seed/data.json` carries
your user accounts, including their password hashes.** They are yours — you
created them in your own browser-local sandbox — and they are in here so that
signing in to this copy works with the same credentials. They are Argon2
hashes, not plaintext, but treat this folder the way you would treat any
export of an account table: do not publish it anywhere you would not publish
a password database.

Sign in at `/b/auth/login`. The sandbox's own starter account is
`{{ADMIN_EMAIL}}` with the password you used there.

## Re-importing it

`seed/` is exactly the format the sandbox itself reads. Drop this folder's
`seed/` directory into another ImpressPress bundle and its first boot will
import the same site, blocks and data — that is how this export was produced
and how it is read back. Nothing about it is export-only.
