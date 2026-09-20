#!/bin/bash
# Create the ONE self-signed code-signing certificate this project signs with,
# for local builds and for CI alike.
#
# Why a script rather than Keychain Access > Certificate Assistant:
#
#   * Certificate Assistant defaults to 365 days. When a signing certificate
#     expires you cannot sign with it any more, and the only fix is a new
#     certificate — which changes the designated requirement and silently
#     voids every user's Full Disk Access grant. This issues 20 years.
#   * CI needs the same identity as local builds, so the private key has to be
#     exportable as a .p12. Generating it here means both sides provably share
#     one certificate instead of two that merely have the same name.
#
# THE .p12 IS THE PROJECT'S SIGNING IDENTITY. Back it up. Regenerating it
# breaks FDA for every existing user (see the warning at the end).
set -euo pipefail
cd "$(dirname "$0")"

IDENTITY="${DSCAN_SIGN_IDENTITY:-disk-scanner-dev}"
OUT="${DSCAN_CERT_DIR:-.}"
P12="$OUT/$IDENTITY.p12"

if security find-certificate -c "$IDENTITY" >/dev/null 2>&1; then
  cat >&2 <<EOF
A certificate named "$IDENTITY" is already in your keychain.

Refusing to create a second one. Two certificates with the same name produce
DIFFERENT designated requirements, so builds would sign with whichever the
keychain returned first and Full Disk Access grants would break at random.

To genuinely start over, delete the existing one in Keychain Access first.
EOF
  exit 1
fi

if [[ -e "$P12" ]]; then
  echo "error: $P12 already exists; move it aside first" >&2
  exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# A code-signing leaf. codesign requires the codeSigning EKU; it does NOT
# require the certificate to be trusted (an untrusted self-signed identity
# reports CSSMERR_TP_NOT_TRUSTED from `security find-identity -v` yet signs
# perfectly well). Trust only affects Gatekeeper, which rejects us regardless
# without Apple notarisation.
cat > "$TMP/cert.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $IDENTITY
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
subjectKeyIdentifier = hash
EOF

echo "==> generating certificate (20 year validity)"
openssl req -x509 -newkey rsa:2048 -nodes -days 7300 \
  -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -config "$TMP/cert.cnf" 2>/dev/null

# A password is mandatory: `security import` cannot read a .p12 with an empty
# one. It is not a secret in any meaningful sense (the file itself is), so it
# is fixed rather than prompted, and travels beside the .p12 into CI.
P12_PASSWORD="${DSCAN_P12_PASSWORD:-disk-scanner}"

# -keypbe/-certpbe/-macalg: OpenSSL 3 defaults to AES-256-CBC + PBKDF2, which
# Apple's Security framework cannot parse — `security import` fails with
# "MAC verification failed during PKCS12 import (wrong password?)", which is
# a misleading message for an algorithm problem. These are the old PBE
# algorithms it does understand.
echo "==> exporting $P12"
openssl pkcs12 -export \
  -inkey "$TMP/key.pem" -in "$TMP/cert.pem" -name "$IDENTITY" \
  -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 \
  -out "$P12" -passout "pass:$P12_PASSWORD"
chmod 600 "$P12"

echo "==> importing into your login keychain"
# -T /usr/bin/codesign pre-authorises codesign to use the key. macOS still
# shows one "codesign wants to access key" prompt on first use; click Always
# Allow. (CI avoids the prompt entirely with set-key-partition-list, which
# needs a keychain password we only control for the throwaway CI keychain.)
security import "$P12" -k "$HOME/Library/Keychains/login.keychain-db" \
  -P "$P12_PASSWORD" -T /usr/bin/codesign -T /usr/bin/security

FPR="$(openssl x509 -in "$TMP/cert.pem" -noout -fingerprint -sha1 | cut -d= -f2)"

cat <<EOF

Created "$IDENTITY"
  SHA-1: $FPR
  valid until: $(openssl x509 -in "$TMP/cert.pem" -noout -enddate | cut -d= -f2)

Every build signed with this certificate gets the designated requirement

  identifier "foundation.msupply.diskscanner" and certificate leaf = H"$(echo "$FPR" | tr -d ':' | tr 'A-F' 'a-f')"

which names the CERTIFICATE, not the binary — so rebuilds keep working and a
Full Disk Access grant survives them. That is the entire point.

Next:
  ./build-app.sh                       build and sign locally

For CI, add two repository secrets (Settings > Secrets and variables > Actions):
  SIGNING_CERT_P12        base64 -i $P12 | pbcopy    (then paste)
  SIGNING_CERT_PASSWORD   $P12_PASSWORD

!! BACK UP $P12 SOMEWHERE SAFE (password manager, encrypted backup).
!! If you lose it you must issue a new certificate, which changes the
!! designated requirement above. Every existing user's Full Disk Access
!! grant then stops applying while System Settings still shows it ON, and
!! each of them has to remove the stale entry with "-" and re-add the app.
!! $P12 is gitignored. Keep it that way.
