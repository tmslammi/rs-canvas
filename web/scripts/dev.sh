#!/usr/bin/env bash
# Serve the app as-is: no bundler, no transpiler, plain ES modules.
#
# Cache-Control: no-store, because a stale main.js or wasm in the browser
# looks exactly like "the new feature isn't there" — reloads then appear to
# do nothing until a hard refresh.
set -euo pipefail
cd "$(dirname "$0")/.."

bash scripts/watch-wasm.sh &
WATCH_PID=$!
trap 'kill "$WATCH_PID" 2>/dev/null || true' EXIT

echo "rs-canvas: http://localhost:5173 (plain files; reload after Rust changes)"
python3 - <<'PY'
import http.server
import functools


class NoStoreHandler(http.server.SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header("Cache-Control", "no-store, max-age=0")
        super().end_headers()


handler = functools.partial(NoStoreHandler, directory=".")
http.server.ThreadingHTTPServer(("127.0.0.1", 5173), handler).serve_forever()
PY
