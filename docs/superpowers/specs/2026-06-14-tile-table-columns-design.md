# 瓦片表新增 id / format / srid 列设计

日期:2026-06-14
状态:已与用户确认

## 背景与目标

瓦片存储表 `_gp_tiles_<name>` 当前为 `z smallint, x integer, y integer, mvt bytea, PRIMARY KEY (z,x,y)`。直接读表的消费者需要三项额外信息:行序号、瓦片格式后缀、瓦片坐标系。本变更给瓦片表加三列:

- `id` —— 序号列(自增,无业务含义)
- `format` —— 后缀格式,恒为 `'pbf'`
- `srid` —— 瓦片坐标系,等于 `tileSRS`

## 已确认的决策

- **id**:`bigserial` 自增;主键/唯一约束保持 `(z, x, y)` 不变(id 不作主键)。并行构建下 bigserial 序列并发安全。
- **format**:固定字符串 `'pbf'`(当前只产 MVT)。不引入配置键。
- **srid**:取 `tileSRS`(`cfg.tile_srs`,瓦片坐标系,默认 3857),非 sourceSRS。
- **暴露面**:仅新增表列。`ST_Tile` 签名与返回不变(仍只返回 `mvt bytea`),Ganos 兼容。三列供直接读表的消费者使用。

## Schema(`src/storage.rs:tile_table_create_sql`)

单一 schema 源,主表与影子表共用,改为:
```sql
CREATE TABLE "{table}" (
    id     bigserial,
    z      smallint NOT NULL,
    x      integer  NOT NULL,
    y      integer  NOT NULL,
    format text     NOT NULL,
    srid   integer  NOT NULL,
    mvt    bytea    NOT NULL,
    PRIMARY KEY (z, x, y)
)
```

## 写入(`src/build.rs:build_zoom_level` 的 INSERT)

- 列表 `(z, x, y, mvt)` → `(z, x, y, format, srid, mvt)`。
- SELECT 投影加两个值:`'pbf'::text`(format)与 `$8::integer`(srid)。`$8` 在现有 14 参数绑定中已是 `tile_srs`,直接复用,不新增参数。
- `id` 由 `bigserial` 默认值自动填充。
- `GROUP BY t.x, t.y`、`HAVING count(*) >= {split_size} AND ST_AsMVT(...) IS NOT NULL`、其余 14 参数绑定均不变。

示例(变更后):
```sql
INSERT INTO "{storage_table}" (z, x, y, format, srid, mvt)
SELECT $1::smallint, t.x, t.y, 'pbf'::text, $8::integer,
       ST_AsMVT(features.*, $2, $3, 'geom', 'fid')
{pipeline}
GROUP BY t.x, t.y
HAVING count(*) >= {split_size} AND ST_AsMVT(features.*, $2, $3, 'geom', 'fid') IS NOT NULL
```

## 不变的部分

- `ST_Tile`(`src/tile.rs`)继续 `SELECT mvt`,签名与返回不变。
- `generate_tile`(z>maxZoom / splitSize 动态切片)直接返回 MVT、不落表;动态瓦片没有 id/format/srid——三列只描述**已物化**的行。
- `ST_Pyramid` / `ST_Pyramids` 输出不变。

## 影响面与回归

- 改动文件:`src/storage.rs`(schema)、`src/build.rs`(INSERT)、`src/lib.rs`(新测试)。
- 现有 5 处直接查瓦片表的测试只用到 `mvt` / `count(*)` / `octet_length(mvt)`,新增列不影响,继续通过:
  - `drop_rate_reduces_low_zoom_features`、`parallel_build_matches_serial`、`incremental_update_rebuilds_only_dirty_tiles`、`split_size_defers_sparse_tiles`,以及内部 `zoom_stats`/`inserted_count` 的 `count(*)`。

## 测试

新增 pg_test `tile_table_has_id_format_srid`:
- 建一个 `splitSize:0`(确保物化)的小金字塔;
- `SELECT format, srid FROM _gp_tiles_<name> LIMIT 1` → `'pbf'`、`3857`(默认 tileSRS);
- `SELECT count(*) FROM _gp_tiles_<name> WHERE id IS NULL` → 0(id 已填充);
- 可选:用非默认 `tileSRS` 建另一个金字塔,断言 `srid` 跟随该值。

## 不在范围

- 不改 `ST_Tile`/`ST_Pyramid` 的返回结构。
- 不引入可配置 `format`(PNG 等未来再议)。
- 不为动态(未物化)瓦片补充这三列。
