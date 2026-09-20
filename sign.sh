#!/bin/bash
# Sign the built binary with a STABLE identity.
#
# Why this exists: TCC binds a Full Disk Access grant to the code-signing
# requirement captured at the moment the user granted it. Ad-hoc signing
# (`codesign -s -`) pins that requirement to the binary's cdhash, which
# changes on every single `cargo build` — so the grant silently stops
# applying while System Settings still shows the toggle as ON.
#
# Signing with a real certificate binds to the certificate instead, so the
# grant survives rebuilds. Do NOT replace this with `codesign -s -`.
set -euo pipefail

IDENTITY="${DSCAN_SIGN_IDENTITY:-disk-scanner-dev}"
BIN="${1:-target/release/dscan}"

if [[ ! -f "$BIN" ]]; then
  echo "error: $BIN not found (cargo build --release first)" >&2
  exit 1
fi

if ! security find-certificate -c "$IDENTITY" >/dev/null 2>&1; then
  cat >&2 <<EOF
No code-signing certificate named "$IDENTITY" was found.

Create one ONCE (it is reused for every build):

  1. Open Keychain Access
  2. Menu: Keychain Access > Certificate Assistant > Create a Certificate...
  3. Name:             $IDENTITY
     Identity Type:    Self Signed Root
     Certificate Type: Code Signing
  4. Create, then leave it in the "login" keychain.

Then re-run: ./sign.sh

For distribution use a Developer ID Application certificate instead and set
DSCAN_SIGN_IDENTITY to its name, plus --options runtime and notarisation.
EOF
  exit 1
fi

codesign --force --sign "$IDENTITY" --timestamp=none "$BIN"
echo "signed $BIN as '$IDENTITY'"
codesign -d --requirements - "$BIN" 2>&1 | sed 's/^/  /'
cat <<EOF

The requirement above is what TCC will store. Because it names the
certificate rather than a hash, a rebuild keeps the same identity and any
Full Disk Access grant continues to apply.

If a grant ever stops working after changing signing identity, remove the
stale entry with the "-" button in System Settings and re-add it. Toggling
it off and on does NOT clear the stored requirement.
EOF
