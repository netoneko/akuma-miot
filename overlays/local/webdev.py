#!/usr/bin/env python3
"""Serve public/ on localhost and proxy /api/* to a kot node's httpapi.

    python3 overlays/local/webdev.py [--port 8080] [--api http://127.0.0.1:9955]

Same shape as production: the page at the origin root, the API under /api
on the same origin, so the page needs no CORS. Passkeys work on localhost.
"""
import argparse, http.server, os, sys, urllib.error, urllib.request

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..', 'public')

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--port', type=int, default=8080)
    ap.add_argument('--api', default='http://127.0.0.1:9955')
    a = ap.parse_args()

    class H(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *args, **kw):
            super().__init__(*args, directory=ROOT, **kw)

        def proxy(self):
            n = int(self.headers.get('content-length') or 0)
            body = self.rfile.read(n) if n else None
            headers = {k: v for k, v in self.headers.items() if k.lower() not in ('host', 'content-length', 'connection')}
            req = urllib.request.Request(a.api + self.path[4:], data=body, headers=headers, method=self.command)
            try:
                r = urllib.request.urlopen(req, timeout=10)
            except urllib.error.HTTPError as e:
                r = e
            except Exception as e:
                self.send_response(502); self.end_headers(); self.wfile.write(str(e).encode()); return
            out = r.read()
            self.send_response(r.status)
            for k, v in r.headers.items():
                if k.lower() not in ('transfer-encoding', 'connection', 'content-length'):
                    self.send_header(k, v)
            self.send_header('content-length', str(len(out)))
            self.end_headers(); self.wfile.write(out)

        def do_GET(self):
            self.proxy() if self.path.startswith('/api/') else super().do_GET()

        def do_POST(self):
            self.proxy() if self.path.startswith('/api/') else self.send_error(405)

        def end_headers(self):
            if not self.path.startswith('/api/'):
                self.send_header('cache-control', 'no-store')
            super().end_headers()

    print(f'http://localhost:{a.port}  ->  public/ ; /api -> {a.api}', file=sys.stderr)
    http.server.ThreadingHTTPServer(('127.0.0.1', a.port), H).serve_forever()

if __name__ == '__main__':
    main()
