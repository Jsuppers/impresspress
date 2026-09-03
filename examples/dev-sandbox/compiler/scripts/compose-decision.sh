# shellcheck shell=bash
#
# What `build-compiler.sh`'s phase 3 must do with the component on disk.
#
# Sourced by `build-compiler.sh` and by `scripts/test-build-kind.sh`. It is
# its own file because it is the one decision in the build that has to be
# provable without composing anything: the composition costs ~35 minutes and
# 12.6 GB of RSS, so a test that exercised it for real would never run.
#
# # Why the kind is recorded beside the component
#
# Phase 3 is skipped when `vfs.core.wasm` already exists, so deriving the
# manifest's `build` field from the CURRENT invocation's `--fast` flag makes
#
#   build-compiler.sh --fast   # composes without wasm-opt
#   build-compiler.sh          # phase 3 skipped
#
# stamp `"build": "full"` on a manifest describing a component nobody
# optimized. `verify-compiler-assets.mjs` then accepts it with no allow flag
# and it is deployable — which is exactly the enforcement the `--fast` rule
# rests on. So the kind is written NEXT TO the component when it is composed
# (`.build-kind`), and read back from there.
#
# `compose_decision <bindings-dir> <wanted-kind>` prints one of:
#
#   compose     there is no component; build one
#   reuse       what is there is at least as good as what was asked for
#   recompose   a `fast` component and a full build was asked for
#   refuse      a component of unknown kind and a full build was asked for
#
# `refuse` rather than `recompose` for the unknown case: a tree composed
# before `.build-kind` existed may well be optimized, and spending 35 minutes
# and 12.6 GB to find out — silently, on someone's laptop — is worse than an
# error that says which one line to write. It cannot ship: `reuse` of an
# unrecorded component only happens on the `--fast` path, and that path's
# manifest says `fast`.

# The kind recorded beside the component, or `unrecorded`.
#
# An EMPTY `.build-kind` is the same as an absent one — a truncated write, or
# a `>` that ran before the composition died, is not a claim that wasm-opt ran
# — and it has to answer that way in both readers, or `build-compiler.sh`
# would hand `write-manifest.mjs` an empty `COMPILER_BUILD_KIND` for a
# component `compose_decision` had already called unrecorded.
recorded_build_kind() {
  local recorded
  recorded="$(cat "$1/.build-kind" 2>/dev/null || true)"
  [ -n "$recorded" ] || recorded=unrecorded
  printf '%s\n' "$recorded"
}

compose_decision() {
  local bindings="$1" want="$2" have=""

  if [ -f "$bindings/vfs.core.wasm" ]; then
    have="$(recorded_build_kind "$bindings")"
  fi

  case "$have" in
    "") echo compose ;;
    # A full component satisfies a `--fast` request too: `--fast` asks for a
    # cheaper build, not for a worse one.
    full) echo reuse ;;
    "$want") echo reuse ;;
    unrecorded)
      if [ "$want" = fast ]; then
        echo reuse
      else
        echo refuse
      fi
      ;;
    *) echo recompose ;;
  esac
}
