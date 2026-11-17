#!/usr/bin/env python3
"""Minimal Gitea/Forgejo API stub for testing scripts/import-forgejo.sh.

Serves one owner ("stubby") with one repository ("stubrepo") from JSON files
in a fixture directory. Only the endpoints the import script reads exist;
everything else answers []. Run: forgejo-stub.py PORT FIXTURE_DIR
"""
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

ROUTES = {
    "/api/v1/users/stubby/repos": "repos.json",
    "/api/v1/orgs/stubby/repos": "repos.json",
    "/api/v1/repos/stubby/stubrepo/labels": "labels.json",
    "/api/v1/repos/stubby/stubrepo/milestones": "milestones.json",
    "/api/v1/repos/stubby/stubrepo/issues": "issues.json",
    "/api/v1/repos/stubby/stubrepo/releases": "releases.json",
}


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        path = self.path.split("?")[0]
        name = ROUTES.get(path)
        body = (Path(FIXTURE) / name).read_bytes() if name and (Path(FIXTURE) / name).exists() else b"[]"
        if "/issues/" in path and path.endswith("/comments"):
            n = path.rstrip("/").rsplit("/", 2)[-2]
            f = Path(FIXTURE) / f"comments-{n}.json"
            body = f.read_bytes() if f.exists() else b"[]"
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


FIXTURE = sys.argv[2] if len(sys.argv) > 2 else "."
HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
