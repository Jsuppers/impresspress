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
#     `impresspress.toml`), which is what puts `{ dev: true }` into `sw.js`
#     and `/seed/` on the service worker's bypass list.
# Building either half without the other is a bundle with no `/b/dev`, so both
# are done here rather than left to the caller.
#
# Usage:
#   examples/dev-sandbox/build.sh            # build dist/
#   examples/dev-sandbox/build.sh --check     # verify seed/manifest.json only
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
# `seed/manifest.json` declares for it and exits non-zero on drift, WITHOUT
# building anything — this is what CI runs to catch a seed file edited
# without regenerating the manifest. A plain build runs the same check first,
# so a stale manifest fails fast rather than shipping a bundle
# `seed::import` will refuse at runtime.
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
print("seed/manifest.json matches seed/site/**", file=sys.stderr)
PY
}

if [ "${1:-}" = "--check" ]; then
  check_seed
  exit 0
fi

check_seed

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
grep -q 'dev: true' "$DIST/sw.js" || {
  echo "build.sh: $DIST/sw.js does not boot with { dev: true } — is [dev] enabled set?" >&2
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

log "dist ready: $(du -sh "$DIST" | cut -f1)"
echo "$DIST"
