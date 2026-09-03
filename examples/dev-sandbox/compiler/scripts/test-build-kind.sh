#!/usr/bin/env bash
#
# `compose_decision` against a fake `.rubrc` tree.
#
# The decision it makes is what keeps `dist/manifest.json`'s `build` field
# honest: phase 3 of `build-compiler.sh` is skipped when the component is
# already there, so without a record beside it a `--fast` component composed
# yesterday is reported as `full` by a plain run today, and the verifier —
# the only thing standing between `--fast` and a deploy — accepts it.
#
# Composing for real costs ~35 minutes and 12.6 GB of RSS, so this drives the
# decision over a directory holding a zero-byte stand-in for the component.
# That is the whole of what the function reads.
#
# Usage: scripts/test-build-kind.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source-path=SCRIPTDIR source=compose-decision.sh
. "$HERE/compose-decision.sh"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

failures=0

# One case: a bindings directory built from `component`/`recorded`, the kind
# asked for, and the decision that must come out.
check() {
  local what="$1" component="$2" recorded="$3" want="$4" expected="$5"
  local dir="$TMP/case"
  rm -rf "$dir"
  mkdir -p "$dir"
  [ "$component" = no ] || : > "$dir/vfs.core.wasm"
  [ "$recorded" = none ] || printf '%s\n' "$recorded" > "$dir/.build-kind"

  local got
  got="$(compose_decision "$dir" "$want")"
  if [ "$got" = "$expected" ]; then
    printf 'ok   %s\n' "$what"
  else
    printf 'FAIL %s: expected %s, got %s\n' "$what" "$expected" "$got" >&2
    failures=$((failures + 1))
  fi
}

#     what                                     component  recorded   want  expected
check "nothing on disk, full asked for"        no         none       full  compose
check "nothing on disk, --fast asked for"      no         none       fast  compose
check "a full component, full asked for"       yes        full       full  reuse
check "a full component, --fast asked for"     yes        full       fast  reuse
check "a fast component, --fast asked for"     yes        fast       fast  reuse
# The one this file exists for: without the record, this arm returns `reuse`
# and the manifest goes out saying `full` over a component nobody optimized.
check "a fast component, full asked for"       yes        fast       full  recompose
check "an unrecorded component, full asked"    yes        none       full  refuse
check "an unrecorded component, --fast asked"  yes        none       fast  reuse
# A record with no component is not a component: the file survives a manual
# `rm` of the wasm, and answering `reuse` there would skip the composition
# and then fail in the vite build with no explanation.
check "a record with no component"             no         full       full  compose

if [ "$failures" -ne 0 ]; then
  printf '\ntest-build-kind.sh: %s failure(s)\n' "$failures" >&2
  exit 1
fi
printf '\ntest-build-kind.sh: all cases pass\n'
