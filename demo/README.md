# Local Tile Server & Map Viewer Operations Guide

<p align="center">
  <a href="README.md">English</a> •
  <a href="README.zh-CN.md">简体中文</a> •
  <a href="../README.md">Back to Project Root</a>
</p>

This guide provides end-to-end operational instructions for launching a local vector tile HTTP server and interacting with the MapLibre GL web map viewer. It serves as an acceptance and verification harness for `ST_BuildPyramid` materialized tiles, `splitSize` sparse tile fallback, and live dynamic slicing beyond `maxZoom`.

---

## 📋 Architecture Overview

- **Backend**: Lightweight Python 3 HTTP server powered by `psycopg` (synchronous single-connection design intended exclusively for local inspection and acceptance testing).
- **Frontend**: MapLibre GL JS & CSS bundled locally for fully offline capability; includes an optional OpenStreetMap basemap toggle.
- **Data Generator**: `demo/seed.sql` populates random spatial point data into `demo_src` (centered around Beijing) and builds a pyramid named `demo`.

---

## 🚀 Step-by-Step Operations

### Step 1: Launch PostgreSQL Instance

From the repository root, start the pgrx-managed PostgreSQL instance (PostgreSQL 17 by default):

```bash
cargo pgrx run pg17
```

In the opened `psql` interactive terminal, create a dedicated disposable database:

```sql
CREATE DATABASE gp_demo;
\connect gp_demo
\conninfo
\q
```

Note the port printed by `\conninfo` (e.g., `28817` or your system's active port).

---

### Step 2: Seed Test Data & Build Pyramid

In a new shell terminal, set `DATABASE_URL` and run `demo/seed.sql`:

```bash
# Replace 28817 with your actual PostgreSQL port from step 1
export DATABASE_URL="host=localhost port=28817 dbname=gp_demo"

# Execute seed script
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f demo/seed.sql
```

**Expected Output**:
The final query prints a non-zero byte size for the root tile (zoom 0):
```text
 z0 bytes 
----------
      142
(1 row)
```

> **Note**: `demo/seed.sql` is idempotent. Re-running it safely drops and recreates both the `demo` pyramid and `demo_src` source table.

---

### Step 3: Install Dependencies & Start Server

Set up an isolated virtual environment and run the HTTP server:

```bash
# 1. Create and activate virtual environment
python3 -m venv /tmp/gp-demo-venv
source /tmp/gp-demo-venv/bin/activate

# 2. Install psycopg driver
pip install 'psycopg[binary]'

# 3. Launch the server (default port 8989)
python demo/server.py
```

To bind to a different port, set the `PORT` environment variable (e.g., `export PORT=9000`).

---

### Step 4: Verification & Acceptance via Browser

Open [http://127.0.0.1:8989](http://127.0.0.1:8989) in your web browser.

#### Verification Checklist:
1. **Initial Rendering**: Map centers around 116.49°E, 40°N displaying a dense cluster of red points.
2. **Materialized Tiles (Zooms 0 - 8)**: Navigating zooms 0 to 8 fetches pre-materialized MVT bytes from the database in milliseconds.
3. **Live Dynamic Slicing (> Zoom 8)**: Zooming in past zoom level 8 exercises on-the-fly slicing from the source table.
4. **Sparse Tile Slicing (`splitSize`)**: Panning toward distant isolated points tests sparse live slicing fallback.

---

### Step 5: Automated Testing via CLI

In another shell, test endpoints with `curl` or run the integration test suite:

```bash
# Check HTML page status
curl -sS -o /dev/null -w "Status: %{http_code}\n" http://127.0.0.1:8989/

# Check tile endpoint (z=0, x=0, y=0)
curl -sS -o /dev/null -w "Status: %{http_code}\n" http://127.0.0.1:8989/tiles/demo/0/0/0.pbf

# Run integration test harness
python demo/test_server.py
```

**HTTP Status Codes**:
- `200 OK`: Valid MVT byte stream returned.
- `204 No Content`: Empty tile (no geometry features in bounds).
- `404 Not Found`: Invalid URL path or malformed parameters.
- `500 Internal Server Error`: Database query error or connection failure.

---

## 🧹 Cleaning Up Demo Resources

To remove the demo pyramid and source table while preserving the database:

```sql
SELECT ST_DropPyramid('demo');
DROP TABLE demo_src;
```
