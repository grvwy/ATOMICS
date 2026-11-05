#!/bin/sh
# Makes ATOMICS's two release keys. For the maintainer, once, on a trusted machine, from a checkout, with the `release`
# GitHub Environment already there: see docs/releasing.md.
#
#   scripts/make-release-keys.sh <file for the offline private key, outside this checkout>
#
# The CI key goes straight into the Environment and stays nowhere else. If it is ever lost, make a new one: the offline
# key is what lets installed copies accept the release that brings it. The offline key goes to the file named, encrypted
# with a passphrase asked for here, and should then leave this machine. Only the two public keys land in the checkout, in
# assets/, to be committed. Nothing is stored or written until both keys have signed and verified a test message.
set +x
set -eu
umask 077

repo=xmagit/ATOMICS
environment=release
secret=ATOMICS_RELEASE_SIGNING_KEY

fail() {
  printf 'make-release-keys: %s\n' "$*" >&2
  exit 1
}

[ "$#" -eq 1 ] || fail 'usage: scripts/make-release-keys.sh <file for the offline private key, outside this checkout>'
case "$1" in
  /*) offline=$1 ;;
  *) offline="$PWD/$1" ;;
esac

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
ci_public="$root/assets/release-signing-key.pub"
offline_public="$root/assets/release-signing-key-offline.pub"
for existing in "$ci_public" "$offline_public"; do
  [ ! -e "$existing" ] ||
    fail "$existing already exists, and installed copies trust it. Changing a key is in docs/releasing.md, under Rotation."
done
offline_folder=$(CDPATH= cd -- "$(dirname -- "$offline")" 2>/dev/null && pwd) ||
  fail "there is no folder $(dirname -- "$offline")."
case "$offline_folder/" in
  "$root"/*) fail "the offline key must stay out of this checkout." ;;
esac
offline="$offline_folder/$(basename -- "$offline")"
[ ! -e "$offline" ] || fail "$offline already exists, and this will not replace a key."

openssl=
for candidate in "${OPENSSL:-openssl}" /opt/homebrew/opt/openssl@3/bin/openssl /usr/local/opt/openssl@3/bin/openssl; do
  if "$candidate" version 2>/dev/null | grep -q '^OpenSSL [3-9]'; then
    openssl=$candidate
    break
  fi
done
[ -n "$openssl" ] || fail 'this takes OpenSSL 3 or later. On a Mac: brew install openssl@3'
command -v gh >/dev/null 2>&1 || fail 'this takes the GitHub CLI, gh.'
rules=$(gh api "repos/$repo/environments/$environment" --jq '.protection_rules[].type' 2>/dev/null) ||
  fail "the $environment Environment of $repo is not there yet: docs/releasing.md says how to make it."
case "$rules" in
  *required_reviewers*) ;;
  *) fail "the $environment Environment has no required reviewer, so nobody would approve a use of this key." ;;
esac

ask() {
  printf '%s' "$1" >&2
  if [ -t 0 ]; then
    # Whatever stops it, the terminal gets its echo back.
    trap 'stty echo; exit 1' HUP INT TERM
    stty -echo
    IFS= read -r answer || answer=
    stty echo
    trap - HUP INT TERM
    printf '\n' >&2
  else
    IFS= read -r answer || answer=
  fi
  printf '%s' "$answer"
}
pass=$(ask 'Passphrase for the offline key: ')
[ -n "$pass" ] || fail 'the offline key needs a passphrase.'
[ "$(ask 'The same again: ')" = "$pass" ] || fail 'the two passphrases differ.'

work=$(mktemp -d)
staged="$offline.new"
trap 'rm -rf -- "$work" "$staged"' EXIT
trap 'exit 1' HUP INT TERM

"$openssl" genpkey -algorithm ED25519 -out "$work/ci.pem"
"$openssl" pkey -in "$work/ci.pem" -pubout -out "$work/ci.pub"
"$openssl" genpkey -algorithm ED25519 -aes-256-cbc -pass fd:3 -out "$staged" 3<<PASS
$pass
PASS
"$openssl" pkey -in "$staged" -passin fd:3 -pubout -out "$work/offline.pub" 3<<PASS
$pass
PASS
cmp -s "$work/ci.pub" "$work/offline.pub" && fail 'the two keys came out the same, which should not happen.'

# Each has to sign, and its public key has to verify that, before anything is kept.
printf 'release keys\n' > "$work/message"
"$openssl" pkeyutl -sign -rawin -inkey "$work/ci.pem" -in "$work/message" -out "$work/ci.sig"
"$openssl" pkeyutl -verify -rawin -pubin -inkey "$work/ci.pub" -in "$work/message" -sigfile "$work/ci.sig" >/dev/null
"$openssl" pkeyutl -sign -rawin -inkey "$staged" -passin fd:3 -in "$work/message" -out "$work/offline.sig" 3<<PASS
$pass
PASS
"$openssl" pkeyutl -verify -rawin -pubin -inkey "$work/offline.pub" -in "$work/message" -sigfile "$work/offline.sig" >/dev/null

gh secret set "$secret" --env "$environment" --repo "$repo" < "$work/ci.pem" >/dev/null ||
  fail "gh could not store the CI key in the $environment Environment, so nothing was kept."
mv -f -- "$staged" "$offline"
mkdir -p "$root/assets"
(umask 022 && cp "$work/ci.pub" "$ci_public" && cp "$work/offline.pub" "$offline_public")

printf 'The CI key is in the %s Environment, and nowhere else.\n' "$environment"
printf 'The offline key is at %s\n' "$offline"
printf '  Move it off this machine, and keep its passphrase apart from it. It is what recovers from a lost CI key.\n'
printf 'Commit these two public keys, through a pull request, before the next release is tagged:\n'
printf '  assets/release-signing-key.pub\n  assets/release-signing-key-offline.pub\n'
