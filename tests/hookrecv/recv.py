#!/usr/bin/env python3
"""Webhook receiver for tests/m6.sh.

Usage: recv.py <port> <outdir>

Each POST is written to <outdir>/<seq>.json as {"path", "headers", "body"}.
Requests to a path starting with /fail answer 500, so retry/backoff can be
tested. Everything else answers 200.
"""
import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

port, outdir = int(sys.argv[1]), sys.argv[2]
os.makedirs(outdir, exist_ok=True)

lock = threading.Lock()
seq = [0]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length).decode("utf-8", "replace")
        if self.path.startswith("/fail"):
            self._reply(500, b"nope")
            return
        with lock:
            seq[0] += 1
            name = "%08d.json" % seq[0]
        with open(os.path.join(outdir, name), "w") as f:
            json.dump({"path": self.path, "headers": dict(self.headers), "body": body}, f)
        self._reply(200, b"ok")

    def do_GET(self):
        self._reply(200, b"recv")

    def _reply(self, code, payload):
        self.send_response(code)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
