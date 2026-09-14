#!/usr/bin/env bash
# Fail when a tracked file outside the documentation tree cites a
# repo-relative documentation path that does not resolve in this repo.
#
# The invariant: a repo-relative path written in a source comment must name a
# file that exists here. A document that lives in another repository must not
# be cited with a path that looks in-repo — inline the substance instead, or
# name the external source unambiguously. A pointer to nothing is worse than
# no pointer: it reads as "the answer is written down over there" when it is
# not written down anywhere.
#
# What counts as a citation: the literal directory name below, followed by a
# slash and a path, where the character before it is not alphanumeric, `/` or
# `.`. That preceding-character rule is what keeps URLs and longer paths out
# (an MDN link, a `/b/vector/api/<dir>/a` route) with no allow-list of files.
#
# A citation resolves if the path exists as written, or with `.md` appended.
# A citation that ends the line on `-` or `/` is a path wrapped across two
# comment lines: it can never be verified by grep or followed by a reader, so
# it fails with its own message. Both joiners have to count — a path broken
# after a `/` leaves a token that IS a real directory, so an existence check
# alone waves it through, which is exactly the defect this guard exists to
# catch. A directory named mid-line is untouched; only end-of-line is a wrap.
#
# The one exemption: `.sql` files under a `migrations/` directory. Not a
# convenience — it is forced by an invariant of the migration system itself.
# `crate::migration_helper`'s "A shipped .sql file is immutable, comments
# included" records it: `apply_if_blessed` hashes a migration's WHOLE text, so
# editing a `--` comment changes its hash exactly as much as editing a
# statement does, and every deployment that already applied it then logs
# `schema drift` on each boot until someone redeploys with `--run-migrations`.
# A guard that demanded that edit would be asking for something the runtime
# punishes, so it does not ask. That rule also says where the explanation goes
# instead: the block's `migrations/mod.rs`, beside the constant, where it is
# not hash-addressed — which is where the admin block's dangling citations
# are written out (`blocks/admin/migrations/mod.rs`). That `mod.rs` is NOT
# exempt: it is ordinary source, and the exemption is exactly as wide as the
# hashing.
#
# Run from anywhere in the working tree.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# The directory whose contents this guard resolves citations against. Spelled
# once, via a variable, so this script's own text contains no citation for it
# to find.
ROOT_DIR="docs"

mapfile -t candidates < <(
  git grep -I -l -e "${ROOT_DIR}/" -- ":!${ROOT_DIR}/" ":!**/migrations/*.sql" || true
)

if [ "${#candidates[@]}" -eq 0 ]; then
  echo "check-doc-pointers: no citations found."
  exit 0
fi

# Emit one `status<TAB>file<TAB>line<TAB>path` record per citation.
# status is WRAPPED for a citation broken across lines, else CHECK.
records=$(
  awk -v dir="$ROOT_DIR" '
    BEGIN {
      # Preceding char must not be alphanumeric, "/" or "." — plus start-of-line.
      re = "(^|[^A-Za-z0-9/.])" dir "/[A-Za-z0-9_./-]+"
    }
    {
      line = $0
      rest = line
      consumed = 0
      while (match(rest, re)) {
        tok = substr(rest, RSTART, RLENGTH)
        endpos = consumed + RSTART + RLENGTH - 1
        consumed += RSTART + RLENGTH - 1
        rest = substr(rest, RSTART + RLENGTH)
        # Drop the preceding separator the pattern had to consume.
        if (substr(tok, 1, 1) != substr(dir, 1, 1)) tok = substr(tok, 2)
        # A path that runs to end-of-line and stops on a joiner is continued on
        # the next line. Both joiners count: `-` inside a dated filename, and
        # `/` between segments. The `/` shape is the dangerous one — the token
        # left behind is a real directory, so a bare existence check passes and
        # the broken citation ships silently.
        if (tok ~ /[-\/]$/ && endpos == length(line)) {
          printf "WRAPPED\t%s\t%d\t%s\n", FILENAME, FNR, tok
          continue
        }
        # Trailing sentence punctuation is not part of the path.
        sub(/[.-]+$/, "", tok)
        printf "CHECK\t%s\t%d\t%s\n", FILENAME, FNR, tok
      }
    }
  ' "${candidates[@]}"
)

failures=0
checked=0

while IFS=$'\t' read -r status file line path; do
  [ -n "${status:-}" ] || continue
  checked=$((checked + 1))
  if [ "$status" = "WRAPPED" ]; then
    failures=$((failures + 1))
    printf '%s:%s: wrapped path — "%s" is broken across comment lines.\n' \
      "$file" "$line" "$path"
    printf '    A path split over two lines cannot be verified or followed. Keep it on one line.\n'
    continue
  fi
  if [ -e "$path" ] || [ -e "$path.md" ]; then
    continue
  fi
  failures=$((failures + 1))
  printf '%s:%s: dangling path — "%s" does not exist in this repository.\n' \
    "$file" "$line" "$path"
  printf '    Inline what the document said, or name the external source without an in-repo path.\n'
done <<< "$records"

if [ "$failures" -gt 0 ]; then
  echo
  echo "check-doc-pointers: $failures unresolvable citation(s) out of $checked checked."
  exit 1
fi

echo "check-doc-pointers: $checked citation(s), all resolve."
