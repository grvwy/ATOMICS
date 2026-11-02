#!/usr/bin/env bash
# Serves www/ on 0.0.0.0:${PORT:-8080}. Open http://<tailnet-ip>:8080/ on the phone.
cd "$(dirname "$0")/www" && exec python3 -m http.server "${PORT:-8080}" --bind 0.0.0.0
