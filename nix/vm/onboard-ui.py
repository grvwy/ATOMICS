#!/usr/bin/env python3
"""Guest-only bridge for one onboarding step. Raw tool output and credential files never
cross SSH. Only browser URLs, short device codes and fixed statuses leave this process.
No output is logged, and input goes straight to the tool's PTY with echo disabled.
"""
import errno
import fcntl
import json
import os
import pty
import re
import select
import signal
import subprocess
import sys
import termios
from urllib.parse import parse_qsl, urlsplit

ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07]*(?:\x07|\x1b\\)")
URL = re.compile(r"https://[^\s<>\"'\x00-\x1f]+")
CODE = re.compile(r"(?<![A-Za-z0-9])(?:[A-Z0-9]{3,6}-[A-Z0-9]{3,6}|[A-Z0-9]{6,12})(?![A-Za-z0-9])")
SECRET = re.compile(r"(?:access.?token|refresh.?token|id.?token|password|secret|api.?key|bearer|oauth_token)", re.I)


def safe_url(value):
    if len(value) > 4096 or any(ord(c) < 32 for c in value):
        return False
    try:
        url = urlsplit(value)
        return (url.scheme == "https" and bool(url.hostname) and not url.username
                and not url.password and not url.fragment
                and not any(SECRET.search(key) or key.lower() in {"token", "code", "code_verifier"}
                            for key, _ in parse_qsl(url.query)))
    except ValueError:
        return False


def events(text):
    text = ANSI.sub("", text)
    code_context = 0
    for line in text.splitlines():
        if SECRET.search(line):
            continue
        for match in URL.finditer(line):
            address = match.group().rstrip(".,);]")
            if safe_url(address):
                yield {"url": address}
        # A URL's state, challenge or other query data is never a displayed device code.
        without_urls = URL.sub("", line)
        if re.search(r"(?:one.time|device|enter|copy|code)", without_urls, re.I):
            code_context = 4
        else:
            code_context = max(0, code_context - 1)
        if code_context:
            for match in CODE.finditer(without_urls):
                if match.group() not in {"ENTER", "BROWSER", "DEVICE"}:
                    yield {"code": match.group()}


def main():
    master, slave = pty.openpty()
    attrs = termios.tcgetattr(slave)
    attrs[3] &= ~termios.ECHO
    termios.tcsetattr(slave, termios.TCSANOW, attrs)
    env = dict(os.environ, BROWSER="true", GH_BROWSER="true", NO_COLOR="1", TERM="dumb")
    def terminal_session():
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

    child = subprocess.Popen(sys.argv[1:], stdin=slave, stdout=slave, stderr=slave,
                             env=env, preexec_fn=terminal_session)
    os.close(slave)
    seen = set()
    window = ""
    incoming = b""

    def emit(event):
        encoded = json.dumps(event)
        if encoded not in seen:
            print(encoded, flush=True)
            seen.add(encoded)

    def stop(*_):
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        raise SystemExit(130)

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGHUP, stop)
    emit({"status": "waiting"})
    try:
        while child.poll() is None:
            ready, _, _ = select.select([master, sys.stdin.buffer], [], [], 0.2)
            if sys.stdin.buffer in ready:
                data = os.read(sys.stdin.fileno(), 4096)
                if not data:
                    stop()
                incoming = (incoming + data)[-8192:]
                while b"\n" in incoming:
                    line, incoming = incoming.split(b"\n", 1)
                    try:
                        code = json.loads(line).get("code", "")
                    except (ValueError, AttributeError):
                        continue
                    if isinstance(code, str) and re.fullmatch(r"[A-Za-z0-9_.~#/-]{0,256}", code):
                        os.write(master, code.encode() + b"\n")
            if master in ready:
                try:
                    data = os.read(master, 4096)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not data:
                    break
                window = (window + data.decode("utf-8", errors="replace"))[-8192:]
                complete_lines = window.rsplit("\n", 1)[0] + "\n" if "\n" in window else ""
                for event in events(complete_lines):
                    emit(event)
                if re.search(r"press (?:enter|return)", ANSI.sub("", window), re.I):
                    os.write(master, b"\n")
                    window = ""
        status = child.wait()
        emit({"status": "complete" if status == 0 else "failed"})
        return status
    finally:
        os.close(master)
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            child.wait(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait()


if __name__ == "__main__":
    sys.exit(main())
