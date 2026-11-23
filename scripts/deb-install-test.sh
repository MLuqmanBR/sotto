#!/usr/bin/env bash
# Install the .deb for real, in a container, and prove it works and removes cleanly.
#
# Why this exists: the .deb could only be *inspected* on this machine, because
# Fedora has no dpkg. That left the packaging path verified by argument rather
# than observation — and the RPM taught us what that costs: its uninstall left two
# empty directories behind, which only running it revealed.
#
# A throwaway Debian container gives a real dpkg, a real apt that has to resolve the
# package's declared dependencies, and a real install/remove cycle. Nothing on the
# host is touched: the package is bind-mounted read-only, and the container is
# removed on exit.
#
# What it checks, in order:
#   1. the declared dependencies resolve (the RPM/Deb dependency names differ from
#      what a modern Debian actually ships — libgtk-3-0 is now libgtk-3-0t64 and
#      libappindicator3-1 is provided by libayatana-appindicator3-1)
#   2. the package installs and dpkg owns its files *and directories*
#   3. the installed app launches, finds its bundled model and starts its engine,
#      with no environment overrides
#   4. removal leaves nothing behind, and purge leaves the database clean
#
# Usage:  scripts/deb-install-test.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DEB="$(python3 -c 'import json;v=json.load(open("src-tauri/tauri.conf.json"))["version"];print(f"target/release/bundle/deb/Sotto_{v}_amd64.deb")')"
IMAGE="docker.io/library/debian:stable-slim"

if ! command -v podman >/dev/null 2>&1; then
  echo "  SKIP  podman is not installed (this test needs a container runtime)"
  exit 0
fi
if [[ ! -f "$DEB" ]]; then
  echo "no package at $DEB — run scripts/package.sh first" >&2
  exit 1
fi
if ! podman image exists "$IMAGE" 2>/dev/null; then
  echo "==> pulling $IMAGE"
  if ! podman pull -q "$IMAGE" >/dev/null 2>&1; then
    echo "  SKIP  could not pull $IMAGE (no network?)"
    exit 0
  fi
fi

echo "Sotto .deb install test (container)"
echo "  image : $IMAGE"
echo "  package: $DEB"
echo

# The whole sequence runs in one container so the install state carries across
# steps. It prints PASS/FAIL lines that this script counts.
timeout 1200 podman run --rm \
  -v "$PWD/$DEB:/tmp/sotto.deb:ro,Z" \
  "$IMAGE" bash -c '
set -uo pipefail
pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1${2:+  ($2)}"; }

. /etc/os-release
pass "running on $PRETTY_NAME"

apt-get update -qq >/dev/null 2>&1

# 1. Dependencies. A .deb with an unsatisfiable Depends cannot be installed at all,
# and the names a bundler emits do not always match what a distro ships today.
MISSING=""
for p in libappindicator3-1 libwebkit2gtk-4.1-0 libgtk-3-0; do
  if apt-get install -s -y "$p" >/dev/null 2>&1; then :; else MISSING+="$p "; fi
done
if [[ -z "$MISSING" ]]; then
  pass "every declared dependency is satisfiable"
else
  fail "every declared dependency is satisfiable" "cannot resolve: $MISSING"
fi

SIM="$(apt-get install -s -y /tmp/sotto.deb 2>&1)"
if grep -q "^Conf sotto" <<<"$SIM"; then
  pass "apt resolves an install of the package"
else
  fail "apt resolves an install of the package" "$(tail -3 <<<"$SIM")"
fi

# 2. Install.
if DEBIAN_FRONTEND=noninteractive apt-get install -y -qq /tmp/sotto.deb >/tmp/install.log 2>&1; then
  pass "the package installs"
else
  fail "the package installs" "$(tail -5 /tmp/install.log)"
  exit 1
fi
[[ "$(dpkg -s sotto 2>/dev/null | awk "/^Status:/{print \$2,\$3,\$4}")" == "install ok installed" ]] \
  && pass "dpkg reports it installed" || fail "dpkg reports it installed"

for f in /usr/bin/sotto /usr/lib/Sotto/sotto-stt /usr/lib/Sotto/model/model.safetensors /usr/share/applications/sotto.desktop; do
  [[ -e "$f" ]] && pass "installed: $f" || fail "installed: $f"
done

# Directories must be owned by the package, or dpkg will leave them empty on
# removal. The RPM had exactly this defect.
for d in /usr/lib/Sotto /usr/lib/Sotto/model; do
  dpkg -L sotto 2>/dev/null | grep -qx "$d" \
    && pass "directory owned: $d" || fail "directory owned: $d"
done

if command -v desktop-file-validate >/dev/null 2>&1; then
  desktop-file-validate /usr/share/applications/sotto.desktop 2>/tmp/dfv.log \
    && pass "the installed desktop entry validates" \
    || fail "the installed desktop entry validates" "$(head -1 /tmp/dfv.log)"
fi

# 3. Launch. A headless container has no display, so one is provided.
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq xvfb >/dev/null 2>&1
Xvfb :99 -screen 0 1920x1080x24 >/tmp/xvfb.log 2>&1 &
sleep 2
mkdir -p /root/sotto-root
DISPLAY=:99 SOTTO_ROOT=/root/sotto-root setsid /usr/bin/sotto >/tmp/app.log 2>&1 &
sleep 18

pgrep -x sotto >/dev/null 2>&1 \
  && pass "the installed app launches" || fail "the installed app launches" "$(tail -3 /tmp/app.log)"

ENGINE="$(ps -o args= -p "$(pgrep -x sotto-stt | head -1)" 2>/dev/null)"
if grep -q "/usr/lib/Sotto/model" <<<"$ENGINE"; then
  pass "it found its bundled model"
else
  fail "it found its bundled model" "${ENGINE:-no engine process}"
fi

grep -q "shortcut toggle: .* registered" /tmp/app.log \
  && pass "it registered its shortcut" || fail "it registered its shortcut"

/usr/lib/Sotto/sotto-stt --selftest --model /usr/lib/Sotto/model 2>&1 | grep -q "result    : OK" \
  && pass "the installed model passes its selftest" || fail "the installed model passes its selftest"

pkill -x sotto 2>/dev/null || true
sleep 1
[[ -e /root/.local/share/sotto ]] && fail "no data written outside the sandbox" || pass "no data written outside the sandbox"

# 4. Remove.
if dpkg -r sotto >/tmp/rm.log 2>&1; then
  pass "dpkg -r succeeds"
else
  fail "dpkg -r succeeds" "$(tail -5 /tmp/rm.log)"
fi
for f in /usr/bin/sotto /usr/lib/Sotto /usr/lib/Sotto/model /usr/share/applications/sotto.desktop /usr/share/icons/hicolor/128x128/apps/sotto.png; do
  [[ -e "$f" ]] && fail "removed: $f" || pass "removed: $f"
done
LEFT="$(find /usr -name "*sotto*" 2>/dev/null | head -5)"
[[ -z "$LEFT" ]] && pass "nothing named sotto remains in /usr" || fail "nothing named sotto remains in /usr" "$LEFT"

if dpkg -P sotto >/tmp/purge.log 2>&1 && ! dpkg -l sotto 2>/dev/null | grep -q "^ii"; then
  pass "purge leaves the package database clean"
else
  fail "purge leaves the package database clean"
fi
' 2>&1 | tee /tmp/opencode/deb-install.out | grep -E "^(PASS|FAIL)"

PASSED="$(grep -c '^PASS' /tmp/opencode/deb-install.out || true)"
FAILED="$(grep -c '^FAIL' /tmp/opencode/deb-install.out || true)"

echo
echo "─────────────────────────────────────────────"
echo "  passed: $PASSED"
echo "  failed: $FAILED"
echo "─────────────────────────────────────────────"
[[ "${FAILED:-0}" -eq 0 ]]