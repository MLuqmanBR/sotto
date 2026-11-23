#!/usr/bin/env bash
# Build a Fedora/OpenSUSE .rpm from the payload that scripts/package.sh has
# already normalised.
#
# Why this exists: the target machine is Fedora 44, and until now the only
# package was a .deb — so the app could not be installed on the machine it was
# written for. Rather than let Tauri's RPM bundler produce a second, differently
# shaped tree (with its own capitalised desktop entry and its own icon
# handling), the RPM is built from the *same* verified payload the .deb uses.
# One tree, two formats, one set of invariants.
#
# Nothing is installed. The RPM is inspected with rpm2cpio, so no package
# database on this machine is touched.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(python3 -c 'import json;print(json.load(open("src-tauri/tauri.conf.json"))["version"])')"
DEB="target/release/bundle/deb/Sotto_${VERSION}_amd64.deb"
OUT_DIR="target/release/bundle/rpm"
RPM="$OUT_DIR/sotto-${VERSION}-1.x86_64.rpm"

[[ -f "$DEB" ]] || {
  echo "no .deb to build from at $DEB — run scripts/package.sh first" >&2
  exit 1
}

if ! command -v rpmbuild >/dev/null 2>&1; then
  echo "rpmbuild is not installed; skipping the RPM (install rpm-build to build it)" >&2
  exit 0
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/sotto-rpm-XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

echo "==> unpacking the verified payload from $(basename "$DEB")"
( cd "$WORK" && ar x "$ROOT/$DEB" )
mkdir -p "$WORK/payload"
tar xzf "$WORK/data.tar.gz" -C "$WORK/payload"

# The file list is generated from the payload rather than written by hand: a
# hand-written list drifts the moment a file is added, and the drift is silent.
FILES="$WORK/files.txt"
( cd "$WORK/payload" && find . -type f -printf '/%P\n' | sort ) > "$FILES"

# Directories the package creates for itself must be listed as owned, or `rpm -e`
# removes the files and leaves empty directories behind. That is a real A18
# failure: uninstalling left /usr/lib/Sotto and /usr/lib/Sotto/model on disk.
#
# Only app-specific directories are claimed. Generic ones (/usr/bin,
# /usr/share/applications, /usr/share/icons/...) belong to the `filesystem`
# package; claiming those would conflict with it.
DIRS="$WORK/dirs.txt"
( cd "$WORK/payload" && find . -type d -path './usr/lib/Sotto*' ! -path . -printf '%%dir /%P\n' | sort ) > "$DIRS"

mkdir -p "$WORK/rpmbuild"/{BUILD,RPMS,SOURCES,SPECS,SPECS}

echo "==> writing the spec"
{
  cat <<SPEC
Name:           sotto
Version:        ${VERSION}
Release:        1
Summary:        Local-first voice dictation
# SPDX expression: the app is MIT, the vendored engine Apache-2.0, and the
# bundled model CC-BY-4.0. The About page carries the attribution the model
# licence requires.
License:        MIT AND Apache-2.0 AND CC-BY-4.0
URL:            https://github.com/MLuqmanBR/sotto
BuildArch:      x86_64
AutoReqProv:    yes

%description
Sotto turns speech into text on the machine it runs on. The speech model ships
inside the package, so nothing is downloaded and no audio leaves the computer.

%install
mkdir -p %{buildroot}
cp -a ${WORK}/payload/. %{buildroot}/

%files
%defattr(-,root,root,-)
SPEC
  cat "$FILES"
  cat "$DIRS"
  cat <<'SPEC'

%changelog
* Mon Nov 23 2026 sotto <sotto@localhost> - ${VERSION}-1
- First build.
SPEC
} > "$WORK/rpmbuild/SPECS/sotto.spec"

echo "==> building"
mkdir -p "$OUT_DIR"
rpmbuild -bb \
  --define "_topdir $WORK/rpmbuild" \
  --define "_rpmdir $WORK/rpmbuild/RPMS" \
  --define "debug_package %{nil}" \
  --define "_build_id_links none" \
  "$WORK/rpmbuild/SPECS/sotto.spec" > "$WORK/rpmbuild.log" 2>&1 || {
    echo "rpmbuild failed:" >&2
    tail -30 "$WORK/rpmbuild.log" >&2
    exit 1
  }

BUILT="$(find "$WORK/rpmbuild/RPMS" -name '*.rpm' | head -1)"
[[ -n "$BUILT" ]] || { echo "rpmbuild produced no package" >&2; exit 1; }
cp "$BUILT" "$RPM"

echo "==> verifying the package"
# Inspected, never installed: no rpm database on this machine is touched.
rm -rf "$WORK/check" && mkdir -p "$WORK/check"
( cd "$WORK/check" && rpm2cpio "$ROOT/$RPM" | cpio -idm --quiet )
CHECK="$WORK/check"

FAIL=0
pass() { echo "    ok   $1"; }
fail() { echo "    FAIL $1"; FAIL=1; }

[[ -x "$CHECK/usr/bin/sotto" ]] \
  && pass "contains usr/bin/sotto" || fail "contains usr/bin/sotto"
[[ -x "$CHECK/usr/lib/Sotto/sotto-stt" ]] \
  && pass "contains the speech engine" || fail "contains the speech engine"
[[ -f "$CHECK/usr/lib/Sotto/model/model.safetensors" ]] \
  && pass "contains the model weights" || fail "contains the model weights"
[[ -f "$CHECK/usr/lib/Sotto/LICENSE" ]] \
  && pass "contains the licence" || fail "contains the licence"

# See package.sh: the e2e surface must never reach a package.
if [[ ! -f "$CHECK/usr/bin/sotto" ]]; then
  fail "ships without the test-only surface (no binary in the package)"
elif strings "$CHECK/usr/bin/sotto" | grep -q "SOTTO_E2E_SCRIPT\|probe_write"; then
  fail "ships without the test-only surface"
else
  pass "ships without the test-only surface"
fi

# See package.sh: A18, checked as far as it can be without root.
RPM_FILES="$(rpm -qpl "$ROOT/$RPM" 2>/dev/null || true)"
RPM_OUTSIDE="$(grep -v '^/usr/' <<<"$RPM_FILES" | head -5 || true)"
[[ -z "$RPM_OUTSIDE" ]] \
  && pass "installs only under /usr (A18)" || fail "installs only under /usr (A18): $RPM_OUTSIDE"

if [[ -n "$(rpm -qp --scripts "$ROOT/$RPM" 2>/dev/null)" ]]; then
  fail "carries no maintainer scripts (A18)"
else
  pass "carries no maintainer scripts (A18)"
fi

COUNT="$(find "$CHECK/usr/share/applications" -name '*.desktop' 2>/dev/null | wc -l)"
[[ "$COUNT" == "1" ]] \
  && pass "contains exactly one desktop entry" \
  || fail "contains exactly one desktop entry (found $COUNT)"

[[ -f "$CHECK/usr/share/applications/sotto.desktop" ]] \
  && pass "desktop entry is named after the app id" \
  || fail "desktop entry is named after the app id"

grep -qx 'StartupWMClass=sotto' "$CHECK/usr/share/applications/sotto.desktop" 2>/dev/null \
  && pass "desktop entry declares the window class" \
  || fail "desktop entry declares the window class"

grep -qx 'Icon=sotto' "$CHECK/usr/share/applications/sotto.desktop" 2>/dev/null \
  && pass "desktop entry points at the icon" \
  || fail "desktop entry points at the icon"

[[ -f "$CHECK/usr/share/icons/hicolor/128x128/apps/sotto.png" ]] \
  && pass "icon installed for hicolor" || fail "icon installed for hicolor"

if command -v desktop-file-validate >/dev/null 2>&1; then
  if desktop-file-validate "$CHECK/usr/share/applications/sotto.desktop" 2>"$WORK/dfv.log"; then
    pass "desktop entry validates against the spec"
  else
    fail "desktop entry validates against the spec: $(head -1 "$WORK/dfv.log")"
  fi
fi

# An RPM that requires nothing would install onto a machine with no GTK and
# fail at launch, so the dependency generator must have found the libraries.
REQS="$(rpm -qpR "$ROOT/$RPM" 2>/dev/null || true)"
if grep -qiE 'libgtk-3|libwebkit2gtk' <<<"$REQS"; then
  pass "declares its GTK/WebKit dependencies"
else
  fail "declares its GTK/WebKit dependencies"
fi

SIZE="$(du -h "$ROOT/$RPM" | cut -f1)"
META="$(rpm -qip "$ROOT/$RPM" 2>/dev/null || true)"
grep -q '^License.*MIT AND Apache-2.0 AND CC-BY-4.0' <<<"$META" \
  && pass "declares the real licence" || fail "declares the real licence"
grep -q 'github.com/MLuqmanBR/sotto' <<<"$META" \
  && pass "names the project URL" || fail "names the project URL"
echo
echo "==> $RPM ($SIZE)"
[[ $FAIL -eq 0 ]] || { echo "RPM verification failed" >&2; exit 1; }
