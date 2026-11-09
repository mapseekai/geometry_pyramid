# geometry_pyramid 中文说明

[English README](README.md)

`geometry_pyramid` 是一个 PostgreSQL 扩展，用于在 PostGIS geometry 表上构建
**矢量瓦片金字塔**。它提供与阿里云
[GanosBase `ganos_geometry_pyramid`][ganos] 接近的 SQL 使用界面，并基于
[pgrx](https://github.com/pgcentralfoundation/pgrx) 构建。

## 当前状态

`ST_BuildPyramid` 会通过 PostGIS 的 `ST_TileEnvelope`、`ST_AsMVTGeom` 和
`ST_AsMVT` 生成 MVT 瓦片，并写入每个金字塔独立的存储表。全量重建在同一事务内替换瓦片行，保留正式表的 OID、读取授权和依赖视图。

已支持的主要能力：

- `parallel` 配置：使用 PostgreSQL background worker 并行构建 zoom。
- `ST_Pyramid()`：返回配置、瓦片数量和逐 zoom 构建统计。
- `ST_UpdatePyramid`：按 `where_clause` 做增量刷新；空条件表示完整重建。
- `ST_UpdatePyramidByExtent`：按 bbox 重新切片，适合删除、移动、跨瓦片更新。
- `ST_Tile`：`maxZoom` 内读取物化瓦片；超过 `maxZoom` 时可按需实时切片。
- `splitSize`：低密度瓦片可延迟到请求时实时切片。
- `buildRules`：按 zoom 配置 LOD 过滤、属性字段覆盖、确定性采样和点网格抽稀。

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
| `minZoom` / `maxZoom` | `0` / `18` | 物化构建的 zoom 范围，含两端。Ganos 配置可用 `maxLevel` 代替 `maxZoom`，同时传入两者会报错。 |
| `dynamicBeyondMaxZoom` | `true` | z > maxZoom 时从源表实时切片；设为 `false` 时严格返回 `NULL`。 |
| `parallel` | `1` | 包含协调端的最大构建槽位数，上限为 32。扩展根据源数据和预计工作量决定是否启动后台 worker。 |
| `preparedBuild` | `false` | 预先投影源几何。串行使用会话临时表，满足并行条件时使用受管的共享工作表。 |
| `featureMajorPoints` | `false` | 纯 `POINT` 源的实验性稀疏路径；需要 `preparedBuild=true`、`parallel=1`、`tileSRS=3857`。 |
| `featureMajorMinZoom` | `13` | `featureMajorPoints` 启用的最小 `maxZoom`；设为 `0` 表示符合条件就启用。 |
| `featureDrivenEnum` | `false` | 从要素包围盒枚举候选瓦片。需要 `preparedBuild=true` 和 `tileSRS=3857`。构建器会估算稀疏格网的成本，也可在单进程构建少量瓦片时复用跨瓦片复杂几何的简化结果。 |
| `attributes` | `[]` | 写入 MVT feature 的非几何字段。 |
| `simplifyPixels` | `1.0` | 按瓦片像素计算的 DP 简化容差；`0` 表示关闭。 |
| `minFeatureSizePixels` | `0.0` | 丢弃低于该像素大小的 polygon/line；`0` 表示关闭。 |
| `dropRate` | `1.0` | tippecanoe 风格点保留率：`dropRate^(maxZoom - z)`；`1.0` 表示关闭。 |
| `tileSizeBudgetBytes` | `0` | 单瓦片未压缩 MVT 的目标字节数。超出后收紧参数并重建该 zoom；用完重试仍超出时发出警告。`0` 表示关闭。 |
| `maxTileSizeAttempts` | `5` | 追逐瓦片大小预算时，每个 zoom 的最大重试次数。 |
| `buildTimeoutMs` | `0` | 每个并行 zoom 及共享源预处理的等待上限（毫秒）；`0` 表示不限时。超时或取消查询时，会先停止 worker 再返回。 |
| `splitSize` | `0` | `0` 表示物化所有非空瓦片；大于 0 时，低于该要素数的瓦片延迟到请求时实时切片。 |
| `buildRules` | `[]` | 按 zoom 的 LOD 规则，形如 `{minZoom, maxZoom, filter?, attrFields?, sample?, pointGrid?}`。 |

## 快速使用

### 1. 启用扩展

每个数据库执行一次：

```sql
CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;
```

### 2. 准备源表

源表需要 geometry 列和稳定的要素 ID。EPSG:3857 和常规 EPSG:4326 数据可使用源几何的 GiST 索引。其他投影可为 `ST_Transform(geom,3857)` 建立表达式 GiST 索引，供原表构建、局部刷新和动态切片使用。

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

首次构建会绑定源表和输出表的数据库对象身份。显式事务、已有未提交写入、切换角色、RLS、视图、SQL 过滤、输出表触发器、DDL 命令开始或结束事件触发器和依赖会话设置的字段均在调用者连接中构建。普通已提交表在具备 SHARE 锁权限且工作量足够时可使用后台 worker，构建期间持有源表 SHARE 锁。遇到活跃写入或小任务则在本连接执行。物化结果反映构建者可见的数据，分别对外服务的租户应使用独立的金字塔名称与读取授权。

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

## 从 0.1.0 升级

安装 0.1.1 包后，在同一事务里升级扩展并绑定每个已有金字塔的源表和瓦片表。旧目录只存裸表名，需要维护者确认实际 schema。以下以 `buildings` 为例。

```sql
BEGIN;
ALTER EXTENSION geometry_pyramid UPDATE TO '0.1.1';
UPDATE public.geometry_pyramid_catalog
SET source_rel = 'data.buildings'::regclass,
    storage_rel = 'tiles._gp_tiles_buildings'::regclass
WHERE pyramid_name = 'buildings';
-- 对每个已核实的金字塔重复以上 UPDATE。
DO $$ BEGIN
    IF EXISTS (SELECT FROM public.geometry_pyramid_catalog
               WHERE source_rel IS NULL OR storage_rel IS NULL) THEN
        RAISE EXCEPTION 'unbound pyramids remain';
    END IF;
END $$;
COMMIT;
```

使用字节预算的旧金字塔可先全量刷新一次，记录各层最终生效的抽稀与简化参数。对曾经重试过的旧层，动态读取延迟生成的瓦片时会提示先刷新。局部刷新会复用这些参数并补齐统计。缺少旧参数、刷新后重新超出预算，或更新范围使用非线性投影、跨越日期变更线时，会在本连接全量重建。

## 使用注意

金字塔名、表名和列名限制为 **45 个字符**，以确保派生表名不会超过
PostgreSQL 的 63 字节标识符限制。

每个金字塔的瓦片存储在 `_gp_tiles_<name>` 表中，字段包括：
`id bigserial, z smallint, x integer, y integer, format text, srid integer,
mvt bytea`，主键为 `(z, x, y)`。`format` 当前固定为 `'pbf'`，`srid` 是瓦片
坐标系。业务可以直接读取该表；`ST_Tile` 只返回 `mvt` 列。

`splitSize=0` 是默认值，会物化 `maxZoom` 内所有非空瓦片。对有几何 GiST 索引、无行级策略的稀疏 EPSG:3857 或 EPSG:4326 普通表，构建可跳过没有候选要素的区域。
如果设置 `splitSize > 0`，逐瓦片构建会在普通无行级策略的源表或已准备的数据上检查带缓冲的候选要素数量，低于阈值的区域直接跳过裁剪。最终是否物化取决于裁剪和 LOD 处理后实际写入的要素数。物化表内未命中的瓦片会在每次请求时从源表切片。

`buildRules.filter` 是原始 SQL 布尔表达式，会以 `ST_BuildPyramid` 调用者的
权限执行，定位类似受信任管理员编写的视图定义。扩展会拒绝语句分隔符 `;`，
但这只是防御性检查，不等同于完整 SQL 表达式沙箱。

粗层密集点可在 `buildRules` 加入 `{"minZoom":0,"maxZoom":5,"pointGrid":{"cellPixels":64,"keepField":"important"}}`。
`cellPixels` 使用 MVT 瓦片坐标单位，范围为 1 到 `tileSize`。在过滤和裁剪后，每个有点的网格保留 ID 最小的点；若格内有可选布尔字段 `keepField` 标记的点，则保留该格所有标记点。请使用稳定且唯一的要素 ID。多点、线和面不受网格影响。启用网格的层在构建、刷新和按需读取时使用同一逐瓦片处理，其他层仍可按要素枚举。

## 构建和测试

```bash
cargo check
cargo pgrx run pg17
cargo pgrx test
cargo pgrx package
```

**Verify and release** 工作流在 PR 或手动触发时构建 Ubuntu 24.04 PG17 安装包，验证全新安装和从 `v0.1.0` 源码标签构建的旧版升级，并附上通过验证的包。发布标签在验证通过后上传同一个包。

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
