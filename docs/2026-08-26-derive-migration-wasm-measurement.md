# Derive migration: measured Cloudflare wasm cost

**Date:** 2026-08-28 (Plan 2, Task 6)
**Replaces:** the spec's synthetic estimate of `+94 KB raw / +21 KB gzip`

## Result

| Feature set | Baseline raw | Post raw | Δ raw | Baseline gz | Post gz | Δ gz |
|---|---:|---:|---:|---:|---:|---:|
| lean (`target-cloudflare`) | 4,159,797 | 4,223,990 | **+64,193 (+1.54 %)** | 1,458,363 | 1,492,646 | **+34,283 (+2.35 %)** |
| full (`target-cloudflare-full`) | 6,471,053 | 6,627,634 | **+156,581 (+2.42 %)** | 2,198,606 | 2,265,628 | **+67,022 (+3.05 %)** |

Bytes of `build/index_bg.wasm` after `worker-build`'s own `wasm-opt` pass; gz is `gzip -9`.

**The estimate did not hold on gzip in either configuration.** Lean raw came in
under the estimate (64 KB vs 94 KB) but gzip over (34 KB vs 21 KB); full is
over on both, because the estimate was a single figure and the cost scales per
migrated block. The real per-block gzip cost is roughly twice what the
synthetic benchmark projected. Plausible reason, not verified: the cost is
mostly *code* — one `JsonSchema` impl per contract type, run at boot inside
`BlockInfo::endpoints` — plus doc-comment strings, and code compresses worse
than the schema JSON it emits.

## Against the budgets

- **8 MB raw warn line** (`crates/impresspress/src/cli/helpers/cloudflare/profile_check.rs:43`):
  full sits at 6.63 MB, 1.37 MB under. Lean at 4.22 MB.
- **Cloudflare compressed limits** (3 MB Free, 10 MB Paid): full is 2.27 MB gz,
  76 % of the Free limit (was 73 %); lean 1.49 MB, 50 %.
- **Startup-CPU cap** — the binding production constraint per
  `docs/2026-07-18-externalize-static-assets-benchmark.md` — is code-bound, so
  raw code growth is the proxy: +1.5 % (lean) / +2.4 % (full). The cushion
  measured there for the lean deploy was ~270 ms under the 400 ms cap; this
  does not move it materially. Not re-measured here.

**Decision: no action.** The measured cost is small in absolute terms, does not
approach any limit, and is the price of the schema guarantees the migration
exists for. `wasm-opt -Oz` (about −215 KB raw per
`docs/CODE_REVIEW_2026-07-16_FINDINGS.md`) remains available headroom; it was
deliberately *not* applied, because it would have masked this measurement and
is a separate decision.

**Not yet included:** products' 69 unmigrated sites. Products contributed 54 of
its 123 sites here; expect the remainder to cost proportionally when typed.

## What was compared

- **Baseline:** `5e7f8c8` — `origin/main`, the commit before the snapshot gate
  (`76d11e1`) that opened the migration.
- **Post:** `7d601c0` — the 22-commit migration plus its three follow-ups
  (orphan products contract types deleted, `PATCH /b/auth/api/me` declared,
  the three IAM role writes declared and typed).
- **Same wafer-run for both:** the #324 tree (`bc73bb4`, identical to the squash
  merge `61e68a0`) via a `[patch]`, so the delta is the migration alone and not
  a wafer-run change. The baseline's own pin (`46317ec`) predates the typed
  builders' serialize contract; building it against that pin would have
  measured two things at once.

## Method — reproduce it exactly

`impresspress-cloudflare` is a **library** (`pub async fn run`); it has no
`#[event(fetch)]`. Running `worker-build` inside the crate itself produces a
57 KB shell with nothing reachable — not a measurement. The deployable cdylib
lives in a consumer (wafer-site in production), so the measurement uses a
minimal consumer that mirrors wafer-site's wiring with no-op hooks, i.e. the
binary is impresspress plus nothing:

```rust
// src/lib.rs
#[cfg(feature = "target-cloudflare")]
#[worker::event(fetch)]
async fn fetch_main(
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
) -> worker::Result<worker::Response> {
    impresspress_cloudflare::run(req, env, ctx, Ok, |_wafer, _storage| Ok(())).await
}

#[cfg(feature = "target-cloudflare")]
#[worker::event(start)]
fn start() {
    impresspress_cloudflare::init_isolate();
}
```

```toml
# Cargo.toml
[package]
name = "wasm-consumer"
version = "0.1.0"
edition = "2021"
resolver = "2"

[lib]
crate-type = ["cdylib", "rlib"]

[features]
default = []
target-cloudflare = ["dep:impresspress-cloudflare", "dep:worker", "dep:wasm-bindgen"]
target-cloudflare-full = ["target-cloudflare", "impresspress-cloudflare/full"]

[dependencies]
impresspress-cloudflare = { path = "<tree>/crates/impresspress-cloudflare", optional = true, default-features = false }
worker       = { version = "0.7.5", features = ["d1"], optional = true }
wasm-bindgen = { version = "0.2", optional = true }

[profile.release]            # identical to the workspace's and wafer-site's
opt-level = "z"
lto = true
codegen-units = 1
strip = true
panic = "abort"
```

Build command — **exactly** what the deploy CLI runs
(`crates/impresspress/src/cli/helpers/cloudflare/build.rs`), from the consumer
root; note there is no `--release`/`--dev` flag (worker-build's default mode is
release):

```bash
worker-build --no-default-features --features target-cloudflare        # lean
worker-build --no-default-features --features target-cloudflare-full   # full
stat -c %s build/index_bg.wasm
gzip -9 -c build/index_bg.wasm | wc -c
```

Feature sets, because the consumer decides what it ships:

- **lean** — `impresspress-cloudflare` with its default features (`default = []`),
  which is the always-on core: admin, auth-ui, email, system, auth. This is
  what wafer-site's Cloudflare target enables (zero optional `block-*`
  features).
- **full** — `impresspress-cloudflare/full`: + files, legalpages, tickets,
  messages, products, userportal — every block the migration touched.

Toolchain: rustc/cargo 1.94.0, worker-build 0.7.5, wasm-bindgen 0.2.122,
wasm-opt 116 (worker-build's default pass, `-O`).

One trap: cargo discovers `.cargo/config.toml` from the *current directory*,
not the manifest path. A consumer placed under the impresspress tree inherits
the repo-level `[patch]`; one placed elsewhere sees the git pin. Put the
consumer where the source you intend to measure will actually be resolved, and
check `Cargo.lock` afterwards to be sure which one you got.
