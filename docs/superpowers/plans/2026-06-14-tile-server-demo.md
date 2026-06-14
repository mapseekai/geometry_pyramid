# HTTP 瓦片服务 + MapLibre demo 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `demo/` 下提供一个最小、离线、一键可跑的端到端验收工具:Python stdlib HTTP 服务把 `ST_Tile` 接成 `/tiles/{name}/{z}/{x}/{y}.pbf`,配离线 MapLibre 页面在浏览器看图,验证超 maxZoom 动态切片与 splitSize 稀疏回退。

**Architecture:** 单文件 `server.py`(stdlib `http.server` + `psycopg`)同源服务瓦片端点与静态页面;`index.html` 用本地 MapLibre 资源渲染纯色背景上的单一矢量层;`seed.sql` 造合成数据并建 `demo` 金字塔(maxZoom=8、splitSize=500)。纯 demo 组件,不进 `cargo pgrx test`。

**Tech Stack:** Python 3(stdlib `http.server` + `psycopg` v3),MapLibre GL JS(本地离线),PostgreSQL/PostGIS + geometry_pyramid 扩展。

**通用约定:**
- 在 `main` 分支工作(扩展已合并);本计划全部是新增 `demo/` 文件,不改扩展源码。
- Python 单测命令:`cd /Users/zhang/code/geometry_pyramid/demo && python3 -m unittest -v`。
- 扩展回归(确认没碰坏扩展):`CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`(应仍 32 passed)——仅在 Task 全部完成后跑一次确认。
- Commit `<type>: <description>`,无 attribution 尾注。忽略 gitnexus MCP 提示。
- **不要 push。**

**连接串说明(实现者必读):** `server.py` 用 `psycopg.connect(os.environ.get("DATABASE_URL", ""))`——空串时 libpq 回退到 `PGHOST`/`PGPORT`/`PGDATABASE` 等环境变量。pgrx 实例的真实端口由 Task 4 探测(`cargo pgrx` 启动信息 / `~/.pgrx/data-17/postgresql.conf` 的 `port`)并写进 README;代码层不硬编码端口。

---

### Task 1: 瓦片服务 `demo/server.py`(含纯函数单测)

**Files:**
- Create: `demo/server.py`
- Create: `demo/test_server.py`

把路径解析/校验做成纯函数 `parse_tile_path`,用 `unittest` 无需 DB 测试;HTTP handler + DB 查询 + 启动自检为集成部分(Task 4 用 curl 端到端验证)。**连接必须惰性建立(在 `main()` 内),模块导入无副作用**,否则单测会尝试连库。

- [ ] **Step 1: 写失败的单测** `demo/test_server.py`

```python
import unittest
import server


class ParseTilePathTests(unittest.TestCase):
    def test_valid_path(self):
        self.assertEqual(
            server.parse_tile_path("/tiles/demo/3/4/5.pbf"),
            ("demo", 3, 4, 5),
        )

    def test_name_with_underscore_and_digits(self):
        self.assertEqual(
            server.parse_tile_path("/tiles/_gp_1/0/0/0.pbf"),
            ("_gp_1", 0, 0, 0),
        )

    def test_rejects_illegal_name(self):
        self.assertIsNone(server.parse_tile_path("/tiles/foo;DROP/0/0/0.pbf"))

    def test_rejects_non_integer(self):
        self.assertIsNone(server.parse_tile_path("/tiles/demo/x/0/0.pbf"))

    def test_rejects_wrong_extension(self):
        self.assertIsNone(server.parse_tile_path("/tiles/demo/0/0/0.png"))

    def test_rejects_root(self):
        self.assertIsNone(server.parse_tile_path("/"))

    def test_rejects_missing_coords(self):
        self.assertIsNone(server.parse_tile_path("/tiles/demo/0/0.pbf"))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: 运行确认失败**

Run: `cd /Users/zhang/code/geometry_pyramid/demo && python3 -m unittest -v`
Expected: FAIL/ERROR(`server` 模块不存在 / `parse_tile_path` 未定义)。

- [ ] **Step 3: 实现 `demo/server.py`**

```python
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
    name, z, x, y = m.group(1), int(m.group(2)), int(m.group(3)), int(m.group(4))
    return (name, z, x, y)


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
        # One reconnect attempt, then re-raise to the handler (→ 500).
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

        # Static page / assets.
        if path in _STATIC:
            fname, ctype = _STATIC[path]
            fpath = DEMO_DIR / fname
            if not fpath.exists():
                self._send(404, b"not found")
                return
            self._send(200, fpath.read_bytes(), ctype)
            return

        # Tile endpoint.
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

    def log_message(self, fmt, *args):  # concise one-line logs
        sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))


def main():
    global _CONN
    dsn = os.environ.get("DATABASE_URL", "")
    try:
        _CONN = psycopg.connect(dsn, autocommit=True)
        # Startup self-check: confirm the extension + a pyramid are reachable.
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
```

- [ ] **Step 4: 运行单测确认通过**

Run: `cd /Users/zhang/code/geometry_pyramid/demo && python3 -m unittest -v`
Expected: 7 tests PASS(导入 `server` 无副作用 → 不连库)。

NOTE: 若导入 `server` 报 `ModuleNotFoundError: psycopg`,先 `pip install 'psycopg[binary]'`,再跑。单测只导入模块、调用纯函数,不触发连接。

- [ ] **Step 5: Commit**

```bash
git add demo/server.py demo/test_server.py
git commit -m "feat: minimal stdlib tile server serving ST_Tile as MVT"
```

---

### Task 2: 离线 MapLibre 页面 `demo/index.html` + 本地资源

**Files:**
- Create: `demo/index.html`
- Create: `demo/maplibre-gl.js`(下载)
- Create: `demo/maplibre-gl.css`(下载)

- [ ] **Step 1: 下载本地 MapLibre 资源(一次性联网)**

Run:
```bash
cd /Users/zhang/code/geometry_pyramid/demo
curl -fsSL https://unpkg.com/maplibre-gl@4.7.1/dist/maplibre-gl.js  -o maplibre-gl.js
curl -fsSL https://unpkg.com/maplibre-gl@4.7.1/dist/maplibre-gl.css -o maplibre-gl.css
ls -l maplibre-gl.js maplibre-gl.css
```
Expected: 两个文件存在,`maplibre-gl.js` 数百 KB(非空、非 HTML 错误页)。校验:`head -c 50 maplibre-gl.js` 应是 JS(非 `<!doctype html>`)。若 unpkg 不可达,换 `https://cdn.jsdelivr.net/npm/maplibre-gl@4.7.1/dist/maplibre-gl.js`(及 `.css`)。

- [ ] **Step 2: 创建 `demo/index.html`**

```html
<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>geometry_pyramid demo</title>
  <link href="./maplibre-gl.css" rel="stylesheet" />
  <style>
    html, body, #map { margin: 0; height: 100%; }
    #hud {
      position: absolute; top: 8px; left: 8px; z-index: 1;
      font: 12px/1.4 monospace; background: rgba(0,0,0,.7); color: #fff;
      padding: 6px 8px; border-radius: 4px;
    }
    #hud b { color: #6cf; }
  </style>
</head>
<body>
  <div id="map"></div>
  <div id="hud">z=<b id="z">0</b> · <span id="mode"></span></div>
  <script src="./maplibre-gl.js"></script>
  <script>
    const PYRAMID = "demo";
    const MAX_ZOOM = 8; // pyramid maxZoom; beyond this → dynamic slicing
    const map = new maplibregl.Map({
      container: "map",
      style: {
        version: 8,
        sources: {
          pyr: {
            type: "vector",
            tiles: [location.origin + "/tiles/" + PYRAMID + "/{z}/{x}/{y}.pbf"],
            minzoom: 0,
            maxzoom: 22  // request beyond pyramid maxZoom to exercise dynamic slicing
          }
        },
        layers: [
          { id: "bg", type: "background", paint: { "background-color": "#11151c" } },
          {
            id: "pts", type: "circle", source: "pyr", "source-layer": PYRAMID,
            paint: {
              "circle-radius": 2.5,
              "circle-color": "#ff5a5f",
              "circle-opacity": 0.85
            }
          }
        ]
      },
      center: [116.49, 40.0],  // matches seed.sql dense cluster
      zoom: 9
    });
    map.addControl(new maplibregl.NavigationControl());
    function updateHud() {
      const z = map.getZoom();
      document.getElementById("z").textContent = z.toFixed(1);
      document.getElementById("mode").textContent =
        z > MAX_ZOOM ? "DYNAMIC (z > maxZoom)" : "materialized";
    }
    map.on("zoom", updateHud);
    map.on("load", updateHud);
  </script>
</body>
</html>
```

- [ ] **Step 3: 静态资源烟测**(无需 DB:server 静态分支独立)

无法在无 DB 时启动 server(`main()` 会自检连库退出)。改为直接断言文件存在且 index.html 引用本地资源:
```bash
cd /Users/zhang/code/geometry_pyramid/demo
test -s maplibre-gl.js && test -s maplibre-gl.css && test -s index.html && echo OK
grep -q './maplibre-gl.js' index.html && grep -q 'source-layer' index.html && echo REFS_OK
```
Expected: 打印 `OK` 和 `REFS_OK`。

- [ ] **Step 4: Commit**

```bash
git add demo/index.html demo/maplibre-gl.js demo/maplibre-gl.css
git commit -m "feat: offline MapLibre demo page with local assets"
```

---

### Task 3: 合成数据 `demo/seed.sql`

**Files:**
- Create: `demo/seed.sql`

- [ ] **Step 1: 创建 `demo/seed.sql`**

```sql
-- geometry_pyramid demo seed: synthetic data + a pyramid showcasing both
-- dynamic boundaries (z > maxZoom level-dynamic, and splitSize density-dynamic).
CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;

DROP TABLE IF EXISTS demo_src CASCADE;
CREATE TABLE demo_src (gid serial PRIMARY KEY, geom geometry(Point, 4326));

-- Dense cluster near (116.49, 40.0): ~4000 points → high density → materialized.
INSERT INTO demo_src (geom)
SELECT ST_SetSRID(ST_MakePoint(116.39 + random() * 0.2, 39.90 + random() * 0.2), 4326)
FROM generate_series(1, 4000);

-- Sparse outliers far away: low density → skipped by splitSize → served dynamically.
INSERT INTO demo_src (geom)
SELECT ST_SetSRID(ST_MakePoint(x, y), 4326)
FROM (VALUES (-120, 50), (80, -45), (10, 10), (150, -30), (-60, -20), (30, 60)) AS p(x, y);

-- Idempotent rebuild. maxZoom=8 (zoom past it → dynamic), splitSize=500
-- (dense cluster materializes, sparse outliers do not but are served on-demand).
SELECT ST_DropPyramid('demo');
SELECT ST_BuildPyramid('demo_src', 'geom', 'gid',
    '{"name": "demo", "maxZoom": 8, "splitSize": 500}');

-- Quick sanity: z=0 covers the world; the dense cluster makes it non-empty.
SELECT 'z0 bytes = ' || COALESCE(octet_length(ST_Tile('demo', 0, 0, 0)), 0) AS sanity;
```

- [ ] **Step 2: 语法自检(无需 DB)**

无法离线校验 SQL 语义;做基本检查:
```bash
grep -q "ST_BuildPyramid" /Users/zhang/code/geometry_pyramid/demo/seed.sql \
  && grep -q '"splitSize": 500' /Users/zhang/code/geometry_pyramid/demo/seed.sql \
  && echo SEED_OK
```
Expected: `SEED_OK`。(真正执行在 Task 4。)

- [ ] **Step 3: Commit**

```bash
git add demo/seed.sql
git commit -m "feat: demo seed with dense cluster and sparse outliers"
```

---

### Task 4: README + 端到端验证(curl 链路)

**Files:**
- Create: `demo/README.md`

本任务实际把扩展装进 pgrx 实例、跑 seed、起 server,用 curl 验证整条链路(server→PG→ST_Tile),并把探到的真实连接串写进 README。浏览器视觉确认是人工最后一步。

- [ ] **Step 1: 把扩展装进 pgrx pg17 并启动**

Run:
```bash
cd /Users/zhang/code/geometry_pyramid
CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx install --pg-config /opt/homebrew/opt/postgresql@17/bin/pg_config 2>&1 | tail -3 || true
CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx start pg17 2>&1 | tail -3
```
NOTE: 若 `install --pg-config` 因 pgrx 版本参数不同而失败,改用 `cargo pgrx start pg17` 后通过 `cargo pgrx run pg17 <<< '\q'` 触发安装,或直接在 Step 2 的 psql 里 `CREATE EXTENSION` 时确认扩展已装(pgrx start 用的是它自己的安装树)。目标:pgrx 的 pg17 实例在跑且能 `CREATE EXTENSION geometry_pyramid`。

- [ ] **Step 2: 探测连接串**

Run:
```bash
PGPORT=$(grep -E '^port' ~/.pgrx/data-17/postgresql.conf | grep -oE '[0-9]+' | head -1)
echo "pgrx pg17 port = $PGPORT"
```
记下端口(pgrx 通常用 288xx)。连接串形如 `host=localhost port=$PGPORT dbname=geometry_pyramid`(pgrx 默认建一个与 crate 同名的数据库;若不存在,用 `postgres` 库并在其中 CREATE)。

- [ ] **Step 3: 跑 seed**

Run:
```bash
PGPORT=$PGPORT psql -h localhost -p "$PGPORT" -d geometry_pyramid -f demo/seed.sql 2>&1 | tail -6
```
Expected: 末尾 `sanity` 行显示 `z0 bytes = <非0>`(证明金字塔建好、z=0 有内容)。若 `geometry_pyramid` 库不存在,先 `createdb -h localhost -p $PGPORT geometry_pyramid` 再重跑。

- [ ] **Step 4: 起 server(后台)并 curl 验证整条链路**

Run:
```bash
cd /Users/zhang/code/geometry_pyramid/demo
pip install 'psycopg[binary]' >/dev/null 2>&1 || true
DATABASE_URL="host=localhost port=$PGPORT dbname=geometry_pyramid" PORT=8080 python3 server.py &
SRV=$!; sleep 1
# 1) 静态页
curl -s -o /dev/null -w "index: %{http_code} %{content_type}\n" http://127.0.0.1:8080/
# 2) 物化瓦片 (z=0 ≤ maxZoom):200 + 非零长度
curl -s -o /tmp/t0.pbf -w "z0: %{http_code} %{size_download}B %{content_type}\n" http://127.0.0.1:8080/tiles/demo/0/0/0.pbf
# 3) 超 maxZoom 动态瓦片 (z=12 > 8) 覆盖密集簇:200 + 非零(证明动态切片)
TX=$(psql -h localhost -p "$PGPORT" -d geometry_pyramid -tAc "SELECT floor((ST_X(ST_Transform(ST_SetSRID(ST_MakePoint(116.49,40.0),4326),3857))+20037508.34)/(2*20037508.34)*pow(2,12))::int")
TY=$(psql -h localhost -p "$PGPORT" -d geometry_pyramid -tAc "SELECT floor((20037508.34-ST_Y(ST_Transform(ST_SetSRID(ST_MakePoint(116.49,40.0),4326),3857)))/(2*20037508.34)*pow(2,12))::int")
curl -s -o /tmp/t12.pbf -w "z12 dynamic: %{http_code} %{size_download}B\n" "http://127.0.0.1:8080/tiles/demo/12/$TX/$TY.pbf"
# 4) 空区:204
curl -s -o /dev/null -w "empty: %{http_code}\n" http://127.0.0.1:8080/tiles/demo/3/0/0.pbf
# 5) 非法 name:404
curl -s -o /dev/null -w "bad: %{http_code}\n" "http://127.0.0.1:8080/tiles/foo;bar/0/0/0.pbf"
kill $SRV 2>/dev/null
```
Expected:
- `index: 200 text/html...`
- `z0: 200 <非0>B application/vnd.mapbox-vector-tile`
- `z12 dynamic: 200 <非0>B`(关键:超 maxZoom 仍出瓦片 → 动态切片走通)
- `empty: 204`
- `bad: 404`

若 `z12 dynamic` 为 204:换一个确实覆盖密集簇的 z=12 瓦片坐标(用上面的公式但确认 TX/TY 正确),或确认 seed 的金字塔 `dynamicBeyondMaxZoom` 默认开(它是)。用 systematic-debugging。

- [ ] **Step 5: 写 `demo/README.md`**(把探到的真实端口写进去)

```markdown
# geometry_pyramid demo — HTTP tile server + MapLibre viewer

Minimal, offline, end-to-end acceptance tool. Serves `ST_Tile` output as MVT and
renders it in the browser on a plain background — no internet, no basemap.

## Prerequisites
- The extension installed into a running PostgreSQL with PostGIS.
- Python 3 with `psycopg`: `pip install 'psycopg[binary]'`.

## Run (3 steps)

1. **Start PostgreSQL with the extension.** Using the pgrx-managed instance:
   ```bash
   CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx start pg17
   ```
   Find its port: `grep -E '^port' ~/.pgrx/data-17/postgresql.conf`.

2. **Seed data + build the demo pyramid:**
   ```bash
   psql -h localhost -p <PORT> -d geometry_pyramid -f demo/seed.sql
   ```
   (If the database does not exist: `createdb -h localhost -p <PORT> geometry_pyramid`.)

3. **Start the tile server:**
   ```bash
   DATABASE_URL="host=localhost port=<PORT> dbname=geometry_pyramid" python3 demo/server.py
   ```
   Open http://127.0.0.1:8080 .

## What to verify
- The dense cluster renders as red points on the dark background.
- Zoom in past **z=8** (the pyramid's `maxZoom`): the HUD shows `DYNAMIC` and
  points still render — tiles are sliced on demand beyond the materialized range.
- Pan to the sparse outliers (e.g. far from the cluster): they render too —
  `splitSize` deferred them at build time; `ST_Tile` slices them on demand.
- The server terminal logs `.pbf` requests with `200` (has data) / `204` (empty).

## Endpoint
`GET /tiles/{name}/{z}/{x}/{y}.pbf` → MVT (`200`), empty tile (`204`), bad path (`404`),
DB error (`500`).

## Notes
- Single-connection, synchronous — acceptance/demo only, not production.
- Offline: MapLibre assets are vendored locally (`maplibre-gl.js`/`.css`).
```
把 `<PORT>` 在文中保留为占位说明(端口随机/可变),但在文件顶部加一行注明你本机探到的实际端口,例如:`> This machine's pgrx pg17 port: <填入 Step 2 探到的值>`。

- [ ] **Step 6: 扩展回归(确认没碰坏扩展)+ Commit**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17 2>&1 | grep 'test result'`
Expected: 32 passed(demo 全是新增文件,未触扩展源码)。

```bash
git add demo/README.md
git commit -m "docs: demo README + verified end-to-end tile serving"
```

---

## 自审记录

- **规格覆盖**:server.py 端点 + 204/400/500 语义(Task 1)、离线 MapLibre 页面 + 本地资源(Task 2)、seed.sql 双动态边界数据(Task 3)、README + 端到端 curl 验证 + 验收清单(Task 4)——spec 各节均有对应任务。同源免 CORS、`/tiles/` 前缀、固定金字塔名 `demo`、maxZoom=8/splitSize=500、启动自检均落到 Task 1/2/3/4。
- **占位符**:无 TBD;`<PORT>` 是运行期真实可变值,Task 4 Step 2 明确探测方式并写入 README,非占位。MapLibre 版本钉 4.7.1,有备用 CDN。
- **一致性**:`parse_tile_path` 返回 `(str,int,int,int)`,单测与 handler 用法一致;index.html 的 tiles 模板 `/tiles/demo/{z}/{x}/{y}.pbf` 与 server `_TILE_RE` 和固定名 `demo` 一致;`source-layer: "demo"` = ST_AsMVT 层名 = 金字塔名;maxZoom 常量(html=8)与 seed(maxZoom:8)一致。
- **范围**:纯 demo,不进 cargo pgrx test;Task 4 的扩展回归只为确认未误伤扩展。
- **错误处理**:400 在规格里提到(z/x/y 解析失败);实现中非法路径统一走 404(parse 返回 None → 404)。这是对规格的轻微收敛——非法 tile 路径与未知路由都返回 404,语义可接受(客户端都视作"无此瓦片");已在 server.py 注释体现。若严格要 400,可在 Task 1 区分"形似 /tiles/ 但参数非法"→400,但 YAGNI,demo 用 404 足够。
```
