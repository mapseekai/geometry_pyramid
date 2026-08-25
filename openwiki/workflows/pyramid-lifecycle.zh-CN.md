# 金字塔生命周期工作流

## 1. 构建或重建金字塔

在加载并索引 PostGIS 源表之后，调用 `ST_BuildPyramid(table, geom_column, fid_column, config_json)`。`src/pyramid.rs` 会解析 `PyramidConfig`、校验标识符和不变量、upsert `geometry_pyramid_catalog`，并调用 `build::build`。

推荐的源表设置：

```sql
CREATE TABLE buildings (
  gid  serial PRIMARY KEY,
  name text,
  geom geometry(Polygon, 4326)
);
CREATE INDEX ON buildings USING gist (geom);

SELECT ST_BuildPyramid(
  'buildings', 'geom', 'gid',
  '{"minZoom":0,"maxZoom":14,"attributes":["name"]}'
);
```

运维注意事项：

- 对同一 `name` 重新运行 `ST_BuildPyramid` 会使用 shadow 存储并通过原子 swap 就地重建（`src/build.rs`、`src/shadow.rs`）。
- 空源或 geometry 全为 NULL 的源会构建出合法的空金字塔；测试已覆盖该行为（`src/build.rs`、`src/lib.rs`）。
- 标识符长度被限制，以确保生成名称适配 PostgreSQL 63 字节标识符限制（`README.md`、`src/config.rs`、`src/pyramid.rs`）。
- `tileSizeBudgetBytes` 不能与 `parallel > 1` 组合；代码会明确拒绝（`src/build.rs`）。

## 2. 获取瓦片

使用 `ST_Tile(name, z, x, y)`：

```sql
SELECT ST_Tile('buildings', 14, 13488, 6210);
```

`src/tile.rs` 处理三种读取模式：

- `minZoom <= z <= maxZoom` 且瓦片存在：从 `_gp_tiles_<name>` 返回物化 MVT 字节。
- 物化范围内 miss 且 `splitSize > 0`：从源表动态重新切片，因为稀疏瓦片可能在构建时被延后。
- `z > maxZoom` 且 `dynamicBeyondMaxZoom:true`：从源表动态切片，以模拟 Ganos `maxLevel` 行为。

`src/dynamic.rs` 共享构建管线和 LOD 子句，因此动态读取应像存储瓦片一样遵守 `buildRules.filter`、`attrFields` 和 `sample`。动态切片不缓存；重复读取可能开销很高，应确保源表有 GiST 索引。

## 3. 查看元数据和统计

```sql
SELECT ST_Pyramid('buildings');
SELECT * FROM ST_Pyramids();
```

`ST_Pyramid` 返回元数据和逐 zoom 统计。`ST_Pyramids()` 是 `sql/bootstrap.sql` 中的 PL/pgSQL，会统计每个 `_gp_tiles_<name>` 表中的行数；如果存储不存在则返回零。

统计由 `src/stats.rs` 维护，并由完整和增量构建路径更新。可用它们分析每个 zoom 的瓦片数、总字节数、最大瓦片字节数、构建耗时和重试次数。

## 4. 刷新源数据变更

有两条更新路径，它们有不同的正确性取舍。

### 谓词更新

```sql
SELECT ST_UpdatePyramid('buildings', 'name = ''City Hall''');
SELECT ST_UpdatePyramid('buildings', ''); -- full rebuild
```

对于非空条件，`src/incremental.rs` 会计算当前匹配行的范围，在每个 zoom 删除 dirty rectangle 内的瓦片，并在调用者事务中只重建这些瓦片。当谓词确实覆盖变更区域时，这条路径很快。

注意：谓词更新无法清理被删除行或移动后离开区域的旧位置，除非该谓词/区域在事后仍覆盖那些旧瓦片。`src/pyramid.rs` 明确记录了这一点。对于 delete 或 move-out 场景，请使用完整重建或 bbox 路径。

### Bbox 更新

```sql
SELECT ST_UpdatePyramidByExtent('buildings', 116.30, 39.90, 116.42, 39.99, 4326);
```

`ST_UpdatePyramidByExtent` 会把传入 bbox 转换到 tile SRS，并从当前源表重新切片所有相交瓦片。它不依赖行在事后是否匹配某个谓词，因此可处理区域内的插入、更新、删除和移动（`src/incremental.rs`）。

## 5. 删除金字塔

```sql
SELECT ST_DropPyramid('buildings');
```

`ST_DropPyramid` 删除 catalog 行、删除瓦片存储、删除统计并放弃 shadow 状态（`src/pyramid.rs`、`src/storage.rs`、`src/stats.rs`、`src/shadow.rs`）。它返回是否删除了 catalog 行。

## 6. 运行本地 demo

`demo/` 目录是离线验收工具：一个 Python HTTP server 提供 `ST_Tile` 字节和静态 MapLibre 资源。

来自 `demo/README.md`：

```bash
CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx install --release \
  --pg-config /opt/homebrew/opt/postgresql@17/bin/pg_config
CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx start pg17

createdb -h localhost -p 28817 geometry_pyramid
psql -h localhost -p 28817 -d geometry_pyramid -f demo/seed.sql

DATABASE_URL="host=localhost port=28817 dbname=geometry_pyramid" python3 demo/server.py
```

当前 `demo/server.py` 提供 `GET /tiles/{name}/{z}/{x}/{y}.pbf`，用简单 identifier regex 校验路径，调用 `SELECT ST_Tile(%s, %s, %s, %s)`，并把 SQL NULL/空字节映射为 HTTP 204。它是单连接、同步的；应把它视为本地验收/demo 代码，而不是生产服务基础设施。

## 工作流决策指南

- 需要 `maxZoom` 内最快读取：保持 `splitSize:0`，让所有非空瓦片都物化。
- 需要为稀疏数据减少/加快构建，并能接受实时读取：使用 `splitSize > 0`，并保持源索引健康。
- 需要按业务重要性做粗 zoom 简化：按 zoom band 使用 `buildRules.filter`、`attrFields` 和/或 `sample`。
- 需要高 zoom 稀疏构建速度：考虑 `preparedBuild:true` 搭配 `featureDrivenEnum:true`；纯点数据且 `maxZoom` 足够高时，考虑 `featureMajorPoints:true`。
- 需要在 delete/move 后做安全本地修正：优先使用 `ST_UpdatePyramidByExtent`，而不是谓词更新。
