#!/usr/bin/env python3
"""Local site editor server: serves the repo on 127.0.0.1 and writes edits to tools/site-edits.json.

    python3 tools/site_editor.py [port]
    open http://127.0.0.1:8792/tools/site-editor.html
"""
import http.server
import json
import os
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "tools", "site-edits.json")


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=ROOT, **kwargs)

    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def do_POST(self):
        if self.path != "/save":
            return self.send_error(404)
        data = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        data["saved_at"] = time.strftime("%Y-%m-%d %H:%M:%S")
        with open(OUT, "w", encoding="utf-8") as f:
            json.dump(data, f, ensure_ascii=False, indent=2)
        body = b'{"ok":true}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8792
    print(f"http://127.0.0.1:{port}/tools/site-editor.html", flush=True)
    http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
