#!/usr/bin/env python3
"""Serve the built client with the isolation headers required by FRB shared Wasm memory."""
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

class Handler(SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header('Cross-Origin-Opener-Policy', 'same-origin')
        self.send_header('Cross-Origin-Embedder-Policy', 'require-corp')
        self.send_header('Cache-Control', 'no-cache')
        super().end_headers()

root = Path(__file__).resolve().parents[1] / 'client' / 'build' / 'web'
print(f'RyzHub: http://127.0.0.1:3000 ({root})', flush=True)
ThreadingHTTPServer(('127.0.0.1', 3000), partial(Handler, directory=str(root))).serve_forever()
