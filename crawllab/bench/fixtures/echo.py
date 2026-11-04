"""Two-origin test site that logs the identity headers of every request.

Port 8000 serves the page; port 8001 is the cross-origin side (iframe, API
with a CORS preflight). Each request prints one JSON line to stdout.
"""
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PAGE = b"""<!doctype html><html><head>
<link rel="stylesheet" href="/s.css">
<script src="/a.js"></script>
<script type="module" src="/m.js"></script>
</head><body><p>echo</p>
<iframe id="s" srcdoc="<p>inner</p>"></iframe>
<iframe id="x" src="http://echo:8001/frame.html"></iframe>
<script>
navigator.userAgentData.getHighEntropyValues(['architecture','fullVersionList','uaFullVersion','platformVersion'])
  .then(v => { window.__hev = v; });
fetch('http://echo:8001/api', {method: 'POST', headers: {'X-Test': '1', 'Content-Type': 'application/json'}, body: '{}'})
  .then(r => r.text()).then(t => { window.__fetchResult = t; }, e => { window.__fetchResult = 'error ' + e; });
</script></body></html>"""

FILES = {
    "/": (PAGE, "text/html"),
    "/a.js": (b"window.__a = 1;", "text/javascript"),
    "/m.js": (b"import './m2.js'; window.__m = 1;", "text/javascript"),
    "/m2.js": (b"window.__m2 = 1;", "text/javascript"),
    "/s.css": (b"p { color: rgb(1, 2, 3); }", "text/css"),
    "/frame.html": (b"<!doctype html><p>cross</p><script src='/f.js'></script>", "text/html"),
    "/f.js": (b"window.__f = 1;", "text/javascript"),
    "/robots.txt": (b"User-agent: *\nAllow: /\n", "text/plain"),
    "/api": (b'{"ok": true}', "application/json"),
}

LOCK = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _log(self):
        h = {k.lower(): v for k, v in self.headers.items()}
        line = {
            "port": self.server.server_port,
            "method": self.command,
            "path": self.path,
            "ua": h.get("user-agent"),
            "accept-language": h.get("accept-language"),
            "sec-ch-ua": h.get("sec-ch-ua"),
            "sec-ch-ua-platform": h.get("sec-ch-ua-platform"),
        }
        with LOCK:
            print(json.dumps(line), flush=True)

    def _cors(self):
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "POST")
        self.send_header("Access-Control-Allow-Headers", "x-test, content-type")

    def do_OPTIONS(self):
        self._log()
        self.send_response(204)
        self._cors()
        self.end_headers()

    def _refresh_routes(self):
        """Anubis-like metarefresh: a cookie, a 2 s refresh, and a pass check."""
        import time
        path = self.path.split("?")[0]
        if path in ("/rh", "/rm"):
            body = b"<!doctype html><title>Challenge</title>"
            if path == "/rm":
                body += b'<meta http-equiv="refresh" content="2; url=/pass">'
            self.send_response(200)
            if path == "/rh":
                self.send_header("Refresh", "2; url=/pass")
            self.send_header("Set-Cookie", f"issued={time.time():.3f}; Path=/")
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return True
        if path == "/pass":
            cookies = dict(c.strip().split("=", 1) for c in (self.headers.get("Cookie") or "").split(";") if "=" in c)
            waited = time.time() - float(cookies.get("issued", "inf").replace("inf", "1e18"))
            if "issued" not in cookies or waited < 0.8:
                self.send_response(403); self.send_header("Content-Length", "0"); self.end_headers()
                return True
            self.send_response(302); self.send_header("Location", "/content"); self.send_header("Content-Length", "0"); self.end_headers()
            return True
        if path == "/content":
            body = b"<!doctype html><title>Real content</title><p>passed</p>"
            self.send_response(200); self.send_header("Content-Type", "text/html"); self.send_header("Content-Length", str(len(body))); self.end_headers()
            self.wfile.write(body)
            return True
        if path == "/rgone":
            body = b"<!doctype html><title>Moving</title>"
            self.send_response(200); self.send_header("Refresh", "1; url=/gone"); self.send_header("Content-Type", "text/html"); self.send_header("Content-Length", str(len(body))); self.end_headers()
            self.wfile.write(body)
            return True
        if path == "/gone":
            body = b"<!doctype html><title>Gone</title><p>404</p>"
            self.send_response(404); self.send_header("X-Doc", "gone"); self.send_header("Content-Type", "text/html"); self.send_header("Content-Length", str(len(body))); self.end_headers()
            self.wfile.write(body)
            return True
        if path == "/loop":
            body = b'<!doctype html><title>Loop</title><meta http-equiv="refresh" content="0">'
            self.send_response(200); self.send_header("Content-Type", "text/html"); self.send_header("Content-Length", str(len(body))); self.end_headers()
            self.wfile.write(body)
            return True
        return False

    def do_GET(self):
        self._log()
        if self._refresh_routes():
            return
        path = self.path.split("?")[0]
        if path == "/link":
            # A page linking to the URL in ?to=, for crawls that leave the host.
            from html import escape
            from urllib.parse import parse_qs, urlsplit
            to = parse_qs(urlsplit(self.path).query).get("to", ["/content"])[0]
            body = f'<!doctype html><title>Link</title><a href="{escape(to)}">next</a>'.encode()
            self.send_response(200); self.send_header("Content-Type", "text/html"); self.send_header("Content-Length", str(len(body))); self.end_headers()
            self.wfile.write(body)
            return
        if path in ("/hk", "/hk2"):
            # /hk sets an HttpOnly cookie and a script-visible one and links on;
            # /hk2 shows what its scripts can read in the title.
            if path == "/hk":
                body = b'<!doctype html><title>Cookies set</title><a href="/hk2">next</a>'
            else:
                body = b'<!doctype html><title>none</title><script>document.title = "seen:" + document.cookie;</script>'
            self.send_response(200)
            if path == "/hk":
                self.send_header("Set-Cookie", "hk=1; HttpOnly; Path=/")
                self.send_header("Set-Cookie", "vis=1; Path=/")
            self.send_header("Content-Type", "text/html"); self.send_header("Content-Length", str(len(body))); self.end_headers()
            self.wfile.write(body)
            return
        # Any other path is "the protected app": a page that names the path.
        default = (f"<!doctype html><title>Backend {path}</title><p>backend reached: {path}</p>".encode(), "text/html")
        body, ctype = FILES.get(path, default)
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self._cors()
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        self._log()
        length = int(self.headers.get("Content-Length") or 0)
        self.rfile.read(length)
        body = FILES["/api"][0]
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self._cors()
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    servers = [ThreadingHTTPServer(("0.0.0.0", port), Handler) for port in (8000, 8001)]
    for server in servers[1:]:
        threading.Thread(target=server.serve_forever, daemon=True).start()
    print(json.dumps({"ready": True}), flush=True)
    sys.stdout.flush()
    servers[0].serve_forever()
