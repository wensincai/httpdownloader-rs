"""Range-capable test HTTP server for hd-cli multi-part download tests."""
import os
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

FILE = sys.argv[2] if len(sys.argv) > 2 else None
PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8766
DELAY = float(sys.argv[3]) if len(sys.argv) > 3 else 0.0


class Handler(BaseHTTPRequestHandler):
    def do_HEAD(self):
        size = os.path.getsize(FILE)
        self.send_response(200)
        self.send_header("Content-Length", str(size))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()

    def do_GET(self):
        size = os.path.getsize(FILE)
        rng = self.headers.get("Range")
        if rng:
            m = re.match(r"bytes=(\d+)-(\d*)", rng)
            start = int(m.group(1))
            end = int(m.group(2)) + 1 if m.group(2) else size
            end = min(end, size)
            self.send_response(206)
            self.send_header("Content-Length", str(end - start))
            self.send_header("Content-Range", f"bytes {start}-{end-1}/{size}")
            self.end_headers()
            with open(FILE, "rb") as f:
                f.seek(start)
                remaining = end - start
                while remaining > 0:
                    chunk = f.read(min(64 * 1024, remaining))
                    if not chunk:
                        break
                    self.wfile.write(chunk)
                    time.sleep(DELAY)
                    remaining -= len(chunk)
        else:
            self.send_response(200)
            self.send_header("Content-Length", str(size))
            self.end_headers()
            with open(FILE, "rb") as f:
                while True:
                    chunk = f.read(64 * 1024)
                    if not chunk:
                        break
                    self.wfile.write(chunk)
                    time.sleep(DELAY)

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
