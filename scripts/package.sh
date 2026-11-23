#!/usr/bin/env bash
# Build the distributable .deb and normalise its desktop integration.
#
# Why this exists: Tauri generates `usr/share/applications/<ProductName>.desktop`
# (so `Sotto.desktop`), while the window's Wayland app id is lowercase `sotto`.
# KDE pairs a window to a desktop entry by that id, so a capitalised file means
# no icon in the taskbar and no correct window grouping. Installing a second,
# lowercase entry on top would list the app twice in the menu.
#
# So: build, then replace the generated entry with one lowercase file that has
# the app id, the icon, and the extra fields the generated one lacks. The result
# is one menu entry, one icon, and a taskbar button that matches.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(python3 -c 'import json;print(json.load(open("src-tauri/tauri.conf.json"))["version"])')"
DEB="target/release/bundle/deb/Sotto_${VERSION}_amd64.deb"

echo "==> building the app and the speech engine"
./scripts/build.sh

echo "==> bundling"
# The custom desktop file in tauri.conf.json is removed by this script, so the
# bundler's own entry is the only one present before normalisation.
cargo tauri build --bundles deb --no-bundle 2>/dev/null || true
cargo tauri build --bundles deb

[[ -f "$DEB" ]] || { echo "no package produced at $DEB" >&2; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/sotto-pkg-XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

echo "==> unpacking $(basename "$DEB")"
( cd "$WORK" && ar x "$ROOT/$DEB" )

mkdir -p "$WORK/data"
tar xzf "$WORK/data.tar.gz" -C "$WORK/data"

# One entry only, named after the window's app id.
rm -f "$WORK/data/usr/share/applications/Sotto.desktop"
install -Dm644 "$ROOT/packaging/sotto.desktop" \
  "$WORK/data/usr/share/applications/sotto.desktop"

# The generated control file has Maintainer: sotto and an empty long
# description ("(none)"). Both are visible in a package manager and both were
# release-blocking polish. Keep the generated dependency list and installed size;
# replace only the fields that are wrong.
echo "==> normalising the Debian control file"
mkdir -p "$WORK/control"
tar xzf "$WORK/control.tar.gz" -C "$WORK/control"
python3 - "$WORK/control/control" <<'PY'
import sys
path = sys.argv[1]
kept = []
for line in open(path, encoding="utf-8").read().splitlines():
    if line.startswith(("Maintainer:", "Section:", "Homepage:")):
        continue
    if line.startswith("Description:"):
        kept.append("Description: Sotto — quiet, local-first dictation")
        kept.append(" Local-first voice dictation for Linux. The speech model ships inside the")
        kept.append(" package, so nothing is downloaded and no audio leaves the computer.")
        continue
    if line.startswith(" "):  # the generated "(none)" continuation
        continue
    kept.append(line)
    if line.startswith("Priority:"):
        kept.append("Section: utils")
        kept.append("Homepage: https://github.com/MLuqmanBR/sotto")
kept.insert(4, "Maintainer: Sotto contributors <sotto@localhost>")
open(path, "w", encoding="utf-8").write("\n".join(kept) + "\n")
PY
( cd "$WORK/control" && tar czf "$WORK/control.tar.gz" --owner=0 --group=0 ./control ./md5sums )

# The icon must be reachable under the name the entry asks for, at the sizes
# KDE and GNOME actually look for.
for size in 32 48 64 128 256 512; do
  dir="$WORK/data/usr/share/icons/hicolor/${size}x${size}/apps"
  mkdir -p "$dir"
  magick -background none "$ROOT/src-tauri/icons/sotto.svg" \
    -resize "${size}x${size}" -depth 8 -define png:color-type=6 PNG32:"$dir/sotto.png"
done

echo "==> repacking"
( cd "$WORK/data" && tar czf "$WORK/data.tar.gz" --owner=0 --group=0 . )
( cd "$WORK" && rm -f "$ROOT/$DEB" && ar rc "$ROOT/$DEB" debian-binary control.tar.gz data.tar.gz )

echo "==> verifying the package"
FAIL=0
TGZ="$WORK/data.tar.gz"
# The listing is captured to a file first. Piping `tar tzf` into `grep -q` looks
# natural but is wrong under `set -o pipefail`: grep exits at the first match,
# tar is killed by SIGPIPE, and the pipeline reports failure even though the
# entry was found. That made early entries look missing while later ones passed.
LIST="$WORK/listing.txt"
tar tzf "$TGZ" > "$LIST"
entry() { tar xzf "$TGZ" -O ./usr/share/applications/sotto.desktop; }

pass() { echo "    ok   $1"; }
fail() { echo "    FAIL $1"; FAIL=1; }

grep -qx './usr/bin/sotto' "$LIST" \
  && pass "contains usr/bin/sotto" || fail "contains usr/bin/sotto"
grep -qx './usr/lib/Sotto/sotto-stt' "$LIST" \
  && pass "contains the speech engine" || fail "contains the speech engine"
grep -qx './usr/lib/Sotto/model/model.safetensors' "$LIST" \
  && pass "contains the model weights" || fail "contains the model weights"
# The About page tells the user the licence ships with the app, so it must.
grep -q 'usr/lib/Sotto/LICENSE' "$LIST" \
  && pass "contains the licence" || fail "contains the licence"

CONTROL="$(tar xzOf "$WORK/control.tar.gz" ./control 2>/dev/null || true)"
grep -q '^Maintainer: Sotto contributors' <<<"$CONTROL" \
  && pass "names a maintainer" || fail "names a maintainer"
grep -q '^Homepage: https://github.com/MLuqmanBR/sotto' <<<"$CONTROL" \
  && pass "names the project URL" || fail "names the project URL"
grep -A2 '^Description:' <<<"$CONTROL" | grep -q 'Local-first voice dictation' \
  && pass "has a real description" || fail "has a real description"

# The test-only surface (`--features e2e`) must never reach a package. It adds a
# command that writes arbitrary text to a path from the environment and a hook that
# evaluates a script from a file: useful for driving the window, unacceptable in
# something a user installs. Asserting it here means a release built with the wrong
# flags fails the packaging step instead of shipping.
# The binary must be present before it can be inspected. Checking a file that is
# not there would make `grep` find nothing and the check pass — the exact false
# pass this suite exists to avoid.
SHIPPED_BIN="$WORK/data/usr/bin/sotto"
if [[ ! -f "$SHIPPED_BIN" ]]; then
  fail "ships without the test-only surface (no binary at ${SHIPPED_BIN#$WORK/})"
elif strings "$SHIPPED_BIN" | grep -q "SOTTO_E2E_SCRIPT\|probe_write"; then
  fail "ships without the test-only surface"
else
  pass "ships without the test-only surface"
fi


DESKTOP_COUNT="$(grep -c 'usr/share/applications/.*\.desktop' "$LIST" || true)"
[[ "$DESKTOP_COUNT" == "1" ]] \
  && pass "contains exactly one desktop entry" \
  || fail "contains exactly one desktop entry (found $DESKTOP_COUNT)"

# A18: uninstall must leave nothing behind outside documented paths. Two things
# make that true, and both are checkable without root:
#   1. the package installs only under /usr, so removing it removes exactly that;
#   2. it carries no maintainer scripts, so nothing runs at install or uninstall
#      that could write elsewhere.
# The app's own runtime writes are confined to documented XDG paths, which the
# isolation suite proves. What is still not exercised is a real `rpm -e`, because
# that needs root.
# `|| true`: grep exits 1 when it finds nothing, which is the success case here,
# and `set -e` would otherwise abort the whole packaging step on a passing check.
OUTSIDE="$(grep -v '^\./usr/' "$LIST" | grep -v '^\./$' | head -5 || true)"
[[ -z "$OUTSIDE" ]] \
  && pass "installs only under /usr (A18)" || fail "installs only under /usr (A18): $OUTSIDE"

if ar p "$ROOT/$DEB" control.tar.gz | tar tz | grep -qE '^\./?(postinst|preinst|postrm|prerm)$'; then
  fail "carries no maintainer scripts (A18)"
else
  pass "carries no maintainer scripts (A18)"
fi

grep -q 'usr/share/applications/sotto.desktop' "$LIST" \
  && pass "desktop entry is named after the app id" \
  || fail "desktop entry is named after the app id"

entry | grep -qx 'StartupWMClass=sotto' \
  && pass "desktop entry declares the window class" \
  || fail "desktop entry declares the window class"

entry | grep -qx 'Icon=sotto' \
  && pass "desktop entry points at the icon" \
  || fail "desktop entry points at the icon"

grep -q 'usr/share/icons/hicolor/128x128/apps/sotto.png' "$LIST" \
  && pass "icon installed for hicolor" || fail "icon installed for hicolor"

# The entry must satisfy the desktop-entry spec, not merely parse. A file with
# two main categories validates as text but can list the app twice in the menu,
# which is the kind of defect only a validator catches.
if command -v desktop-file-validate >/dev/null 2>&1; then
  entry > "$WORK/entry.desktop"
  if desktop-file-validate "$WORK/entry.desktop" 2>"$WORK/dfv.log"; then
    pass "desktop entry validates against the spec"
  else
    fail "desktop entry validates against the spec: $(head -1 "$WORK/dfv.log")"
  fi
fi

SIZE="$(du -h "$ROOT/$DEB" | cut -f1)"
echo
echo "==> $DEB ($SIZE)"
[[ $FAIL -eq 0 ]] || { echo "package verification failed" >&2; exit 1; }

# The RPM is built from this same verified payload, so both formats carry one
# tree and one set of invariants. It is skipped with a message if rpmbuild is
# absent, rather than failing the run.
echo
echo "==> building the RPM from the same payload"
"$ROOT/scripts/package-rpm.sh"
