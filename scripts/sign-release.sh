#!/bin/sh
# Sign the archive bytes with Ed25519, then verify against the app's public key. A release job signs with the CI key; a
# release signed by hand after that was lost names the offline key's public file (docs/releasing.md, Rotation).
# Usage: ATOMICS_RELEASE_SIGNING_KEY=<PKCS#8 PEM> sh scripts/sign-release.sh archive [public-key]
set +x
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo 'Usage: sh scripts/sign-release.sh archive [public-key]' >&2
  exit 2
fi
if [ -z "${ATOMICS_RELEASE_SIGNING_KEY:-}" ]; then
  echo 'ATOMICS_RELEASE_SIGNING_KEY is required (Ed25519 private key, PKCS#8 PEM); see docs/releasing.md.' >&2
  exit 1
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
archive=$1
public_key=${2:-"$root/assets/release-signing-key.pub"}
if [ ! -f "$public_key" ]; then
  echo 'Release public key missing: provision assets/release-signing-key.pub before releasing; see docs/releasing.md.' >&2
  exit 1
fi
openssl=${OPENSSL:-openssl}
if ! "$openssl" pkey -pubin -in "$public_key" -text_pub -noout | grep -q '^ED25519 Public-Key:'; then
  echo 'The release public key must be Ed25519 in PEM format.' >&2
  exit 1
fi

umask 077
work=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/atomics-sign.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
trap 'exit 1' HUP INT TERM
printf '%s\n' "$ATOMICS_RELEASE_SIGNING_KEY" > "$work/private.pem"
unset ATOMICS_RELEASE_SIGNING_KEY
# An empty passphrase prevents a malformed/encrypted secret from prompting on stdin.
"$openssl" pkeyutl -sign -rawin -inkey "$work/private.pem" -passin pass: \
  -in "$archive" -out "$work/archive.sig"
"$openssl" pkeyutl -verify -rawin -pubin -inkey "$public_key" \
  -in "$archive" -sigfile "$work/archive.sig"
[ "$(wc -c < "$work/archive.sig" | tr -d ' ')" = 64 ]
cp "$work/archive.sig" "$archive.sig"
