#!/usr/bin/env bash
# Sign the release packages.
#
# Two formats, two mechanisms, because that is what each ecosystem actually
# verifies:
#
#   .rpm  An OpenPGP signature in the RPM header. `rpmkeys --checksig` and `dnf`
#         both verify it against the imported public key, and `dnf install` warns
#         or refuses when it does not match.
#
#   .deb  Debian has no in-package signature that apt checks for a local file
#         install, so the standard practice is a **detached** signature next to
#         the file. `debsigs` would embed one, but it is not packaged for Fedora;
#         a detached `.asc` is what can be produced and verified here, and
#         `gpg --verify` is a check any user can run without extra tooling.
#
# The passphrase is never read, echoed or stored by this script. It is unlocked
# once into gpg-agent (see README), and rpm/gpg ask the agent.
#
# Usage: scripts/sign.sh [--verify-only]
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(python3 -c 'import json;print(json.load(open("src-tauri/tauri.conf.json"))["version"])')"
RPM="target/release/bundle/rpm/sotto-${VERSION}-1.x86_64.rpm"
DEB="target/release/bundle/deb/Sotto_${VERSION}_amd64.deb"
KEYFILE="keys/sotto-signing-key.asc"
FPR="$(gpg --with-colons --import-options show-only --import "$KEYFILE" 2>/dev/null | awk -F: '/^fpr/{print $10; exit}')"

FAIL=0
ok()   { echo "  $(printf '\033[32mPASS\033[0m')  $1"; }
bad()  { echo "  $(printf '\033[31mFAIL\033[0m')  $1${2:+  ($2)}"; FAIL=1; }
head_(){ echo; echo "$(printf '\033[1m')$1$(printf '\033[0m')"; }

[[ -n "$FPR" ]] || { echo "no public key at $KEYFILE" >&2; exit 1; }

if [[ "${1:-}" != "--verify-only" ]]; then
  head_ "1. Sign the RPM"
  for f in "$RPM" "$DEB"; do
    [[ -f "$f" ]] || { echo "missing $f — run scripts/package.sh first" >&2; exit 1; }
  done

  # rpmsign is idempotent: a second run reports "identical signature, skipping".
  # That is not a failure, so the exit status alone cannot be the check; the
  # signature is verified below.
  if rpmsign --addsign "$RPM" 2>&1 | sed 's/^/        /'; then
    ok "rpmsign ran"
  else
    bad "rpmsign ran" "see output above"
  fi

  head_ "2. Detached signature for the .deb"
  rm -f "$DEB.asc"
  if gpg --batch --yes --local-user "$FPR" --armor --detach-sign \
        --output "$DEB.asc" "$DEB" 2>&1 | sed 's/^/        /'; then
    [[ -s "$DEB.asc" ]] && ok "wrote $(basename "$DEB.asc")" \
                         || bad "wrote $(basename "$DEB.asc")" "file is empty"
  else
    bad "signed the .deb" "see output above"
  fi

  head_ "3. Checksums for both"
  ( cd target/release/bundle && sha256sum "deb/$(basename "$DEB")" "rpm/$(basename "$RPM")" ) \
    > "target/release/bundle/SHA256SUMS"
  gpg --batch --yes --local-user "$FPR" --armor --detach-sign \
      --output "target/release/bundle/SHA256SUMS.asc" "target/release/bundle/SHA256SUMS" 2>/dev/null \
    && ok "SHA256SUMS and its signature written" \
    || bad "SHA256SUMS and its signature written"
fi

head_ "4. Verify what a user would verify"

# rpmkeys needs the public key in the RPM database. The check is only meaningful
# with it imported, so report which state we are in rather than a bare NOKEY.
if rpm -qa gpg-pubkey* --qf '%{SUMMARY}\n' 2>/dev/null | grep -q "Sotto Releases"; then
  OUT="$(rpmkeys -v --checksig "$RPM" 2>&1)"
  if grep -q "signature, key fingerprint: $(echo "$FPR" | tr 'A-Z' 'a-z'): OK" <<<"$OUT"; then
    ok "RPM signature verifies against the imported key"
  else
    bad "RPM signature verifies against the imported key" "$(tail -2 <<<"$OUT" | tr '\n' ' ')"
  fi
  grep -q "Payload SHA256 digest: OK" <<<"$OUT" \
    && ok "RPM payload digest is intact" || bad "RPM payload digest is intact"
else
  echo "        note: public key not in the RPM database, skipping rpmkeys"
  echo "        import it with: sudo rpm --import $KEYFILE"
fi

if gpg --verify "$DEB.asc" "$DEB" 2>"$ROOT/.sign-verify.log"; then
  ok "detached .deb signature verifies"
  grep -q "Good signature from \"Sotto Releases" "$ROOT/.sign-verify.log" \
    && ok "it is signed by Sotto Releases" || bad "it is signed by Sotto Releases"
else
  bad "detached .deb signature verifies" "$(tail -2 "$ROOT/.sign-verify.log" | tr '\n' ' ')"
fi
rm -f "$ROOT/.sign-verify.log"

if gpg --verify "target/release/bundle/SHA256SUMS.asc" \
              "target/release/bundle/SHA256SUMS" 2>/dev/null; then
  ok "checksum file signature verifies"
else
  bad "checksum file signature verifies"
fi

head_ "5. A tampered copy must be refused"

# A signature check that passes on a modified file would be worthless, so prove
# it catches one. The copy is corrupted in a throwaway directory; the real
# artifact is never touched.
TMP="$(mktemp -d)"
cp "$RPM" "$TMP/tampered.rpm"
printf 'x' | dd of="$TMP/tampered.rpm" bs=1 seek=200000 conv=notrunc status=none
# Capture first, then grep. Piping `rpmkeys` straight into `grep -q` looks natural
# and is wrong under `set -o pipefail`: rpmkeys exits non-zero for a bad
# signature, so the pipeline reports failure even when grep matched. This is the
# same trap that once made the package listing look incomplete.
BAD_RPM="$(rpmkeys --checksig "$TMP/tampered.rpm" 2>&1 || true)"
if grep -q "NOT OK" <<<"$BAD_RPM"; then
  ok "a modified RPM is reported as not OK"
else
  bad "a modified RPM is reported as not OK" "$(tail -1 <<<"$BAD_RPM")"
fi

cp "$DEB" "$TMP/tampered.deb"
printf 'x' | dd of="$TMP/tampered.deb" bs=1 seek=200000 conv=notrunc status=none
if gpg --verify "$DEB.asc" "$TMP/tampered.deb" >/dev/null 2>&1; then
  bad "a modified .deb fails its signature" "verification passed on a corrupted file"
else
  ok "a modified .deb fails its signature"
fi
rm -rf "$TMP"

echo
echo "─────────────────────────────────────────────"
if [[ "$FAIL" -eq 0 ]]; then
  echo "  Signed and verified."
  echo "  Key fingerprint: $FPR"
else
  echo "  Signing verification FAILED."
  exit 1
fi