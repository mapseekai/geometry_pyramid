# 候选过滤优化:`&&` + envelope margin 设计

日期:2026-06-14
状态:已与用户确认

## 背景与目标

瓦片管线 `tile_pipeline_from_sql`(`src/build.rs`,被构建路径与动态切片路径共用)用一段 WHERE 过滤源表候选行。当前用 `ST_Intersects(geom, envelope)`,EXPLAIN 实测显示:GiST 索引先用 `&&` 选出候选(本例 24154 行),再逐行做精确 `_ST_Intersects` 复检——而复检只剔掉 2 行,对瓦片渲染是冗余的(`ST_AsMVTGeom` 会把不落瓦片的几何裁成 NULL,后续 `features.geom IS NOT NULL` 兜底)。样例瓦片:`ST_Intersects` 12.6ms vs 纯 `&&` 3.0ms。

同时,当前过滤用的 `ST_TileEnvelope` 不含缓冲余量,可能漏掉应出现在瓦片缓冲区(`tileExtend`)的边缘几何,导致跨瓦片边界的线/面渲染出现接缝。

目标:对齐 Martin 的高效做法——(1) 过滤用纯 `&&`(只走索引 bbox);(2) 过滤包络加 `margin` 余量纳入缓冲区边缘几何。

## 已确认的决策

- 仅改 `tile_pipeline_from_sql` 的源表过滤 WHERE;`ST_AsMVTGeom` 调用不动。
- margin = `tileExtend / tileSize`(= `$9 / $3`,均为已绑定参数),把 MVT 缓冲宽度(tile 单位)换算成包络放大比例。
- 共享管线,构建与动态切片同时生效。
- 不新增 SQL 参数、不新增配置键。

## 改动(`src/build.rs:tile_pipeline_from_sql`)

当前:
```sql
FROM "{table}" src
WHERE src."{geom_column}" IS NOT NULL
  AND ST_Intersects(
         src."{geom_column}",
         ST_Transform(ST_TileEnvelope($1, t.x, t.y), $10)
      )
```
改为:
```sql
FROM "{table}" src
WHERE src."{geom_column}" IS NOT NULL
  AND src."{geom_column}" && ST_Transform(
         ST_TileEnvelope($1, t.x, t.y, margin => ($9::float8 / $3::float8)),
         $10
      )
```

参数复用(不新增):`$1` z、`$3` tile_size、`$9` tile_extend、`$10` source_srs。`&&` 两侧 SRID 一致(`src.geom` 为 source_srs;`ST_Transform(..., $10=source_srs)`),与原 `ST_Intersects` 相同。

`ST_AsMVTGeom(prep.gtile, ST_TileEnvelope($1, t.x, t.y), $3, $9, true)` 保持不变——MVT 几何的包络仍是精确瓦片,缓冲由其 `$9` 参数负责;margin 仅用于扩大"候选行"集合。

## 依赖

- PostGIS 3.1+ 的 `ST_TileEnvelope(..., margin)`。本环境 PostGIS 3.6.4,支持。

## 效果与正确性

- **更快**:过滤只走 GiST 索引的 `&&`,去掉逐行精确相交复检。
- **边缘无缝**:`&&` 候选集 ⊇ 精确相交集,且 margin 放大包络 → 落在缓冲区的边缘几何进入候选,经 `ST_AsMVTGeom`(buffer=`$9`)裁入缓冲区,跨瓦片边界的线/面不再出现接缝。
- **输出变化**:瓦片边缘会多出缓冲区几何。逐字节/大小对比的测试均为"同管线两侧对比",仍成立:
  - `parallel_build_matches_serial`:串行与并行同用此管线 → 两侧同变,仍逐字节相等。
  - `incremental_update_rebuilds_only_dirty_tiles`:快照前后同管线。
  - `drop_rate_reduces_low_zoom_features`:z0 < z4 不等式仍成立。
  - `split_size_defers_sparse_tiles`:单要素瓦片不会因 margin 越过 splitSize=100。

## 测试

新增 pg_test `tile_filter_uses_bbox_and_margin`:
- 构造一个几何**刚好落在某瓦片外、但在其缓冲带内**的要素(例如一个细长多边形/线,边界紧贴瓦片边);splitSize:0 物化。
- 断言该瓦片的 MVT 非空(margin 让边缘几何进入候选并被裁入缓冲)——证明缓冲带被填充。
- 备选/补充:对一个明确包含数据的瓦片断言 `&&` 路径仍正常出非空瓦片(回归 `&&` 改写不破坏基本切片)。
- 现有 5 处查表测试不变,预期继续通过(同管线两侧对比)。

## 影响面

- 改动文件:`src/build.rs`(`tile_pipeline_from_sql` 一处 WHERE)+ `src/lib.rs`(新 pg_test)。
- 不动:`storage.rs`、`tile.rs`、`config.rs`、`dynamic.rs`(后者复用同一管线,自动受益)。

## 不在范围

- 不改 `ST_AsMVTGeom` 的 buffer/extent 行为。
- 不引入可配置 margin(固定派生自 tileExtend/tileSize)。
