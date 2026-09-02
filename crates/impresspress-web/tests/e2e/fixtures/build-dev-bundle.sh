#!/usr/bin/env bash
#
# Build the dev-sandbox e2e bundle and print the directory to serve.
#
# The bundle is produced through the **sealed** web flow, which is the only
# flow that lets the `browser-devtools` wasm be substituted without a second
# consumer crate: `sealed_web::build` resolves the wasm, the JS glue and the
# inline-JS snippets through `IMPRESSPRESS_WEB_PKG_DIR` (see
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

# Note: `crates/impresspress/build.rs` `include_bytes!`s `pkg/` and so the CLI
# cannot be COMPILED without it — but this script does not compile the CLI, it
# runs one that is already on PATH, and what gets served comes from `pkg-dev/`
# through the override below. `pkg/` is deliberately not required here: the
# embed x web flow content-hashes it in place and removes the unhashed pair,
# so a tree that has just built the ordinary bundle would fail a check that
# has nothing to do with this script's job.

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
  # The whole wasm-pack output, not two files out of three: the JS glue
  # imports from `snippets/`, and a dist missing that tree cannot load its own
  # module.
  IMPRESSPRESS_WEB_PKG_DIR="$WEB/pkg-dev" \
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
# The glue's first statement is an import from here. A dist without it loads no
# module at all, and the failure mode — a service worker that self-destructs and
# leaves the boot shell up — is a 60-second Playwright timeout rather than an
# error, so it is worth one line here.
[ -d "$DIST/snippets" ] || {
  echo "build-dev-bundle.sh: $DIST/snippets is missing; the JS glue cannot resolve its imports" >&2
  exit 1
}

# 4. The seed bundle, served as static files under the `/seed/` prefix the
#    service worker bypasses when the sandbox is on.
rm -rf "$DIST/seed"
cp -r "$FIXTURE/seed" "$DIST/seed"

log "dist ready"
echo "$DIST"
