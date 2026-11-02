#!/usr/bin/env python3
"""Serves www/ for the phone test and saves logs the page sends (POST /log) to logs/.

HOST defaults to 127.0.0.1, for `tailscale serve` in front (HTTPS). Set HOST to the tailnet IP for plain
http instead. Never bind a public interface. PORT defaults to 8080. Nothing is cached, so a rebuild shows
on reload, in the installed PWA too.
"""
import datetime
import http.server
import os
import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parent
LOGS = ROOT / "logs"
MAX_LOG_BYTES = 4 << 20


class Handler(http.server.SimpleHTTPRequestHandler):
    extensions_map = {
        **http.server.SimpleHTTPRequestHandler.extensions_map,
        ".js": "text/javascript",
        ".wasm": "application/wasm",
        ".webmanifest": "application/manifest+json",
    }

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(ROOT / "www"), **kwargs)

    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def do_POST(self):
        if self.path.split("?")[0] != "/log":
            return self.send_error(404)
        length = int(self.headers.get("Content-Length") or 0)
        if not 0 < length <= MAX_LOG_BYTES:
            return self.send_error(413)
        body = self.rfile.read(length)
        tag = re.sub(r"[^a-z0-9-]", "", (self.headers.get("X-Spike-Tag") or "").lower())[:40] or "log"
        LOGS.mkdir(exist_ok=True)
        name = f"{datetime.datetime.now():%Y%m%d-%H%M%S}-{tag}.txt"
        (LOGS / name).write_bytes(body)
        reply = name.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)


if __name__ == "__main__":
    addr = (os.environ.get("HOST", "127.0.0.1"), int(os.environ.get("PORT", "8080")))
    print(f"serving {ROOT / 'www'} on http://{addr[0]}:{addr[1]}/", flush=True)
    http.server.ThreadingHTTPServer(addr, Handler).serve_forever()
