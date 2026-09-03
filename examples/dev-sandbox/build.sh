#!/usr/bin/env bash
#
# Build the dev-sandbox bundle and print the directory to serve.
#
# This is the one bundle recipe dev.impresspress.org, CI's `e2e-dev-sandbox`
# job and the local e2e run all share (Plan 1 shipped a scratch copy under
# `crates/impresspress-web/tests/e2e/fixtures/`; this replaced it — see
# `crates/impresspress-web/tests/e2e/dev-foundations.spec.ts`, which now reads
# `seed/manifest.json` from this directory instead of pinning a hash).
#
# Two halves have to agree for the sandbox to exist at all (design §13):
#   * the wasm must be COMPILED with `--features browser-devtools`, and
#   * the bundle must be BUILT with `[dev] enabled = true` (see
#     `impresspress.toml`), which is what renders `const DEV_ENABLED = true;`
#     into `sw.js` and puts `/seed/` on the service worker's bypass list.
# Building either half without the other is a bundle with no `/b/dev`, so both
# are done here rather than left to the caller.
#
# Usage:
#   examples/dev-sandbox/build.sh            # build dist/
#   examples/dev-sandbox/build.sh --check     # verify what is already built
#
# `IMPRESSPRESS=/path/to/impresspress` overrides which CLI binary assembles
# the bundle. Default is whatever is on `PATH`, which is the trap this
# override exists for: a stale `~/.cargo/bin/impresspress` from an older
# checkout silently builds a bundle without the recursive-directory overlay
# (`cli/helpers/overlays.rs`), so `dist/seed/` never appears and the sanity
# check below fails with no hint that the CLI is the problem. Build a current
# one with `cargo install --path crates/impresspress --locked` (add
# `--root ./out` to keep it out of `~/.cargo/bin`) and point this at it.
#
# `--check` verifies every `seed/site/**` file against the hash and size
# `seed/manifest.json` declares for it — and, when `compiler/dist/` has been
# built, that its files match `compiler/dist/manifest.json` and none of them
# is over Cloudflare's asset limit — exiting non-zero on drift, WITHOUT
# building anything — this is what CI runs to catch a seed file edited
# without regenerating the manifest. A plain build runs both of those checks
# too — the seed's before it builds anything, so a stale manifest fails fast
# rather than shipping a bundle `seed::import` will refuse at runtime, and the
# compiler's once the toolchain is in place, over whatever `dist/` the build
# is about to overlay.
#
# The whole wasm-pack output is bundled, not just the wasm + JS pair: the JS
# glue imports from `snippets/`, and a pkg dir missing that tree cannot load
# its own module (`IMPRESSPRESS_WEB_PKG_DIR`, resolved by
# `crates/impresspress/src/cli/helpers/wasm.rs`).
#
# The last line of stdout is the absolute `dist/` path; CI captures it with
# `tail -1`.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"

log() { printf '==> %s\n' "$*" >&2; }

# Resolved at this point, while the caller's cwd is still current: the build
# below runs
# from `$HERE`, so a relative override (`IMPRESSPRESS=./out/bin/impresspress`,
# typed from the repo root) would otherwise be looked up against
# `examples/dev-sandbox/` and not be found. `|| true` because `set -e` is on
# and "not found" is reported by the check further down, with instructions.
IMPRESSPRESS_BIN="$(command -v "${IMPRESSPRESS:-impresspress}" 2>/dev/null || true)"
case "$IMPRESSPRESS_BIN" in
  /* | '') ;;
  *) IMPRESSPRESS_BIN="$(cd "$(dirname "$IMPRESSPRESS_BIN")" && pwd)/$(basename "$IMPRESSPRESS_BIN")" ;;
esac

# The seed fixture's manifest declares a hash and a size for every site file,
# and `seed::import` refuses the whole bundle if either disagrees with the
# bytes it actually fetches. Checking it here turns "the seed silently did
# not import" into a build failure that names the file.
check_seed() {
  log "verifying seed/site/** against seed/manifest.json"
  python3 - "$HERE" <<'PY'
import hashlib, json, sys, pathlib

root = pathlib.Path(sys.argv[1], "seed")
manifest = json.loads((root / "manifest.json").read_text())
declared = {entry["path"] for entry in manifest["site"]}
for entry in manifest["site"]:
    path = root / "site" / entry["path"]
    if not path.is_file():
        raise SystemExit(f"{path}: seed/manifest.json declares it but the file is missing")
    data = path.read_bytes()
    actual = hashlib.sha256(data).hexdigest()
    if actual != entry["sha256"] or len(data) != entry["size"]:
        raise SystemExit(
            f"{path}: is {len(data)} bytes / {actual}, but seed/manifest.json "
            f"declares {entry['size']} bytes / {entry['sha256']}"
        )

# The reverse direction: seed::import only ever imports what the manifest
# lists, so a file added under seed/site/ and never added to
# seed/manifest.json passes the forward check above and then silently never
# ships — the fresh origin that seeds from this bundle just never gets it.
# Walk the actual files and fail on anything the manifest does not declare.
site_dir = root / "site"
actual_files = {p.relative_to(site_dir).as_posix() for p in site_dir.rglob("*") if p.is_file()}
undeclared = actual_files - declared
if undeclared:
    raise SystemExit(
        "seed/site/** has file(s) seed/manifest.json does not declare, so they would "
        "never be imported: " + ", ".join(sorted(undeclared))
    )

print("seed/manifest.json matches seed/site/**", file=sys.stderr)
PY
}

# The browser toolchain (`compiler/`) is 365 MiB of composed wasm and takes
# ~55 minutes to build from cold — and its `wasm-opt` pass peaked at 12.6 GB
# RSS when it was measured, so it CANNOT run on a 7 GB CI runner: cache
# `compiler/dist/` on `compiler/PIN.json` (which fully determines it) or use a
# large runner. It is therefore built only when it is missing or when
# `PIN.json` has moved since the tree in `compiler/dist/` was produced.
# Everything about that tree — the rubrc commit, the sysroot, the tools —
# comes from that one file, so comparing it against the built manifest is the
# whole staleness test.
compiler_is_current() {
  python3 - "$HERE" <<'COMPILERPIN'
import json, pathlib, sys

compiler = pathlib.Path(sys.argv[1], "compiler")
manifest = compiler / "dist" / "manifest.json"
if not manifest.is_file():
    raise SystemExit(1)
pin = json.loads((compiler / "PIN.json").read_text())
built = json.loads(manifest.read_text())
if built.get("version") != pin["version"]:
    raise SystemExit(1)
if built.get("rubrc", {}).get("sha") != pin["rubrc"]["sha"]:
    raise SystemExit(1)
COMPILERPIN
}

# The compiler is only checked when it has been built: a tree that has never
# run `build-compiler.sh` is a normal state for anyone working on the seed or
# the wasm, and `--check` is meant to be cheap.
#
# The environment reaches the verifier untouched, which is how
# `IMPRESSPRESS_COMPILER_ALLOW_FAST=1` gets through: a CI job that only wants
# to know whether the compiler still works may build it with
# `build-compiler.sh --fast` and set that variable. The deploy workflow must
# never set it — a `--fast` component skips `wasm-opt` entirely.
check_compiler() {
  if [ ! -d "$HERE/compiler/dist" ]; then
    log "compiler/dist is not built — nothing to check"
    return 0
  fi
  log "verifying compiler/dist against its manifest and the 24 MiB asset limit"
  node "$HERE/compiler/scripts/verify-compiler-assets.mjs"
}

if [ "${1:-}" = "--check" ]; then
  check_seed
  check_compiler
  exit 0
fi

check_seed

# 0. The browser toolchain, overlaid onto the bundle at
#    `/__impresspress_dev/compiler/` (see `impresspress.toml`).
if compiler_is_current; then
  log "compiler/dist is current for compiler/PIN.json"
else
  log "compiler/build-compiler.sh (dist is missing or built from another pin)"
  "$HERE/compiler/build-compiler.sh"
fi
# `compiler_is_current` answers one question — was this tree built from the
# pin in the file? — off two manifest fields, and never looks at the bytes
# beside it or at `manifest.build`. A `--fast` component is therefore
# "current" forever: `IMPRESSPRESS_COMPILER_ALLOW_FAST=1` is needed for the
# run of `build-compiler.sh` that PRODUCES one, and not for any later build
# that picks it up. So the verifier runs here as well, over whatever `dist/`
# is about to be overlaid — without it a plain build would quietly assemble an
# unoptimized toolchain, and an edited or truncated file under `dist/` would
# ship unhashed. This is the check every other file in this directory says is
# what keeps a `--fast` tree out of a deploy.
check_compiler

# 1. The feature-on wasm. `--out-dir pkg-dev` keeps it away from `pkg/`, which
#    is the ordinary (feature-off) bundle every other consumer serves — a
#    tree that has just built the ordinary bundle must not be disturbed by
#    this script, and vice versa.
log "wasm-pack build --features browser-devtools -> $REPO/crates/impresspress-web/pkg-dev"
(cd "$REPO/crates/impresspress-web" && wasm-pack build --target web --release --out-dir pkg-dev -- --features browser-devtools)

# 2. The sealed × web flow. `examples/dev-sandbox` has an `impresspress.toml`
#    but no `Cargo.toml`, so the CLI's mode detection (`mode.rs`) takes the
#    sealed path — which honours `IMPRESSPRESS_WEB_PKG_DIR` rather than
#    rebuilding impresspress-web itself.
cd "$HERE"
[ -n "$IMPRESSPRESS_BIN" ] || {
  echo "build.sh: no impresspress CLI on PATH (or at \$IMPRESSPRESS='${IMPRESSPRESS:-}')." >&2
  echo "  cargo install --path '$REPO/crates/impresspress' --locked --root '$REPO/out'" >&2
  echo "  IMPRESSPRESS='$REPO/out/bin/impresspress' examples/dev-sandbox/build.sh" >&2
  exit 1
}
log "assembling the bundle with $IMPRESSPRESS_BIN"
IMPRESSPRESS_WEB_PKG_DIR="$REPO/crates/impresspress-web/pkg-dev" \
  "$IMPRESSPRESS_BIN" build --target web --release

DIST="$HERE/dist"
[ -f "$DIST/sw.js" ] || { echo "build.sh: $DIST/sw.js was not produced" >&2; exit 1; }
# The one build-time constant `sw.js` renders `[dev] enabled` into
# (`impresspress-bundle`'s `sw.js.tmpl`): `initialize({ dev: DEV_ENABLED })`
# and the isolation-header passthrough both read it, so this single line is
# the whole of "the sandbox is on in this bundle".
grep -q 'const DEV_ENABLED = true;' "$DIST/sw.js" || {
  echo "build.sh: $DIST/sw.js does not declare 'const DEV_ENABLED = true;' — is [dev] enabled set?" >&2
  exit 1
}
[ -d "$DIST/snippets" ] || {
  echo "build.sh: $DIST/snippets is missing; the JS glue cannot resolve its imports" >&2
  exit 1
}
[ -f "$DIST/seed/manifest.json" ] || {
  echo "build.sh: $DIST/seed/manifest.json was not overlaid — check impresspress.toml's [[assets.overlay]]," >&2
  echo "  or an impresspress CLI older than the recursive-directory overlay (cli/helpers/overlays.rs):" >&2
  echo "  $IMPRESSPRESS_BIN" >&2
  exit 1
}
[ -f "$DIST/__impresspress_dev/compiler/manifest.json" ] || {
  echo "build.sh: $DIST/__impresspress_dev/compiler/manifest.json was not overlaid — check impresspress.toml's [[assets.overlay]]" >&2
  exit 1
}

log "dist ready: $(du -sh "$DIST" | cut -f1)"
echo "$DIST"
