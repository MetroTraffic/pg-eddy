#!/usr/bin/env bash
# tck_floor.sh — enforce the TCK regression floor.
#
# Reads the minimum required pass count from tests/tck/baseline.txt, runs the
# full TCK suite, and exits non-zero if the actual pass count drops below the
# floor. Intended for CI and the release checklist (see AGENTS.md).
#
# Per plans/rs-polygraph-analysis.md §4.3: no phase may merge if TCK drops
# below the floor.
#
# Usage:
#   ./tests/tck/tck_floor.sh            # run TCK and check floor
#   ./tests/tck/tck_floor.sh --report   # also print classification of failures

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BASELINE_FILE="$REPO_ROOT/tests/tck/baseline.txt"

if [[ ! -f "$BASELINE_FILE" ]]; then
    echo "error: baseline file not found: $BASELINE_FILE" >&2
    exit 2
fi

# Read the floor (first non-comment, non-empty line).
FLOOR="$(grep -v '^\s*#' "$BASELINE_FILE" | grep -v '^\s*$' | head -1 | tr -d '[:space:]')"
if ! [[ "$FLOOR" =~ ^[0-9]+$ ]]; then
    echo "error: baseline.txt does not contain a valid integer floor: '$FLOOR'" >&2
    exit 2
fi

# Run the TCK.
cd "$REPO_ROOT"
rm -rf tmp_check

OUT="$(mktemp)"
DIAG="$(mktemp)"
trap 'rm -f "$OUT" "$DIAG"' EXIT

PG_REGRESS="${PG_REGRESS:-/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress}" \
PERL5LIB="/usr/lib/postgresql/18/lib/pgxs/src/test/perl:${PERL5LIB:-}" \
PATH="/usr/lib/postgresql/18/bin:$PATH" \
    perl tests/tck/run_tck.pl 2>"$DIAG" 1>"$OUT" || true

PASS_COUNT="$(grep -c '^ok' "$OUT" || true)"
FAIL_COUNT="$(grep -c '^not ok' "$OUT" || true)"
TOTAL="$((PASS_COUNT + FAIL_COUNT))"

echo "TCK results: $PASS_COUNT/$TOTAL passed (floor: $FLOOR)"

if [[ "$PASS_COUNT" -lt "$FLOOR" ]]; then
    DROP="$((FLOOR - PASS_COUNT))"
    echo "FAIL: TCK pass count dropped by $DROP below the floor." >&2
    echo "      Either fix the regression or update tests/tck/baseline.txt." >&2
    exit 1
fi

if [[ "$PASS_COUNT" -gt "$FLOOR" ]]; then
    GAIN="$((PASS_COUNT - FLOOR))"
    echo "INFO: TCK pass count is $GAIN above the floor; consider bumping baseline.txt."
fi

if [[ "${1:-}" == "--report" ]]; then
    echo
    echo "Failure summary (top 20 feature groups):"
    grep '^not ok' "$OUT" \
        | sed -E 's/^not ok [0-9]+ - ([A-Za-z]+[0-9]*).*$/\1/' \
        | sort | uniq -c | sort -rn | head -20
fi

exit 0
