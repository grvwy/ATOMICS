"""Release signatures must authenticate the archive with the installed trust anchor."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


class ReleaseSigningTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="atomics-sign-test-")
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.private = self.directory / "private.pem"
        self.public = self.directory / "public.pem"
        self.archive = self.directory / "archive with spaces.tar.gz"
        self.archive.write_bytes(b"archive bytes to authenticate")
        self.runner_temp = self.directory / "runner-temp"
        self.runner_temp.mkdir()
        self.generate_key(self.private, self.public)

    def generate_key(self, private, public, algorithm="ED25519"):
        subprocess.run(
            ["openssl", "genpkey", "-algorithm", algorithm, "-out", str(private)],
            check=True, capture_output=True,
        )
        subprocess.run(
            ["openssl", "pkey", "-in", str(private), "-pubout", "-out", str(public)],
            check=True, capture_output=True,
        )

    def sign(self, key=None, public=None):
        env = dict(os.environ, RUNNER_TEMP=str(self.runner_temp))
        env.pop("OPENSSL", None)
        env["ATOMICS_RELEASE_SIGNING_KEY"] = self.private.read_text() if key is None else key
        result = subprocess.run(
            ["sh", str(ROOT / "scripts/sign-release.sh"), str(self.archive),
             str(self.public if public is None else public)],
            env=env, capture_output=True, text=True,
        )
        if env["ATOMICS_RELEASE_SIGNING_KEY"]:
            self.assertTrue(
                env["ATOMICS_RELEASE_SIGNING_KEY"] not in result.stdout + result.stderr,
                "signer exposed the private key",
            )
        self.assertEqual(list(self.runner_temp.iterdir()), [], "private-key staging must be removed")
        return result

    def verify(self):
        return subprocess.run(
            ["openssl", "pkeyutl", "-verify", "-rawin", "-pubin", "-inkey", str(self.public),
             "-in", str(self.archive), "-sigfile", str(self.archive) + ".sig"],
            capture_output=True,
        )

    def test_signature_verifies_the_archive_and_rejects_tampering(self):
        result = self.sign()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(Path(str(self.archive) + ".sig").stat().st_size, 64)
        self.assertEqual(self.verify().returncode, 0)
        self.archive.write_bytes(self.archive.read_bytes() + b"tampered")
        self.assertNotEqual(self.verify().returncode, 0)

    def test_mismatched_key_does_not_publish_a_signature(self):
        other_private = self.directory / "other-private.pem"
        self.generate_key(other_private, self.directory / "other-public.pem")
        self.assertNotEqual(self.sign(key=other_private.read_text()).returncode, 0)
        self.assertFalse(Path(str(self.archive) + ".sig").exists())

    def test_missing_secret_has_a_clear_error(self):
        result = self.sign(key="")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("ATOMICS_RELEASE_SIGNING_KEY is required", result.stderr)

    def test_missing_public_key_fails_closed(self):
        result = self.sign(public=self.directory / "missing.pem")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Release public key missing", result.stderr)

    def test_other_key_algorithms_are_rejected(self):
        self.generate_key(self.directory / "x25519-private.pem", self.public, "X25519")
        result = self.sign()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be Ed25519", result.stderr)

    def test_malformed_secret_does_not_publish_a_signature(self):
        result = self.sign(key="invalid-private-key")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(Path(str(self.archive) + ".sig").exists())


if __name__ == "__main__":
    unittest.main()
