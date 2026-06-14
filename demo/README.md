# geometry_pyramid demo — HTTP tile server + MapLibre viewer

Minimal, offline, end-to-end acceptance tool. Serves `ST_Tile` output as MVT and
renders it in the browser on a plain background — no internet, no basemap.

> This machine's pgrx pg17 instance listens on port **28817** (host `localhost`,
> database `geometry_pyramid`). Yours may differ — check
> `~/.pgrx/17.log` for the `listening on ... .s.PGSQL.<port>` line, or
> `lsof -nP -iTCP -sTCP:LISTEN | grep postgres`.

## Prerequisites
- The `geometry_pyramid` extension installed into a running PostgreSQL with PostGIS.
- Python 3 with `psycopg`: `pip install 'psycopg[binary]'`
  (on PEP 668 systems: `python3 -m pip install 'psycopg[binary]' --break-system-packages`).

## Run (3 steps)

1. **Start PostgreSQL with the extension.** Using the pgrx-managed instance:
   ```bash
   CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx install --release \
     --pg-config /opt/homebrew/opt/postgresql@17/bin/pg_config
   CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx start pg17
   ```
   Find its port (this machine: 28817): `grep listening ~/.pgrx/17.log | tail -1`.

2. **Seed data + build the demo pyramid:**
   ```bash
   createdb -h localhost -p 28817 geometry_pyramid    # first time only
   psql -h localhost -p 28817 -d geometry_pyramid -f demo/seed.sql
   ```
   The final line prints `z0 bytes = <non-zero>`, confirming the pyramid built.

3. **Start the tile server:**
   ```bash
   DATABASE_URL="host=localhost port=28817 dbname=geometry_pyramid" python3 demo/server.py
   ```
   Open <http://127.0.0.1:8080> .

## What to verify
- The dense cluster renders as red points on the dark background.
- Zoom in past **z=8** (the pyramid's `maxZoom`): the HUD shows `DYNAMIC` and
  points still render — tiles are sliced on demand beyond the materialized range.
- Pan to the sparse outliers (far from the cluster): they render too —
  `splitSize` deferred them at build time; `ST_Tile` slices them on demand.
- The server terminal logs `.pbf` requests with `200` (has data) / `204` (empty).

## Verified end-to-end (curl)
With the demo pyramid built (`maxZoom=8`, `splitSize=500`):

| Request | Result |
|---|---|
| `GET /` | `200 text/html` |
| `GET /tiles/demo/0/0/0.pbf` (materialized) | `200`, ~56 KB MVT |
| `GET /tiles/demo/12/3373/1550.pbf` (z>maxZoom) | `200`, ~8.7 KB MVT — **0 rows in storage, sliced on demand** |
| `GET /tiles/demo/3/0/0.pbf` (empty region) | `204` |
| `GET /tiles/foo;bar/0/0/0.pbf` (bad name) | `404` |
| `GET /maplibre-gl.js` | `200 application/javascript` |

Only 9 tiles were materialized for the whole dataset (the dense cluster at
z ≤ 8 meeting `splitSize`); everything else is served dynamically.

## Endpoint
`GET /tiles/{name}/{z}/{x}/{y}.pbf` → MVT (`200`), empty tile (`204`),
bad/unknown path (`404`), DB error (`500`).

## Notes
- Single-connection, synchronous — acceptance/demo only, not production.
- Offline: MapLibre assets are vendored locally (`maplibre-gl.js` / `maplibre-gl.css`).
- The page requests tiles up to z=22 so zooming past `maxZoom` exercises the
  on-demand path; `source-layer` is the pyramid name (`demo`).
