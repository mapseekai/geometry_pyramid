#!/usr/bin/env python3
"""Minimal offline tile server for geometry_pyramid.

Serves ST_Tile output at /tiles/{name}/{z}/{x}/{y}.pbf and the static MapLibre
demo page at /. Single-connection, synchronous — for local acceptance only, not
production. Connection comes from DATABASE_URL (or libpq PG* env vars).
"""
import os
import re
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

import psycopg

DEMO_DIR = Path(__file__).resolve().parent
PORT = int(os.environ.get("PORT", "8080"))

# /tiles/<name>/<z>/<x>/<y>.pbf  — name limited to identifier chars.
_TILE_RE = re.compile(r"^/tiles/([A-Za-z0-9_]+)/(\d+)/(\d+)/(\d+)\.pbf$")

# Static files served from DEMO_DIR with their content types.
_STATIC = {
    "/": ("index.html", "text/html; charset=utf-8"),
    "/index.html": ("index.html", "text/html; charset=utf-8"),
    "/maplibre-gl.js": ("maplibre-gl.js", "application/javascript; charset=utf-8"),
    "/maplibre-gl.css": ("maplibre-gl.css", "text/css; charset=utf-8"),
}


def parse_tile_path(path):
    """Return (name, z, x, y) for a valid tile path, else None.

    Pure function (no I/O) so it can be unit-tested without a database.
    """
    m = _TILE_RE.match(path)
    if not m:
        return None
    return (m.group(1), int(m.group(2)), int(m.group(3)), int(m.group(4)))


# Lazily-initialized module connection (set in main(), not at import time).
_CONN = None


def _fetch_tile(name, z, x, y):
    """Run ST_Tile; return bytes (possibly empty) or None on SQL NULL."""
    global _CONN
    try:
        with _CONN.cursor() as cur:
            cur.execute("SELECT ST_Tile(%s, %s, %s, %s)", (name, z, x, y))
            row = cur.fetchone()
    except psycopg.Error:
        # One reconnect attempt, then let errors propagate to the handler (→ 500).
        _CONN = psycopg.connect(os.environ.get("DATABASE_URL", ""), autocommit=True)
        with _CONN.cursor() as cur:
            cur.execute("SELECT ST_Tile(%s, %s, %s, %s)", (name, z, x, y))
            row = cur.fetchone()
    return row[0] if row else None


class Handler(BaseHTTPRequestHandler):
    def _send(self, code, body=b"", ctype="text/plain; charset=utf-8"):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_GET(self):
        path = self.path.split("?", 1)[0]

        if path in _STATIC:
            fname, ctype = _STATIC[path]
            fpath = DEMO_DIR / fname
            if not fpath.exists():
                self._send(404, b"not found")
                return
            self._send(200, fpath.read_bytes(), ctype)
            return

        parsed = parse_tile_path(path)
        if parsed is None:
            self._send(404, b"not found")
            return
        name, z, x, y = parsed
        try:
            mvt = _fetch_tile(name, z, x, y)
        except psycopg.Error as e:
            self._send(500, f"db error: {e}".encode())
            return
        if not mvt:  # None (SQL NULL) or empty bytes → empty tile.
            self._send(204)
            return
        self._send(200, bytes(mvt), "application/vnd.mapbox-vector-tile")

    def log_message(self, fmt, *args):
        sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))


def main():
    global _CONN
    dsn = os.environ.get("DATABASE_URL", "")
    try:
        _CONN = psycopg.connect(dsn, autocommit=True)
        with _CONN.cursor() as cur:
            cur.execute("SELECT 1")
            cur.fetchone()
    except psycopg.Error as e:
        sys.stderr.write(
            f"FATAL: cannot connect to PostgreSQL ({e}).\n"
            f"Set DATABASE_URL (or PGHOST/PGPORT/PGDATABASE) to your pgrx instance.\n"
        )
        sys.exit(1)
    httpd = HTTPServer(("127.0.0.1", PORT), Handler)
    sys.stderr.write(f"tile server on http://127.0.0.1:{PORT}  (Ctrl-C to stop)\n")
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        sys.stderr.write("\nbye\n")


if __name__ == "__main__":
    main()
