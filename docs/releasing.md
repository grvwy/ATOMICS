# Desktop releases

A `v<version>` tag matching `workspace.package.version` in `Cargo.toml` runs
`.github/workflows/web.yml`. The existing `bundle` and `release` jobs publish the web client.
Independent `desktop` jobs build on GitHub-hosted Ubuntu 22.04 (x86_64) and macOS 15 (arm64),
then `desktop-release` attaches both builds to the release created by the web job.
A missing `ATOMICS_RELEASE_SIGNING_KEY` secret fails the desktop jobs immediately with a setup
message; web publishing continues. Manual web workflow dispatch does not build desktops.

Each desktop target produces three release assets:

```text
atomics-<version>-x86_64-unknown-linux-gnu.tar.gz
atomics-<version>-x86_64-unknown-linux-gnu.tar.gz.sha256
atomics-<version>-x86_64-unknown-linux-gnu.tar.gz.sig
atomics-<version>-aarch64-apple-darwin.tar.gz
atomics-<version>-aarch64-apple-darwin.tar.gz.sha256
atomics-<version>-aarch64-apple-darwin.tar.gz.sig
```

The Linux archive contains `atomics` at its root. The macOS archive contains `ATOMICS.app`,
assembled by `scripts/macos-app.sh`. Linux builds need glibc 2.35 or later and the runtime
libraries corresponding to the dependencies in the workflow. The app bundle keeps the existing
ad-hoc macOS code signature; the release signature below authenticates the download and does
not provide Apple Developer ID signing or notarization.

## Provision the release key once

Use OpenSSL 3 or later. On macOS, install `openssl@3` with Homebrew and put
`$(brew --prefix openssl@3)/bin` first on `PATH`. The signing command uses
[OpenSSL's Ed25519 support](https://docs.openssl.org/3.1/man1/openssl-pkeyutl/#ed25519-and-ed448-algorithms).

The maintainer generates the production key on a trusted machine. Run these commands in the
checkout, with shell tracing disabled; the private key stays in a private directory outside it:

```sh
set +x
umask 077
key_dir=$(mktemp -d)
openssl genpkey -algorithm ED25519 -out "$key_dir/release-private.pem"
openssl pkey -in "$key_dir/release-private.pem" -pubout -out assets/release-signing-key.pub
gh secret set ATOMICS_RELEASE_SIGNING_KEY < "$key_dir/release-private.pem"
```

`ATOMICS_RELEASE_SIGNING_KEY` is the complete **unencrypted PKCS#8 PEM** private key,
including its header, footer and newlines. Keep a protected backup outside the repository;
never log or commit the private key. Only the two desktop steps that need the secret receive it.
The signer uses a mode-0600 temporary file, deletes it on exit, and verifies the signature before
publishing it.

The public trust anchor belongs at **`assets/release-signing-key.pub`**, as a PEM SubjectPublicKeyInfo
Ed25519 public key. Commit it before the first desktop tag and embed it in the app's updater.
The maintainer provisions this file and the matching secret before releasing; desktop signing
fails closed until both exist.
The PEM decodes to a 44-byte DER value: the 12-byte prefix `302a300506032b6570032100` followed
by the 32-byte Ed25519 public key, for verifiers that take raw public keys.
The signer rejects a secret that does not match this public key. Verification must use the key
embedded in the installed app; a key downloaded alongside a release is not a trust anchor.
Changing the signing key requires distributing the new trust anchor to installed apps first.

## Verify and test a release

The `.sha256` file is the standard SHA-256 digest, two spaces and the archive basename.
The `.sig` is a **raw 64-byte Ed25519 signature over the exact tar.gz bytes**, with no prehash,
base64 or signature wrapper. Verify both before extracting or installing an archive:

```sh
version=0.1.7
target=x86_64-unknown-linux-gnu # aarch64-apple-darwin on Apple Silicon
archive="atomics-$version-$target.tar.gz"
gh release download "v$version" --pattern "$archive*" --dir /tmp/atomics-release-check
cd /tmp/atomics-release-check
shasum -a 256 -c "$archive.sha256"
openssl pkeyutl -verify -rawin -pubin -inkey /trusted/checkout/assets/release-signing-key.pub \
  -in "$archive" -sigfile "$archive.sig"
tar -tzf "$archive"
```

Before tagging, check YAML with PyYAML installed:

```sh
python3 -c 'import yaml,sys; yaml.safe_load(open(sys.argv[1]))' .github/workflows/web.yml
sh -n scripts/sign-release.sh
sh -n scripts/macos-app.sh
python3 tests/test_release_signing.py
```

Hosted builds and uploads cannot be proved by a local YAML check. After merging and provisioning
the trust anchor and secret, the release chief must set the intended version in `Cargo.toml` and
`Cargo.lock`, then run `git tag v0.1.7` and `git push origin v0.1.7` for the planned release.
Watch it with `gh run watch <run-id> --exit-status`, and run the download/check commands above
for **both** targets. Confirm that each archive has the documented contents and the web assets
are still present. Exercise missing-secret behavior in a disposable fork with a matching version
tag and no signing secret: both desktop jobs must show the explicit setup error while `bundle`
and `release` succeed. Do not remove the production secret to test this case.
