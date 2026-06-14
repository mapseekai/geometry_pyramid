# HTTP 矢量瓦片服务 + 浏览器 demo 设计

日期:2026-06-14
状态:已与用户逐节确认

## 背景与目标

为 geometry_pyramid 提供一个最小的端到端验收工具:把 `ST_Tile(name,z,x,y)` 接成 HTTP 端点,配一个 MapLibre 页面在浏览器里实时缩放/平移,肉眼确认金字塔渲染——特别是两条动态边界:**超 maxZoom 的层级动态切片**与 **splitSize 稀疏块动态回退**。

这是独立于扩展本体(Rust/pgrx)的 demo 组件,放在 `demo/`,不进 `cargo pgrx test`。

## 已确认的决策

- **技术栈**:Python,仅用 stdlib `http.server` + 唯一外部依赖 `psycopg`(连 PG 必需)。
- **底图**:纯色背景,只渲染我们的矢量层,完全离线。
- **MapLibre 资源**:本地放一份 `maplibre-gl.js` + `.css`(离线,不引 CDN)。
- **数据**:自带 `seed.sql` 合成数据(密集簇 + 稀疏离群点)+ `ST_BuildPyramid`,一键可跑。
- **同源**:server 同时服务页面和瓦片,免 CORS(仍加 `Access-Control-Allow-Origin: *` 无害)。
- **金字塔名**:seed 与 html 都用固定名 `demo`。

## 文件布局

```
demo/
├── server.py        # stdlib http.server + psycopg:瓦片端点 + 自服务 index.html
├── index.html       # MapLibre GL JS,纯色背景,单一矢量源
├── maplibre-gl.js   # 本地 MapLibre(离线)
├── maplibre-gl.css  # 本地 MapLibre 样式
├── seed.sql         # 合成数据 + ST_BuildPyramid
└── README.md        # 三步跑起来 + 验收清单
```

## server.py(单文件,约 80 行)

- 模块级单连接 `psycopg.connect(DATABASE_URL)`;每请求一次查询;连接断开时重连一次(简单 try/重连)。`http.server` 本就串行,单连接足够 demo。
- 启动自检:跑一次 `SELECT ST_Tile('demo',0,0,0)` 确认连通,失败打印友好提示并退出。
- 路由(正则匹配 path):
  - `GET /` 或 `/index.html` → 返回 index.html(`text/html`)。
  - `GET /tiles/{name}/{z}/{x}/{y}.pbf` → name 经 `[A-Za-z0-9_]+` 校验;z/x/y 转 int;`SELECT ST_Tile(%s,%s,%s,%s)`:
    - 非空 bytes → `200`,`Content-Type: application/vnd.mapbox-vector-tile`,`Access-Control-Allow-Origin: *`。
    - NULL/空 → `204 No Content`(MapLibre 视作空瓦片)。
    - z/x/y 解析失败或 name 非法 → `400`。
    - ST_Tile 抛错(如 zoom 越界)→ `500` + 文本。
  - 其它静态资源(`/maplibre-gl.js`、`/maplibre-gl.css`)→ 按扩展名返回正确 Content-Type。
  - 其它 → `404`。
- 配置(环境变量):`DATABASE_URL`(默认指向 pgrx 实例,如 `postgresql://localhost:28817/geometry_pyramid`,实现时探到真实端口写进 README)、`PORT`(默认 8080)。
- 端点前缀用 `/tiles/...` 以避免与 `/`、静态资源路由冲突。

## index.html(MapLibre,纯色背景)

- 本地引用 `./maplibre-gl.js` / `./maplibre-gl.css`(离线)。
- style `version:8`:一个纯色 `background` 图层 + 一个 `vector` source:
  ```
  tiles: ["http://localhost:8080/tiles/demo/{z}/{x}/{y}.pbf"], minzoom:0, maxzoom:22
  ```
  maxzoom 故意设 22(> 金字塔 maxZoom=8),让 MapLibre 请求超 maxZoom 的瓦片以触发动态切片。
- 渲染图层:`circle` 图层,`source-layer: "demo"`(= ST_AsMVT 的层名 = 金字塔名),颜色醒目。
- 左上角信息框:前端按 `map.getZoom()` 显示当前 z 与"是否 > maxZoom(动态区)",辅助肉眼对应。
- 初始视图:对准 seed 的密集簇中心。

## seed.sql

```sql
DROP TABLE IF EXISTS demo_src CASCADE;
CREATE TABLE demo_src (gid serial PRIMARY KEY, geom geometry(Point,4326));
-- 密集簇:某区域数千点(高密度 → 物化)
INSERT INTO demo_src (geom)
SELECT ST_SetSRID(ST_MakePoint(116.39 + random()*0.2, 39.90 + random()*0.2),4326)
FROM generate_series(1,4000);
-- 稀疏离群:十几个远处孤立点(低密度 → splitSize 跳过 → 动态)
INSERT INTO demo_src (geom)
SELECT ST_SetSRID(ST_MakePoint(x, y),4326)
FROM (VALUES (-120,50),(80,-45),(10,10),(150,-30),(-60,-20),(30,60)) AS p(x,y);

SELECT ST_DropPyramid('demo');  -- 幂等(未知金字塔返回 false,不报错)
SELECT ST_BuildPyramid('demo_src','geom','gid',
   '{"name":"demo","maxZoom":8,"splitSize":500}');
```
maxZoom=8(缩放过 8 进动态区);splitSize=500(密集簇物化、稀疏离群点不物化但访问时动态生成)。两条动态边界都能在浏览器观察到。

## README.md(三步 + 验收清单)

```
1. cargo pgrx run pg17           # 起 PG + 装扩展(或 cargo pgrx start pg17 + 自己的 psql)
2. psql <conn> -f demo/seed.sql  # 造数据 + 建金字塔
3. pip install psycopg           # 一次性
   DATABASE_URL=<conn> python3 demo/server.py   # 打开 http://localhost:8080
```

验收清单:
- 浏览器打开 → 密集簇渲染成点;
- z≤8 → 瓦片来自物化存储;缩放过 8 → 信息框标"动态区",点仍渲染(超 maxZoom 动态切片);
- 平移到稀疏离群点 → 它们也渲染(splitSize 稀疏块动态回退);
- server 终端可见 `.pbf` 请求 + 200/204 日志。

## 错误处理

- name 非法字符 / z,x,y 非整数 → 400(不触达 DB)。
- ST_Tile 返回 NULL → 204(空瓦片,正常)。
- ST_Tile 抛错(zoom 越界等)→ 500 + 文本。
- DB 连接失败 → 启动自检即退出并提示;运行中断连 → 单次重连,再失败则该请求 500。

## 测试 / 验收

本组件是手动验收工具,**不进 `cargo pgrx test`**。验收方式见 README 清单(浏览器肉眼 + server 日志)。server.py 的启动自检提供最低限度的连通性保证。

## 不在范围

- 生产级特性(连接池、并发、缓存头、gzip、鉴权)——demo 串行足够。
- 瓦片缓存、TileJSON 端点(MapLibre 直接用 tiles 模板,无需 TileJSON)。
- 联网底图。
