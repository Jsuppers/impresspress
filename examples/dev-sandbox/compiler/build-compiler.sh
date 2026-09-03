#!/usr/bin/env bash
#
# Build `dist/<version>/` — the Rubrc compiler, packaged as versioned static
# assets the dev sandbox serves from `/__impresspress_dev/compiler/`.
#
# Everything this produces is derived from `PIN.json` and nothing else: the
# rubrc commit, the `wasi_virt_layer-cli` release binary that composes its
# prebuilt toolchain modules into one component, and the wasm32-wasip1 sysroot
# tarball we vendor so the browser never fetches from a third-party host. A
# machine with node 22, rustup and curl can reproduce `dist/` from this script
# alone.
#
# Usage:
#   compiler/build-compiler.sh          # build dist/<version>/ (slow: see below)
#
# The phases, in order, each skipped when its output already exists so a
# re-run after editing `src/worker-entry.ts` costs seconds rather than half an
# hour:
#
#   1. checkout  `.rubrc/` at PIN.json's sha (~250 MB — the prebuilt rustc,
#                llvm, cargo and rust-analyzer wasm modules are checked into
#                the repo, so no LLVM build happens here).
#   2. tools     `npm ci` (vite, bun), then the two sha256-checked release
#                binaries the composition needs — `wasi_virt_layer-cli` and
#                Binaryen's `wasm-opt` — and the rustup targets.
#   3. compose   `wasi_virt_layer build` — links the four toolchain modules
#                plus rubrc's vfs/vfs-shell into a single `vfs.core.wasm`.
#                THIS IS THE SLOW ONE: `wasm-opt -Oz` over ~230 MB of input
#                takes 15-30 minutes single-core.
#   4. bundle    our vite build: `src/worker-entry.ts` plus, via aliases,
#                rubrc's `worker_process/` + `vfs_bindings/`. None of rubrc's
#                UI (Monaco, xterm, the SharedObject layer) is imported.
#   5. asset     brotli-compress `vfs.core-<hash>.wasm` and split it into
#                <= 24 MiB parts, because Cloudflare refuses a static asset
#                larger than that; vendor the sysroot tarball.
#   6. manifest  `dist/manifest.json` (sizes + sha256 of every file), then
#                `scripts/verify-compiler-assets.mjs`.
#
# Phase 3 needs a nightly toolchain: `wasi_virt_layer` builds the VFS crate
# with `-Zbuild-std=std,panic_unwind` (`--vfs-unwind`), and both toolchains
# need the `wasm32-wasip1-threads` target.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

log() { printf '==> %s\n' "$*" >&2; }
die() { printf 'build-compiler.sh: %s\n' "$*" >&2; exit 1; }

command -v node >/dev/null || die "node is required (>= 22)"
command -v git >/dev/null || die "git is required"
command -v curl >/dev/null || die "curl is required"
command -v rustup >/dev/null || die "rustup is required"
command -v tar >/dev/null || die "tar is required"

NODE_MAJOR="$(node -p 'process.versions.node.split(".")[0]')"
[ "$NODE_MAJOR" -ge 22 ] || die "node >= 22 is required, found $(node --version)"

pin() { node -p "JSON.stringify(require('$HERE/PIN.json')$1)" | sed 's/^"//; s/"$//'; }

VERSION="$(pin ".version")"
RUBRC_REPO="$(pin ".rubrc.repo")"
RUBRC_SHA="$(pin ".rubrc.sha")"
BINARYEN_URL="$(pin ".binaryen.url")"
BINARYEN_SHA256="$(pin ".binaryen.sha256")"
WVL_URL="$(pin ".wasi_virt_layer_cli.url")"
WVL_SHA256="$(pin ".wasi_virt_layer_cli.sha256")"
SYSROOT_URL="$(pin ".sysroot.url")"
SYSROOT_SHA256="$(pin ".sysroot.sha256")"
SYSROOT_TRIPLE="$(pin ".sysroot.triple")"
PART_BYTES="$(pin ".part_bytes")"

RUBRC="$HERE/.rubrc"
CACHE="$HERE/.cache"
DIST="$HERE/dist"
OUT="$DIST/$VERSION"

mkdir -p "$CACHE"

# `sha256sum` is coreutils-only; node is already a hard dependency here.
sha256_of() {
  node -e '
    const { createHash } = require("node:crypto");
    const fs = require("node:fs");
    const h = createHash("sha256");
    const s = fs.createReadStream(process.argv[1]);
    s.on("data", (c) => h.update(c));
    s.on("end", () => process.stdout.write(h.digest("hex")));
  ' "$1"
}

fetch_verified() {
  local url="$1" dest="$2" want="$3"
  if [ -f "$dest" ] && [ "$(sha256_of "$dest")" = "$want" ]; then
    return 0
  fi
  log "downloading $url"
  curl -fsSL --retry 3 -o "$dest.tmp" "$url"
  local got
  got="$(sha256_of "$dest.tmp")"
  [ "$got" = "$want" ] || die "$url: sha256 is $got, PIN.json says $want"
  mv "$dest.tmp" "$dest"
}

# ---------------------------------------------------------------- 1. checkout

if [ "$(git -C "$RUBRC" rev-parse HEAD 2>/dev/null || true)" != "$RUBRC_SHA" ]; then
  log "cloning $RUBRC_REPO at $RUBRC_SHA into .rubrc (~250 MB)"
  rm -rf "$RUBRC"
  mkdir -p "$RUBRC"
  git -C "$RUBRC" init -q
  git -C "$RUBRC" remote add origin "$RUBRC_REPO"
  git -C "$RUBRC" fetch -q --depth 1 origin "$RUBRC_SHA"
  git -C "$RUBRC" checkout -q FETCH_HEAD
else
  log "checkout .rubrc already at $RUBRC_SHA"
fi

# ------------------------------------------------------------------- 2. tools

if [ ! -d "$HERE/node_modules" ]; then
  log "npm ci"
  (cd "$HERE" && npm ci --no-audit --no-fund)
fi

BUN="$HERE/node_modules/.bin/bun"
[ -x "$BUN" ] || die "node_modules/.bin/bun is missing — did npm ci run?"

# Binaryen's own release build, not npm's `binaryen` package: that one is a
# JS/wasm port of wasm-opt, and -Oz over a 94 MB module runs into both its
# single-threaded slowness and node's heap ceiling.
BINARYEN_DIR="$CACHE/binaryen"
if [ ! -x "$BINARYEN_DIR/bin/wasm-opt" ]; then
  fetch_verified "$BINARYEN_URL" "$CACHE/binaryen.tar.gz" "$BINARYEN_SHA256"
  rm -rf "$BINARYEN_DIR"
  mkdir -p "$BINARYEN_DIR"
  tar -xzf "$CACHE/binaryen.tar.gz" -C "$BINARYEN_DIR" --strip-components=1
  [ -x "$BINARYEN_DIR/bin/wasm-opt" ] || die "the Binaryen tarball did not contain bin/wasm-opt"
fi
WASM_OPT_DIR="$BINARYEN_DIR/bin"

WVL_DIR="$CACHE/wasi_virt_layer-cli"
if [ ! -x "$WVL_DIR/wasi_virt_layer" ]; then
  fetch_verified "$WVL_URL" "$CACHE/wasi_virt_layer-cli.tar.xz" "$WVL_SHA256"
  rm -rf "$WVL_DIR"
  mkdir -p "$WVL_DIR"
  # cargo-dist tarballs carry one top-level directory; flatten it.
  tar -xJf "$CACHE/wasi_virt_layer-cli.tar.xz" -C "$WVL_DIR" --strip-components=1
  [ -x "$WVL_DIR/wasi_virt_layer" ] || die "the wasi_virt_layer-cli tarball did not contain the binary"
fi

log "rustup: wasm32-wasip1-threads on the default and the nightly toolchain"
rustup target add wasm32-wasip1-threads >/dev/null
if ! rustup toolchain list | grep -q '^nightly-'; then
  log "installing the nightly toolchain (rust-src + wasm32-wasip1-threads)"
  rustup toolchain install nightly --profile minimal \
    -c rust-src -t wasm32-wasip1-threads >/dev/null
else
  rustup component add --toolchain nightly rust-src >/dev/null
  rustup target add --toolchain nightly wasm32-wasip1-threads >/dev/null
fi

# ----------------------------------------------------------------- 3. compose

BINDINGS="$RUBRC/page/src/worker_process/vfs_bindings"

if [ ! -f "$BINDINGS/vfs.core.wasm" ]; then
  log "bun install in .rubrc"
  (cd "$RUBRC" && PATH="$HERE/node_modules/.bin:$PATH" "$BUN" install --frozen-lockfile)

  # `bun run vfs:build:prod` is rubrc's own recipe: compose, copy the bindings
  # next to the page sources, then install the bindings' own dependencies.
  # `wasm-opt` has to be OURS and not the one wasi_virt_layer vendors — it
  # passes `--enable-shared-everything`, which Binaryen <= 116 rejects.
  log "composing vfs.core.wasm (15-30 minutes: wasm-opt over ~230 MB)"
  (cd "$RUBRC" && PATH="$WASM_OPT_DIR:$WVL_DIR:$HERE/node_modules/.bin:$PATH" \
    "$BUN" run vfs:build:prod)

  [ -f "$BINDINGS/vfs.core.wasm" ] || die "the composition did not produce $BINDINGS/vfs.core.wasm"
else
  log "compose vfs.core.wasm already built ($(du -h "$BINDINGS/vfs.core.wasm" | cut -f1))"
fi

# ------------------------------------------------------------------ 4. bundle

log "vite build -> dist/$VERSION"
rm -rf "$OUT"
(cd "$HERE" && COMPILER_VERSION="$VERSION" npx --no-install vite build)

[ -f "$OUT/worker.js" ] || die "the vite build did not produce dist/$VERSION/worker.js"

# ------------------------------------------------------------------- 5. asset

log "brotli + split vfs.core-*.wasm into <= $PART_BYTES byte parts"
VFS_PART_BYTES="$PART_BYTES" node "$HERE/scripts/prepare-vfs-asset.mjs" "$OUT"

log "vendoring the $SYSROOT_TRIPLE sysroot"
fetch_verified "$SYSROOT_URL" "$CACHE/$SYSROOT_TRIPLE.tar.br" "$SYSROOT_SHA256"
mkdir -p "$OUT/sysroot"
cp "$CACHE/$SYSROOT_TRIPLE.tar.br" "$OUT/sysroot/$SYSROOT_TRIPLE.tar.br"

# ---------------------------------------------------------------- 6. manifest

node "$HERE/scripts/write-manifest.mjs"
node "$HERE/scripts/verify-compiler-assets.mjs"

log "dist/$VERSION ready: $(du -sh "$OUT" | cut -f1)"
echo "$OUT"
