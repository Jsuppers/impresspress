#!/usr/bin/env bash
#
# Build the dev-sandbox e2e bundle and print the directory to serve.
#
# The bundle is produced through the **sealed** web flow, which is the only
# flow that lets the `browser-devtools` wasm be substituted without a second
# consumer crate: `sealed_web::build` resolves the wasm and the JS glue through
# `IMPRESSPRESS_WEB_WASM` / `IMPRESSPRESS_WEB_JS` (see
# `crates/impresspress/src/cli/helpers/wasm.rs`), so a feature-on `wasm-pack`
# output can be bundled by the same CLI the ordinary flow uses.
#
# Two halves have to agree for the sandbox to exist at all (design §13):
#   * the wasm must be COMPILED with `--features browser-devtools`, and
#   * the bundle must be BUILT with `[dev] enabled = true`, which is what puts
#     `{ dev: true }` into `sw.js` and `/seed/` on the service worker's bypass
#     list.
# Building either half without the other is a bundle with no `/b/dev`, so both
# are done here rather than left to the caller.
#
# Usage:
#   crates/impresspress-web/tests/e2e/fixtures/build-dev-bundle.sh
#
# Environment:
#   DEV_BUNDLE_DIR  Where the throwaway sealed app is assembled.
#                   Default: ${TMPDIR:-/tmp}/impresspress-dev-bundle
#                   It MUST NOT sit inside a Cargo package: the CLI walks up
#                   for a `Cargo.toml` and would switch to the embed flow,
#                   which rebuilds the wasm itself and ignores the overrides.
#   IMPRESSPRESS    The CLI to run. Default: `impresspress` from PATH.
#
# Idempotent: re-running rebuilds the wasm (cargo decides what that costs) and
# recreates `dist/` from scratch. The last line of stdout is the dist path.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../../.." && pwd)"
WEB="$REPO/crates/impresspress-web"
FIXTURE="$WEB/tests/e2e/fixtures/dev-sandbox-seed"
BUNDLE_DIR="${DEV_BUNDLE_DIR:-${TMPDIR:-/tmp}/impresspress-dev-bundle}"
IMPRESSPRESS="${IMPRESSPRESS:-impresspress}"

log() { printf '==> %s\n' "$*" >&2; }

if [ ! -f "$REPO/Cargo.toml" ] || [ ! -d "$WEB" ]; then
  echo "build-dev-bundle.sh: $REPO does not look like the impresspress workspace" >&2
  exit 1
fi

command -v "$IMPRESSPRESS" >/dev/null 2>&1 || {
  cat >&2 <<EOF
build-dev-bundle.sh: no \`$IMPRESSPRESS\` on PATH.

Install it first (the same way CI's e2e-build job does):
  cargo install --path crates/impresspress --locked --debug --root out
  export PATH="\$PWD/out/bin:\$PATH"
EOF
  exit 1
}

# The CLI's build.rs `include_bytes!`s the ordinary pkg/ build. It is not what
# gets served — the env overrides below replace it — but the crate does not
# compile without it, so say so here rather than in a build script backtrace.
if [ ! -f "$WEB/pkg/impresspress_web.js" ]; then
  echo "build-dev-bundle.sh: $WEB/pkg is missing; the impresspress CLI cannot be built without it" >&2
  exit 1
fi

# 1. The feature-on wasm. `--out-dir pkg-dev` keeps it away from `pkg/`, which
#    is the ordinary (feature-off) bundle every other e2e serves.
log "wasm-pack build --features browser-devtools -> $WEB/pkg-dev"
(cd "$WEB" && wasm-pack build --target web --release --out-dir pkg-dev -- --features browser-devtools)

# 2. The seed fixture's manifest declares a hash, a size and a content type,
#    and `seed::import` refuses the bundle if any of them disagrees with the
#    bytes. Checking it here turns "the seed silently did not import" into a
#    build failure that names the file.
log "verifying the seed fixture against its manifest"
python3 - "$FIXTURE" <<'PY'
import hashlib, json, sys, pathlib

root = pathlib.Path(sys.argv[1], "seed")
manifest = json.loads((root / "manifest.json").read_text())
for entry in manifest["site"]:
    path = root / "site" / entry["path"]
    data = path.read_bytes()
    actual = hashlib.sha256(data).hexdigest()
    if actual != entry["sha256"] or len(data) != entry["size"]:
        raise SystemExit(
            f"{path}: is {len(data)} bytes / {actual}, but seed/manifest.json "
            f"declares {entry['size']} bytes / {entry['sha256']}"
        )
PY

# 3. A throwaway sealed app whose only content is the flag. No Cargo.toml, so
#    the CLI takes the sealed path and honours the wasm/JS overrides.
log "assembling the sealed app in $BUNDLE_DIR"
rm -rf "$BUNDLE_DIR"
mkdir -p "$BUNDLE_DIR"
cat > "$BUNDLE_DIR/impresspress.toml" <<'TOML'
[app]
name = "dev-sandbox-e2e"
title = "Dev sandbox e2e"
boot_redirect = "/"

[dev]
enabled = true
TOML

(
  cd "$BUNDLE_DIR"
  IMPRESSPRESS_WEB_WASM="$WEB/pkg-dev/impresspress_web_bg.wasm" \
  IMPRESSPRESS_WEB_JS="$WEB/pkg-dev/impresspress_web.js" \
    "$IMPRESSPRESS" build --target web --release
)

DIST="$BUNDLE_DIR/dist"
[ -f "$DIST/sw.js" ] || { echo "build-dev-bundle.sh: $DIST/sw.js was not produced" >&2; exit 1; }
# The one line that proves both halves lined up: `[dev] enabled` is what turns
# `__DEV_ENABLED__` into `true` in the service worker's `initialize()` call.
grep -q 'dev: true' "$DIST/sw.js" || {
  echo "build-dev-bundle.sh: $DIST/sw.js does not boot with { dev: true } — is [dev] enabled set?" >&2
  exit 1
}

# 4. The rest of the wasm-pack output.
#
#    KNOWN GAP IN THE SEALED WEB FLOW, staged here rather than papered over:
#    `crates/impresspress/src/cli/flows/sealed_web.rs` writes exactly two files
#    out of a `wasm-pack --target web` build — `impresspress_web_bg.wasm` and
#    `impresspress_web.js`. That output is three things, not two: the glue's
#    first statement is
#      import { … } from './snippets/impresspress-browser-<hash>/js/bridge.js';
#    (`impresspress-browser`'s `#[wasm_bindgen(module = "/js/bridge.js")]`
#    inline JS), and without `snippets/` the module fails to load, `sw.js`
#    self-destructs, and every sealed × web bundle serves the boot shell
#    forever. The embed × web flow does not hit this because it bundles pkg/
#    in place, where wasm-pack already wrote the directory.
#
#    The root-cause fix is in `sealed_web.rs` (and in the `include_bytes!` pair
#    `crates/impresspress/build.rs` bakes, which today has no snippets to
#    resolve at all). Until it lands, the e2e cannot boot the bundle it builds,
#    so the directory is staged here — visibly, and only here.
rm -rf "$DIST/snippets"
cp -r "$WEB/pkg-dev/snippets" "$DIST/snippets"

# 5. The seed bundle, served as static files under the `/seed/` prefix the
#    service worker bypasses when the sandbox is on.
rm -rf "$DIST/seed"
cp -r "$FIXTURE/seed" "$DIST/seed"

log "dist ready"
echo "$DIST"
