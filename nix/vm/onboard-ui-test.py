#!/usr/bin/env python3
"""The UI bridge never forwards raw tool output, tokens or credential-bearing URLs."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest

bridge_path = Path(os.environ.get("ATOMICS_ONBOARD_UI_BRIDGE", Path(__file__).with_name("onboard-ui.py")))
spec = importlib.util.spec_from_file_location("bridge", bridge_path)
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


class BridgeTests(unittest.TestCase):
    def test_only_addresses_and_device_codes_leave_the_guest(self):
        canary = "secret-canary-123456789"
        text = ("First copy your one-time code: ABCD-1234\n"
                "Open https://example.com/device in your browser\n"
                f"Token: {canary}\naccess_token={canary}\n"
                f"https://example.com/?access_token={canary}\n"
                f"https://user:{canary}@example.com/device\n"
                f"https://example.com/#token={canary}\n")
        events = list(bridge.events(text))
        self.assertEqual(events, [{"code": "ABCD-1234"}, {"url": "https://example.com/device"}])
        self.assertNotIn(canary, json.dumps(events))

    def test_an_authorization_challenge_is_an_address_not_a_device_code(self):
        address = "https://example.com/authorize?state=ABCD1234&code_challenge=EFGH5678"
        self.assertEqual(list(bridge.events("Open " + address + "\n")), [{"url": address}])
        for bad in ["http://example.com", "file:///etc/passwd", "javascript:alert(1)", "https://example.com/?password=x"]:
            self.assertFalse(bridge.safe_url(bad))

    def test_real_bridge_with_fake_tool_never_sends_its_secret(self):
        with tempfile.TemporaryDirectory() as work:
            tool = Path(work) / "onboard"
            tool.write_text("#!/bin/sh\nprintf 'Open https://example.com/device\\nFirst copy your one-time code: ABCD-1234\\nToken: canary-private-value\\n'\nread answer\n[ \"$answer\" = one-time-code ]\n")
            tool.chmod(0o755)
            process = subprocess.Popen([sys.executable, str(bridge_path), str(tool)],
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            timeout = threading.Timer(5, process.kill)
            timeout.start()
            messages = []
            try:
                for line in process.stdout:
                    event = json.loads(line)
                    messages.append(event)
                    if "code" in event:
                        process.stdin.write('{"code":"one-time-code"}\n')
                        process.stdin.flush()
                    if event.get("status") == "complete":
                        break
                process.wait(timeout=5)
                stderr = process.stderr.read()
                self.assertEqual(process.returncode, 0, stderr)
                self.assertIn({"status": "complete"}, messages)
                self.assertIn({"url": "https://example.com/device"}, messages)
                self.assertIn({"code": "ABCD-1234"}, messages)
                self.assertNotIn("canary-private-value", json.dumps(messages) + stderr)
                self.assertNotIn("one-time-code", json.dumps(messages) + stderr)
            finally:
                timeout.cancel()
                if process.poll() is None:
                    process.kill()
                    process.wait()
                process.stdin.close()
                process.stdout.close()
                process.stderr.close()

    def test_device_codes_on_the_following_line(self):
        self.assertEqual(list(bridge.events("Enter this one-time code:\n\nABCD-EFGH\n")), [{"code": "ABCD-EFGH"}])



if __name__ == "__main__":
    unittest.main()
