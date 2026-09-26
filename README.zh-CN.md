# geometry_pyramid 中文说明

[English README](README.md)

`geometry_pyramid` 是一个 PostgreSQL 扩展，用于在 PostGIS geometry 表上构建
**矢量瓦片金字塔**。它提供与阿里云
[GanosBase `ganos_geometry_pyramid`][ganos] 接近的 SQL 使用界面，并基于
[pgrx](https://github.com/pgcentralfoundation/pgrx) 构建。

## 当前状态

`ST_BuildPyramid` 会通过 PostGIS 的 `ST_TileEnvelope`、`ST_AsMVTGeom` 和
`ST_AsMVT` 生成 MVT 瓦片，并写入每个金字塔独立的存储表。构建过程使用
**原子重建**：瓦片先写入影子表，再在一个事务中切换到正式表，因此读请求不会
看到半成品。

已支持的主要能力：

- `parallel` 配置：使用 PostgreSQL background worker 并行构建 zoom。
- `ST_Pyramid()`：返回配置、瓦片数量和逐 zoom 构建统计。
- `ST_UpdatePyramid`：按 `where_clause` 做增量刷新；空条件表示完整重建。
- `ST_UpdatePyramidByExtent`：按 bbox 重新切片，适合删除、移动、跨瓦片更新。
- `ST_Tile`：`maxZoom` 内读取物化瓦片；超过 `maxZoom` 时可按需实时切片。
- `splitSize`：低密度瓦片可延迟到请求时实时切片。
- `buildRules`：按 zoom 配置 LOD 过滤、属性字段覆盖和确定性采样。

当前 roadmap 中仍保留 coalesce-densest。

## SQL 接口

| 函数 | 用途 |
|---|---|
| `ST_BuildPyramid(table, geom, fid, config) -> bool` | 注册并构建一个金字塔。 |
| `ST_UpdatePyramid(name, where_clause) -> bool` | 刷新与匹配行相交的瓦片；空条件表示完整重建。 |
| `ST_UpdatePyramidByExtent(name, xmin, ymin, xmax, ymax, srid) -> bool` | 从当前源表重新切片 bbox 相交瓦片，可处理删除、移动和跨瓦片更新。 |
| `ST_DropPyramid(name) -> bool` | 删除金字塔及其瓦片存储表。 |
| `ST_Pyramid(name) -> jsonb` | 查看金字塔元数据和逐 zoom 构建统计。 |
| `ST_Tile(name, z, x, y) -> bytea` | 获取 MVT 瓦片；空瓦片、不可用瓦片或未知金字塔返回 `NULL`。 |
| `ST_Pyramids() -> SETOF (name, source_table, built_at, tile_count)` | 列出所有金字塔。 |
| `geometry_pyramid_version() -> text` | 查看扩展版本。 |

瓦片 envelope 使用 PostGIS 内置函数 `ST_TileEnvelope(z, x, y)`。

## 配置项

`ST_BuildPyramid` 的 `config` 参数是 JSON 文档；传入 `'{}'` 使用默认值。

| Key | 默认值 | 说明 |
|---|---|---|
| `name` | 源表名 | 金字塔名称；允许同一源表构建多个金字塔。 |
| `sourceSRS` | `4326` | 源 geometry 列的 SRID。 |
| `tileSRS` | `3857` | 瓦片坐标系，目前仅支持 Web Mercator（`3857`），其他值会被拒绝。 |
| `tileSize` | `4096` | MVT extent，必须大于 0。 |
| `tileExtend` | `8` | 瓦片边缘 buffer，用于减少边界裁剪问题。 |
| `minZoom` / `maxZoom` | `0` / `18` | 需要物化构建的 zoom 范围。 |
| `dynamicBeyondMaxZoom` | `true` | z > maxZoom 时从源表实时切片；设为 `false` 时严格返回 `NULL`。 |
| `parallel` | `1` | 构建 worker 数量；大于 1 时使用 background worker 并行构建。 |
| `preparedBuild` | `false` | `parallel=1` 时的加速路径：先将源几何投影到临时表并建立索引。 |
| `featureMajorPoints` | `false` | 纯 `POINT` 源的实验性稀疏路径；需要 `preparedBuild=true`、`parallel=1`、`tileSRS=3857`。 |
| `featureMajorMinZoom` | `13` | `featureMajorPoints` 启用的最小 `maxZoom`；设为 `0` 表示符合条件就启用。 |
| `featureDrivenEnum` | `false` | 通用 feature-driven 枚举路径；适合高 zoom 稀疏数据，输出与 tile-major 路径一致。 |
| `attributes` | `[]` | 写入 MVT feature 的非几何字段。 |
| `simplifyPixels` | `1.0` | 按瓦片像素计算的 DP 简化容差；`0` 表示关闭。 |
| `minFeatureSizePixels` | `0.0` | 丢弃低于该像素大小的 polygon/line；`0` 表示关闭。 |
| `dropRate` | `1.0` | tippecanoe 风格点保留率：`dropRate^(maxZoom - z)`；`1.0` 表示关闭。 |
| `tileSizeBudgetBytes` | `0` | 单瓦片 MVT 大小上限；超限时收紧参数重建该 zoom；`0` 表示关闭。 |
| `maxTileSizeAttempts` | `5` | 追逐瓦片大小预算时，每个 zoom 的最大重试次数。 |
| `buildTimeoutMs` | `0` | 每个并行 zoom 及共享源预处理的等待上限（毫秒）；`0` 表示不限时。超时或取消查询时，会先停止 worker 再返回。 |
| `splitSize` | `0` | `0` 表示物化所有非空瓦片；大于 0 时，低于该要素数的瓦片延迟到请求时实时切片。 |
| `buildRules` | `[]` | 按 zoom 的 LOD 规则，形如 `{minZoom, maxZoom, filter?, attrFields?, sample?}`。 |

## 快速使用

### 1. 启用扩展

每个数据库执行一次：

```sql
CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;
```

### 2. 准备源表

源表需要一个 geometry 列和一个稳定的要素 id 列。建议给 geometry 列建立 GiST
索引，这会直接影响构建和实时切片性能。

```sql
CREATE TABLE buildings (
    gid  serial PRIMARY KEY,
    name text,
    geom geometry(Polygon, 4326)
);

CREATE INDEX ON buildings USING gist (geom);
```

### 3. 构建金字塔

`ST_BuildPyramid(table, geom, fid, config)` 的前三个参数分别是源表名、geometry
列名和稳定要素 id 列名。

```sql
SELECT ST_BuildPyramid(
    'buildings',
    'geom',
    'gid',
    '{
       "name":       "buildings",
       "sourceSRS":  4326,
       "tileSRS":    3857,
       "minZoom":    0,
       "maxZoom":    14,
       "attributes": ["name"]
     }'
);
```

`sourceSRS` 必须与源表 geometry 的 SRID 一致。`attributes` 指定写入 MVT 的
非几何字段。重复执行 `ST_BuildPyramid` 会原地重建同名金字塔。

### 4. 获取瓦片

```sql
SELECT ST_Tile('buildings', 14, 13488, 6210);
```

返回值是 MVT `bytea`。空瓦片、不可用瓦片或未知金字塔返回 `NULL`。
`maxZoom` 以内的瓦片来自物化存储；当 `dynamicBeyondMaxZoom=true` 时，超过
`maxZoom` 的请求会从源表实时切片。

### 5. 查看状态

```sql
SELECT ST_Pyramid('buildings');
SELECT * FROM ST_Pyramids();
```

`ST_Pyramid` 返回 `jsonb`，包含配置、总瓦片数和逐 zoom 构建统计。

### 6. 更新源数据后刷新

按条件刷新：

```sql
SELECT ST_UpdatePyramid('buildings', 'name = ''City Hall''');
```

完整重建：

```sql
SELECT ST_UpdatePyramid('buildings', '');
```

按范围刷新：

```sql
SELECT ST_UpdatePyramidByExtent('buildings', 116.30, 39.90, 116.42, 39.99, 4326);
```

`ST_UpdatePyramid(name, where_clause)` 只重建与匹配行相交的瓦片。如果要处理
删除、移动或需要覆盖旧位置的更新，优先使用 `ST_UpdatePyramidByExtent`，它会
从当前源表重新切片整个 bbox 区域。

### 7. 删除金字塔

```sql
SELECT ST_DropPyramid('buildings');
```

## 使用注意

金字塔名、表名和列名限制为 **45 个字符**，以确保派生表名不会超过
PostgreSQL 的 63 字节标识符限制。

每个金字塔的瓦片存储在 `_gp_tiles_<name>` 表中，字段包括：
`id bigserial, z smallint, x integer, y integer, format text, srid integer,
mvt bytea`，主键为 `(z, x, y)`。`format` 当前固定为 `'pbf'`，`srid` 是瓦片
坐标系。业务可以直接读取该表；`ST_Tile` 只返回 `mvt` 列。

`splitSize=0` 是默认值，会物化 `maxZoom` 内所有非空瓦片，读取时就是存储查找。
如果设置 `splitSize > 0`，构建会跳过低密度瓦片；这些瓦片在请求时会从源表实时
切片。这样可以缩短构建、减少存储，但会增加重复读取成本。

`buildRules.filter` 是原始 SQL 布尔表达式，会以 `ST_BuildPyramid` 调用者的
权限执行，定位类似受信任管理员编写的视图定义。扩展会拒绝语句分隔符 `;`，
但这只是防御性检查，不等同于完整 SQL 表达式沙箱。

## 构建和测试

```bash
cargo check
cargo pgrx run pg17
cargo pgrx test
cargo pgrx package
```

进入 psql 后：

```sql
CREATE EXTENSION postgis;
CREATE EXTENSION geometry_pyramid;
SELECT ST_BuildPyramid('buildings', 'geom', 'gid', '{"maxZoom":14}');
SELECT ST_Pyramid('buildings');
```

## Roadmap

1. ~~Bootstrap：元数据 catalog、SQL 接口、配置解析。~~ ✅
2. ~~Tile codec：通过 PostGIS `ST_AsMVTGeom` / `ST_AsMVT` 输出 MVT。~~ ✅
3. ~~Build pipeline：按 zoom 写入每个金字塔的瓦片存储表。~~ ✅
4. ~~Catalog views：`ST_Pyramids()`。~~ ✅
5. ~~Tippecanoe 风格策略：逐 zoom 简化、最小要素尺寸、点抽稀。~~ ✅
6. ~~Tile-size budget：类似 `--drop-densest-as-needed` 的几何收紧重试。~~ ✅
7. **Coalesce-densest**：低 zoom 合并相邻小面（`ST_Union` / `ST_Buffer(0)`）。
8. ~~Parallelism：支持 `parallel` 配置，使用 PostgreSQL background worker。~~ ✅
9. ~~Incremental updates：`ST_UpdatePyramid(name, where_clause)` 增量刷新。~~ ✅
10. ~~Statistics：`ST_Pyramid()` 输出逐 zoom 行数、字节数和构建耗时。~~ ✅
11. ~~On-demand slicing beyond maxZoom：超过 `maxZoom` 时按需实时切片。~~ ✅
12. ~~splitSize：低密度瓦片延迟到请求时实时切片。~~ ✅
13. ~~buildRules：逐级 LOD 过滤、属性字段覆盖和采样。~~ ✅

[ganos]: https://www.alibabacloud.com/help/en/polardb/polardb-for-postgresql/vector-pyramid

## License

MIT（见 `LICENSE`）。
