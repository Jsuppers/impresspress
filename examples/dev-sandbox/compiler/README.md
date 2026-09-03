# The dev sandbox's compiler

`dev.impresspress.org` compiles agent-written Rust blocks **in the browser**.
The compiler is [Rubrc](https://github.com/oligamiq/rubrc) — rustc, cargo and
LLVM built to wasm and composed into one component — packaged here as
versioned static assets and wrapped in a message protocol the `/b/dev` page
speaks.

Nothing in this directory is a fork. `build-compiler.sh` checks out rubrc at
the commit `PIN.json` names, builds its component with rubrc's own recipe, and
bundles **our** worker against two of its source trees. The pieces we wrote are
`src/worker-entry.ts` (the protocol, the WASI farm, the artifact capture),
`src/vfs-runner.ts` (rubrc's `util_cmd.ts` with the UI taken out) and the
scripts. Everything else is rubrc's, at its own licence: MIT OR Apache-2.0,
which is also the licence of the rustc/cargo/LLVM wasm it embeds.

## Layout

```
compiler/
  PIN.json                       every input: rubrc's commit, the composer, Binaryen, the sysroot
  src/ansi.ts                    ANSI stripping for the shell transcript
  build-compiler.sh              PIN.json -> dist/<version>/            (~20 min from cold)
  src/protocol.ts                the page <-> worker contract
  src/worker-entry.ts            the worker the page creates
  src/vfs-runner.ts              the worker that runs the toolchain component
  src/probe.html                 the verification page (see "What was confirmed")
  scripts/prepare-vfs-asset.mjs  brotli + split the composed wasm
  scripts/write-manifest.mjs     dist/manifest.json
  scripts/verify-compiler-assets.mjs   what `build.sh --check` runs
  scripts/serve-probe.mjs        the probe's server, with the sandbox's COOP/COEP
  scripts/run-probe.mjs          runs the probe headlessly and prints the numbers
  dist/                          gitignored build output, overlaid at /__impresspress_dev/compiler/
  .rubrc/ .cache/ node_modules/  gitignored build inputs
```

`dist/` is not committed: it is a quarter of a gigabyte, and it is fully
determined by `PIN.json`, so CI caches it on that file's hash (Task 8).

## Building

```bash
compiler/build-compiler.sh          # or: examples/dev-sandbox/build.sh, which calls it when stale
```

Needs node >= 22, rustup, curl and tar. It downloads what it needs (rubrc,
`wasi_virt_layer-cli`, Binaryen, the sysroot tarball), checks every download
against the sha256 in `PIN.json`, and caches each phase, so a re-run after
editing `src/**` takes seconds. From cold it takes about twenty minutes,
almost all of it `wasm-opt -Oz` over ~230 MB of toolchain wasm.

Two things the script insists on that are easy to get wrong by hand:

* **Binaryen is pinned to an upstream release.** `wasi_virt_layer` passes
  `--enable-shared-everything`, which Binaryen <= 116 rejects — including the
  0.116.1 it vendors — and npm's `binaryen` package is a JS port that is ~13x
  slower here and runs into node's heap ceiling on the 94 MB LLVM module.
* **`wasm32-wasip1-threads` on both toolchains, plus `rust-src` on nightly.**
  The composition builds rubrc's VFS crate with `-Zbuild-std=std,panic_unwind`.

### Why the sysroot is vendored

Rubrc fetches its standard library at runtime from
`https://oligamiq.github.io/rust_wasm/v0.2.0/<triple>.tar.br`
(`lib/src/sysroot.ts`). A sandbox whose compiler's standard library comes from
a third-party host is a supply chain we do not control, so `build-compiler.sh`
downloads that tarball at build time, checks its sha256 against `PIN.json`,
and vendors it into `dist/<version>/sysroot/`. `worker-entry.ts` answers the
component's `sysrootStartFetch` bridge call from there — the browser makes no
cross-origin request at all.

### Why the component is split

Cloudflare will not serve a static asset over 25 165 824 bytes and the
composed component is 365 MiB (the four toolchain modules going in are
~230 MB; single-memory lowering grows them). `prepare-vfs-asset.mjs` brotli-compresses it
and splits it into `vfs.core-<hash>.wasm.br.part-NNN` beside a
`vfs.core-<hash>.wasm.br.json` describing them; `vfs-runner.ts` fetches the
parts in order, pipes them through a brotli decoder into
`WebAssembly.compileStreaming`, and caches the compiled module in IndexedDB so
the second visit skips the download. The manifest shape is rubrc's own, so
their loader and ours read the same files.

### `v1-dist`

Rubrc's `prepare-vfs-asset.mjs` fetches a `v1-dist` branch, so the first thing
this task checked was whether that branch already carries a composed and split
component we could pin instead of composing. It does not: `v1-dist`
(`a8521e69d5eb5369d897022bf38b8d0627fb4c98`, "Preserve previous deployment as
v1") is a snapshot of the old *page* — Monaco, xterm, their chunks — with no
`vfs.core-*` file in it at all. So we compose, from the pinned sources, with
pinned tools.

## The protocol

`src/protocol.ts` is the contract and the types are the documentation; this is
the shape of a session. The page creates the worker from
`manifest.json`'s `entry` (`/__impresspress_dev/compiler/<version>/worker.js`,
`{ type: "module" }`) and then:

```
page → { type: 'init', id }
     ← { type: 'progress', id, stage: 'download', loaded, total }   (repeatedly)
     ← { type: 'progress', id, stage: 'initializing', detail }
     ← { type: 'ready', id, rustcVersion }

page → { type: 'compile', id, crateName, files: { 'Cargo.toml': '…', 'src/lib.rs': '…' },
         target: 'wasm32-wasip1', release: true }
     ← { type: 'progress', id, stage: 'compiling', detail }         (repeatedly)
     ← { type: 'result', id, success, artifact?, stdout, stderr, diagnostics, elapsedMs }

page → { type: 'cancel', id }
     ← { type: 'result', id, success: false, cancelled: true, … }
```

* **One compile at a time.** A `compile` that arrives while another is in
  flight is answered with a failed `result`, not queued.
* **The artifact is transferred**, not copied: after `result`, the buffer
  belongs to the page.
* **`cancel` spends the worker.** Rubrc's shell runs a command on a session
  thread that nothing outside it can unwind, so the worker cannot abandon a
  compile in progress. It answers `{ cancelled: true }` and marks itself
  broken; **the adapter must `terminate()` it and `init` a fresh one.** That
  costs a re-instantiation, not a re-download — the compiled module is in
  IndexedDB.
* **`diagnostics`** are `{ file, line, column, severity, message, code? }`.
* **`stdout`** is the shell transcript with ANSI stripped (cargo writes its
  diagnostics there); **`stderr`** is what the guest wrote to fd 2 outside the
  shell's own stream, which is usually empty.

### How a compile actually happens

Worth knowing before changing `worker-entry.ts`: the component is a *terminal*.
There is no API for "build this crate". A compile is

1. each file written through the VFS's write-file event
   (`input_string` with session `0xEEEEEEEE`, a JSON `{path, content}`),
2. `cargo clean`, because that write does not move the file's mtime and cargo
   would otherwise call the crate fresh (see "What was confirmed"), then
   `cargo build --release --target wasm32-wasip1 --message-format=json` typed
   into session 0 one code point at a time,
3. a wait for the shell's `<cwd> $ ` prompt to come back — the only completion
   signal there is,
4. `download /target/wasm32-wasip1/release/<crate>.wasm`, which streams the
   file back out through the host bridge as chunks.

The other structural surprise is the worker pair. The WASI *farm* services
calls for every thread of the guest and those threads block on `Atomics.wait`
until it does, so the farm cannot share a thread with the guest: `worker.js`
is the farm and the protocol, and it spawns `vfs-runner` for the guest, which
in turn spawns thread workers.

## What was confirmed

Run for real against `dist/`, by `scripts/run-probe.mjs` (which serves
`src/probe.html` with the sandbox's own
`Cross-Origin-Embedder-Policy: credentialless` and drives it in headless
chromium):

```bash
node scripts/run-probe.mjs
```

Every line below is from a run on 2026-09-03 against `dist/807ace9e`
(rubrc `807ace9e`), chromium 146 headless, on a 24-core linux box.

| | |
| --- | --- |
| `ready` (cold: nothing cached) | **7 085 ms** (7.1-7.5 s over three runs) |
| `ready` (warm: component in IndexedDB) | **6 834 ms** |
| `compile` of the `hello` template, release, `wasm32-wasip1` | **37 863 ms** (cargo's own figure: 37.57 s) |
| artifact | **88 892 bytes**, instantiates |
| `compile` of the same crate with a syntax error | 5 663 ms |
| total download to first `ready` | **75.1 MB** (13 files: 55.4 MB of component parts, 18.9 MB sysroot, 0.8 MB JS) |
| largest single file | **25 165 824 bytes** — `vfs.core-*.wasm.br.part-001`, exactly the cap |

1. **The worker starts from a same-origin module URL, and its subordinate
   workers resolve theirs after bundling.** `new Worker('./807ace9e/worker.js',
   { type: 'module' })` starts, spawns `vfs-runner`, and that spawns eight
   `thread_spawn` workers plus the background worker, all from hashed URLs
   vite emitted. Confirmed.
2. **`crossOriginIsolated === true` in the page and in a worker, under
   `Cross-Origin-Embedder-Policy: credentialless`** — the value the sandbox
   deploys, not rubrc's `require-corp`. `SharedArrayBuffer` is available in
   the worker. Confirmed.
3. **Machine-readable diagnostics, no regex needed.** `cargo build
   --message-format=json` works through the shell: the transcript carries
   `{"reason":"compiler-message",…}` and `{"reason":"build-finished",…}` lines,
   and the deliberate error came back as
   `{ file: "src/lib.rs", line: 46, column: 18, severity: "error", message: "expected `;`, found `value`" }`.
   The regex fallback in `worker-entry.ts` stays for the case where a build
   dies before cargo emits JSON (a malformed `Cargo.toml`), but it was not
   needed here. Confirmed.
4. **The release build of the std-only guest is 88 892 bytes and
   instantiates**, exporting exactly the wafer ABI: `memory`,
   `__wafer_alloc`, `__wafer_host_codec`, `__wafer_info`, `__wafer_handle`,
   `__wafer_lifecycle`. Under the 200 KB the design assumed and well under
   the sandbox's 4 MiB limit. Confirmed.
5. **Sizes.** Largest file 25 165 824 bytes (a part, at the cap); total
   75.1 MB, of which 55.4 MB is the component's three brotli parts (365.3 MiB
   of wasm compressed to 52.8 MiB), 18.9 MB the vendored sysroot and 0.8 MB
   the JS. Confirmed.
6. **Times.** Cold 7.1 s, warm 6.8 s — within noise of each other. The warm start is not faster: on
   localhost the download is not the cost — instantiating a 365 MiB module
   and streaming the sysroot tarball into the VFS is, and the IndexedDB path
   only skips the fetch and `compileStreaming`. Over a real network the gap
   will open up; on a fast local one it does not. Confirmed, and worth
   knowing before promising users a fast second visit.

Two things this run also settled, neither of them predicted:

* **Cargo's freshness check cannot be trusted here.** The VFS's write-file
  event replaces a file's contents without moving its mtime, so the second
  compile of an edited crate came back `"fresh": true` with the *first*
  build's artifact — a green build of code nobody wrote. `worker-entry.ts`
  runs `cargo clean` before every build for that reason. It costs nothing:
  a block has no dependencies, so there is no warm dependency graph to lose.
* **Vite rewrites `new URL(`./x/${v}`, import.meta.url)` into a build-time
  glob lookup.** Ours resolved to `undefined` because `sysroot/` does not
  exist until the build vendors it. `worker-entry.ts` reads `import.meta.url`
  through a variable to keep that resolution at runtime; do not "simplify"
  it back.


## Updating the pin

Change `PIN.json`, run `build-compiler.sh`, run the probe, and update the
numbers above. `dist/<version>/` is keyed on the rubrc sha, so an old bundle
keeps working while the new one is built; nothing is served from a version the
manifest does not name.
