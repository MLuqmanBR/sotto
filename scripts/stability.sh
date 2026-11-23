#!/usr/bin/env bash
# Run the test suite repeatedly and fail if it is not stable.
#
# Why this exists: three separate test defects in this project were *flaky* rather
# than wrong — tests sharing a temporary directory, tests racing on an environment
# variable. Each one passed most runs, so a single green run proved nothing about
# them. A suite that fails one run in four trains you to ignore failures, which is
# worse than having no test at all.
#
# Usage:  scripts/stability.sh [runs]
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

RUNS="${1:-5}"
FAILED=0
declare -a FAILURES=()

echo "Sotto stability check — $RUNS runs of the workspace suite"
echo

for i in $(seq 1 "$RUNS"); do
  OUT="$(cargo test --workspace 2>&1)"
  SUMMARY="$(grep -cE '^test result: ok' <<<"$OUT" || true)"
  BAD="$(grep -E '^test result: FAILED|^test .* FAILED' <<<"$OUT" || true)"
  if [[ -z "$BAD" ]]; then
    echo "  run $i/$RUNS  ok   ($SUMMARY suites passed)"
  else
    echo "  run $i/$RUNS  FAIL"
    echo "$BAD" | sed 's/^/        /'
    FAILED=$((FAILED+1))
    FAILURES+=("run $i: $(echo "$BAD" | head -1)")
  fi
done

echo
if [[ $FAILED -eq 0 ]]; then
  echo "==> stable: $RUNS/$RUNS runs passed"
  exit 0
fi

echo "==> NOT stable: $FAILED of $RUNS runs failed"
for f in "${FAILURES[@]}"; do echo "    $f"; done
echo
echo "A flaky test is a defect in the test. Find the shared state (temp dir, env"
echo "var, port, audio device) and remove it, rather than re-running until green."
exit 1
