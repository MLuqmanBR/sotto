#!/usr/bin/env bash
# Everything that has to pass before this can be called finished.
#
# One command, in the order a failure is cheapest to diagnose: compile, unit and
# integration tests, the static checks, the sandboxed end-to-end suite, the real
# window driven through its own buttons, repeated runs to catch flakiness, then
# packaging for both formats.
#
# The point is that no step is remembered or skipped. A "publish-ready" claim that
# depends on someone recalling the sixth script is not a claim.
#
# Usage:  scripts/verify-all.sh [--quick]
#   --quick  skip the repeated-run stability pass (the slowest step)
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

QUICK=0
[[ "${1:-}" == "--quick" ]] && QUICK=1

export TMPDIR="${TMPDIR:-$HOME/.cache/build-tmp}"
mkdir -p "$TMPDIR"

STEPS=()
FAILED=0

step() {
  local name="$1"; shift
  echo
  echo "━━━ $name"
  if "$@"; then
    echo "    ▸ $name: PASS"
    STEPS+=("PASS  $name")
  else
    echo "    ▸ $name: FAIL"
    STEPS+=("FAIL  $name")
    FAILED=$((FAILED+1))
  fi
}

build()          { ./scripts/build.sh; }
tests()          { cargo test --workspace; }
warnings()       { [[ "$(./scripts/build.sh 2>&1 | grep -c '^warning')" == "0" ]]; }
command_surface(){ python3 scripts/check-command-surface.py; }
palette()        { node scripts/check-palette.js; }
shortcuts()      { node scripts/check-shortcuts.js; }
readme()         { python3 scripts/check-readme.py; }
dictlayout()     { ./scripts/check-dictionary-layout.py; }
signing()        { ./scripts/sign.sh; }
isolation()      { ./scripts/isolation-test.sh; }
gui()            { ./scripts/gui-e2e.sh; }
x11()            { ./scripts/x11-overlay.sh; }
tray()           { ./scripts/tray-test.sh; }
install()        { ./scripts/install-test.sh; }
deb_install()    { ./scripts/deb-install-test.sh; }
stability()      { ./scripts/stability.sh 3; }
package()        { ./scripts/package.sh; }

step "Build (release, no warnings)"        warnings
step "Workspace tests"                     tests
step "Command surface"                     command_surface
step "Accent palette"                      palette
step "Shortcut defaults and guard"         shortcuts
step "Public README"                       readme
step "Dictionary row layout"                dictlayout
step "Isolation suite"                     isolation
step "Real window, real buttons"           gui
step "X11 placement on an offset monitor"  x11
step "Tray icon behaviour"                tray
# Packages first: the install tests must exercise the artifacts this run built,
# not whatever was left in target/ by an earlier version. Running them before the
# build made a clean checkout fail with "no package at ...".
step "Packages (.deb and .rpm)"            package
# After packaging, because packaging rebuilds the artifacts and would discard a
# signature added earlier.
step "Sign and verify the packages"        signing
step "Install and uninstall the RPM"       install
step "Install and uninstall the .deb"      deb_install
if [[ $QUICK -eq 0 ]]; then
  step "Stability (3 consecutive runs)"    stability
fi

echo
echo "═════════════════════════════════════════════"
for line in "${STEPS[@]}"; do echo "  $line"; done
echo "═════════════════════════════════════════════"
if [[ $FAILED -eq 0 ]]; then
  echo "  All ${#STEPS[@]} steps passed."
  exit 0
fi
echo "  $FAILED of ${#STEPS[@]} steps failed."
exit 1
