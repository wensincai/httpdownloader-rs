"""M2 integration test server: Digest/Basic auth + cookie gate."""
import hashlib
import os
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8767
USER, PASS = "hduser", "hdpass"
BODY = b"AUTH-OK-CONTENT-0123456789" * 100


class Handler(BaseHTTPRequestHandler):
    def send_body(self, code=200, extra=None):
        self.send_response(code)
        self.send_header("Content-Length", str(len(BODY)))
        for k, v in (extra or []):
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(BODY)

    def do_HEAD(self):
        # same gate as GET without body
        self.do_GET_head_only()

    def do_GET_head_only(self):
        path = self.path
        if path.startswith("/basic"):
            auth = self.headers.get("Authorization", "")
            expected = "Basic " + __import__("base64").b64encode(f"{USER}:{PASS}".encode()).decode()
            code = 200 if auth == expected else 401
        elif path.startswith("/digest"):
            auth = self.headers.get("Authorization", "")
            code = 200 if (auth.startswith("Digest ") and f'username="{USER}"' in auth) else 401
        elif path.startswith("/cookie"):
            auth = self.headers.get("Authorization", "")
            code = 200 if (auth.startswith("Digest ") and f'username="{USER}"' in auth) else 401
        else:
            code = 404
        self.send_response(code)
        if code == 200:
            self.send_header("Content-Length", str(len(BODY)))
            self.send_header("Accept-Ranges", "bytes")
        elif code == 401:
            nonce = hashlib.md5(os.urandom(8)).hexdigest()
            if path.startswith("/basic"):
                self.send_header("WWW-Authenticate", 'Basic realm="hd-test"')
            else:
                self.send_header(
                    "WWW-Authenticate",
                    f'Digest realm="hd-test", nonce="{nonce}", qop="auth", algorithm=MD5',
                )
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        if self.path.startswith("/basic"):
            auth = self.headers.get("Authorization", "")
            expected = "Basic " + __import__("base64").b64encode(f"{USER}:{PASS}".encode()).decode()
            if auth == expected:
                self.send_body()
            else:
                self.send_response(401)
                self.send_header("WWW-Authenticate", 'Basic realm="hd-test"')
                self.send_header("Content-Length", "0")
                self.end_headers()
        elif self.path.startswith("/digest"):
            auth = self.headers.get("Authorization", "")
            if auth.startswith("Digest ") and f'username="{USER}"' in auth:
                self.send_body()
            else:
                nonce = hashlib.md5(os.urandom(8)).hexdigest()
                self.send_response(401)
                self.send_header(
                    "WWW-Authenticate",
                    f'Digest realm="hd-test", nonce="{nonce}", qop="auth", algorithm=MD5',
                )
                self.send_header("Content-Length", "0")
                self.end_headers()
        elif self.path.startswith("/cookie"):
            auth = self.headers.get("Authorization", "")
            if auth.startswith("Digest ") and f'username="{USER}"' in auth:
                self.send_body()
            else:
                nonce = hashlib.md5(os.urandom(8)).hexdigest()
                self.send_response(401)
                self.send_header(
                    "WWW-Authenticate",
                    f'Digest realm="hd-test", nonce="{nonce}", qop="auth", algorithm=MD5',
                )
                self.send_header("Set-Cookie", "session=abcd1234; Path=/")
                self.send_header("Content-Length", "0")
                self.end_headers()
        else:
            self.send_body(404)

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
