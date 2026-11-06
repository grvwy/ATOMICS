"""Exercise cidata parsing, mounts, executable links and the generated guest-check environment."""
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(sys.argv.pop(1)).resolve()


class MacSharesTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.log = self.root / 'calls'
        self.output = self.root / 'desktop.env'
        self.cidata = self.root / 'cidata'
        self.cidata.mkdir()
        for command in ('install', 'mkdir', 'ln', 'mount', 'mountpoint'):
            stub = self.bin / command
            stub.write_text('#!/bin/sh\nprintf "%s\\n" "$(basename "$0") $*" >> "$CALLS"\n'
                            + ('exit "${MOUNTED:-1}"\n' if command == 'mountpoint' else 'exit 0\n'))
            stub.chmod(0o755)
        self.settings = {
            'dynamic': 'true', 'cidata': str(self.cidata), 'mac_home': '/Users/@ATOMICS_MAC_USER@',
            'socket': '/Users/@ATOMICS_MAC_USER@/.atomics/atomics.sock',
            'executables': '/Users/@ATOMICS_MAC_USER@/Applications/ATOMICS.app/Contents/MacOS/atomics '
                           '/Applications/ATOMICS.app/Contents/MacOS/atomics',
            'shares': '', 'owner': 'atomics', 'group': 'users', 'guest_home': '/home/atomics',
            'binary': '/run/current-system/sw/bin/atomics', 'host_port': '47800', 'output': str(self.output),
        }
        self.params('alice')
        self.mounts()

    def params(self, user):
        (self.cidata / 'param.env').write_text(f'PARAM_UNRELATED=ignored\nPARAM_ATOMICS_MAC_USER={user}\n')

    def mounts(self, user='alice', extra=''):
        (self.cidata / 'user-data').write_text(
            '#cloud-config\nmounts:\n'
            '- [vz-rosetta, /mnt/lima-rosetta, virtiofs, defaults, "0", "0"]\n'
            f'- [mount-tag-1, /Users/{user}/repos, virtiofs, "rw,nosuid,nodev", "0", "0"]\n'
            f'- [mount-tag-2, /Users/{user}/.atomics/briefs, virtiofs, "ro,nosuid,nodev", "0", "0"]\n'
            + extra)

    def run_script(self, **env):
        config = self.root / 'config'
        config.write_text('\n'.join(f'{key}={shlex.quote(value)}' for key, value in self.settings.items()))
        return subprocess.run(['bash', str(SCRIPT), str(config)], text=True, capture_output=True,
                              env=dict(os.environ, PATH=f'{self.bin}:{os.environ["PATH"]}', CALLS=str(self.log), **env))

    def test_runtime_identity_mount_modes_and_mac_links(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        log = self.log.read_text()
        self.assertIn('mount -t virtiofs -o rw,nosuid,nodev mount-tag-1 /Users/alice/repos', log)
        self.assertIn('mount -t virtiofs -o ro,nosuid,nodev mount-tag-2 /Users/alice/.atomics/briefs', log)
        self.assertIn('ln -sfn /run/current-system/sw/bin/atomics /Users/alice/Applications/ATOMICS.app/Contents/MacOS/atomics', log)
        self.assertIn('ln -sfn /run/current-system/sw/bin/atomics /Applications/ATOMICS.app/Contents/MacOS/atomics', log)
        self.assertIn('install -d -m 0700 -o atomics -g users /Users/alice/.atomics', log)
        self.assertNotIn('vz-rosetta', log)
        self.assertIn('ATOMICS_HOST_FOLDER=/Users/alice/repos', self.output.read_text())
        self.assertNotIn('@ATOMICS_', self.output.read_text())

    def test_custom_share_mount_point_comes_from_cidata(self):
        self.mounts(extra='- [external, /Volumes/work, virtiofs, "rw", "0", "0"]\n')
        self.assertEqual(self.run_script().returncode, 0)
        self.assertIn('external /Volumes/work', self.log.read_text())
        self.assertIn('/Volumes/work:rw', self.output.read_text())

    def test_second_boot_keeps_existing_mounts(self):
        self.assertEqual(self.run_script(MOUNTED='0').returncode, 0)
        self.assertNotIn('mount -t', self.log.read_text())
        self.assertTrue(self.output.exists())

    def test_build_time_identity_works_without_param_env(self):
        self.settings = {key: value.replace('@ATOMICS_MAC_USER@', 'alice') for key, value in self.settings.items()}
        self.settings.update(dynamic='false', shares='/Users/alice/repos:rw /Users/alice/.atomics/briefs:ro')
        (self.cidata / 'param.env').unlink()
        self.assertEqual(self.run_script().returncode, 0)
        self.assertIn('/Users/alice/repos', self.output.read_text())

    def test_missing_or_malicious_identity_is_never_evaluated(self):
        for value in ('', '../alice', 'alice/other', '$(touch /tmp/never-run)', 'alice\nPARAM_ATOMICS_MAC_USER=alice'):
            self.params(value)
            self.assertNotEqual(self.run_script().returncode, 0, value)
            self.assertFalse(self.log.exists())
        (self.cidata / 'param.env').unlink()
        self.assertNotEqual(self.run_script().returncode, 0)

    def test_bad_mount_paths_fail_before_any_mutation(self):
        for path in ('/etc', '/Users/alice/../root', '/Users/alice/repos;false', '/Users/alice'):
            self.mounts(extra=f'- [bad, {path}, virtiofs, "rw", "0", "0"]\n')
            self.assertNotEqual(self.run_script().returncode, 0, path)
            self.assertFalse(self.log.exists())

    def test_links_inside_shares_and_missing_mount_tags_are_refused(self):
        self.settings['executables'] = '/Users/@ATOMICS_MAC_USER@/repos/atomics'
        self.assertNotEqual(self.run_script().returncode, 0)
        self.assertFalse(self.log.exists())
        self.settings.update(dynamic='false', mac_home='/Users/alice', socket='/Users/alice/.atomics/atomics.sock',
                             executables='/Applications/ATOMICS.app/Contents/MacOS/atomics', shares='/Users/alice/missing:rw')
        self.assertNotEqual(self.run_script().returncode, 0)
        self.assertFalse(self.output.exists())


unittest.main()
