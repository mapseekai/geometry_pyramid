# 按空间范围更新金字塔(ST_UpdatePyramidByExtent)设计

日期:2026-06-15
状态:已与用户确认

## 背景与目标

现有 `ST_UpdatePyramid(name, where_clause)` 按属性条件做增量刷新:从匹配行的 `ST_Extent` 算脏矩形,逐 zoom DELETE + 重切。它有一个已记录的局限——纯删除 / 跨瓦片移动时,若 where_clause 匹配不到被删/移出的行,旧瓦片清不掉。

新增**按空间范围(bbox)更新**:调用者直接给一个范围,系统把该范围内的瓦片按**当前源数据**整体重切。它不依赖匹配变更行,因此天然正确处理该区域内的插入/更新/删除/移动——正是 where_clause 路径局限的克星。

## 已确认决策

- **API**:新函数 `ST_UpdatePyramidByExtent(name, xmin, ymin, xmax, ymax, srid)`,与现有 `ST_UpdatePyramid(name, where_clause)` 并存(不重载)。
- **srid**:必填、显式。bbox 坐标按该 srid 解释,内部转到 tileSRS 再算脏矩形。
- **复用**:复用 `incremental` 的逐 zoom 脏矩形 DELETE + 重切 + 刷 stats 机制;单事务、原地、失败回滚、无影子表;预算重试不启用(同 where_clause 增量)。

## API

```
boolean ST_UpdatePyramidByExtent(
    name text,
    xmin double precision, ymin double precision,
    xmax double precision, ymax double precision,
    srid integer)
```
pgrx 注册名小写 `st_updatepyramidbyextent`(可不加引号调用)。返回 `true` 成功(含退化 bbox 的 no-op),未知金字塔返回 `false`。

## 架构(`src/incremental.rs` 重构 + `src/pyramid.rs` 新函数)

### incremental.rs

抽出共享的逐 zoom 重切:
```rust
/// Rebuild every tile intersecting the given tile-SRS bbox, across
/// minZoom..=maxZoom: DELETE the dirty rectangle then re-slice it in place,
/// refreshing per-zoom stats. Caller's transaction; rollback-safe.
pub(crate) fn rebuild_extent(
    pyramid_name: &str,
    source_table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
    xmin_m: f64, ymin_m: f64, xmax_m: f64, ymax_m: f64,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>>
```
这就是把现有 `update()` 里 extent 算出来之后的那段循环原样搬进来。

- `update(..., where_clause)`:保留 pre-flight 校验 + 从 `WHERE (clause)` 算 `ST_Extent`(tileSRS);匹配为空 → notice + `Ok(0)`;否则调 `rebuild_extent(...)`。行为不变。
- 新增 `update_extent(..., xmin, ymin, xmax, ymax, srid)`:把入参 bbox 从 `srid` 转到 tileSRS 取四边,再调 `rebuild_extent(...)`:
  ```sql
  SELECT json_build_array(ST_XMin(g), ST_YMin(g), ST_XMax(g), ST_YMax(g))::text
  FROM (SELECT ST_Transform(ST_MakeEnvelope($1,$2,$3,$4,$5), $6) AS g) s
  ```
  `$1..$4` = xmin/ymin/xmax/ymax,`$5` = srid,`$6` = cfg.tile_srs。`ST_MakeEnvelope` 自动规整 min/max。转换结果 NULL(异常)→ `Ok(0)` no-op。

### pyramid.rs

```rust
#[pg_extern(name = "st_updatepyramidbyextent")]
fn st_update_pyramid_by_extent(
    name: &str, xmin: f64, ymin: f64, xmax: f64, ymax: f64, srid: i32,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>
```
- `validate_identifier(name)?`
- catalog-first 取目录行(`SELECT MAX(json_build_object('source_table',...,'geom_column',...,'fid_column',...,'config',...)::text) FROM geometry_pyramid_catalog WHERE pyramid_name=$1`);`None` → `Ok(false)`(未知金字塔,与已修复的 st_update_pyramid 同款 MAX 包裹,避免 0 行报错)。
- 解析 source_table/geom_column/fid_column/config(NULL 字段 `ok_or` 干净报错,同现有写法)。
- `let total = incremental::update_extent(name, &source_table, &geom_column, &fid_column, &cfg, xmin, ymin, xmax, ymax, srid)?;`
- `UPDATE geometry_pyramid_catalog SET built_at = now() WHERE pyramid_name = $1`。
- `notice!("geometry_pyramid: pyramid '{}' range-refreshed ({} tiles)", name, total)`;`Ok(true)`。

## 数据流 / 语义

- 对 `minZoom..=maxZoom` 每层:`TileRect::from_bbox_m(z, bbox_in_tile_srs)` → DELETE 该矩形瓦片 → `build_zoom_level` 按当前源数据原地重切 → 刷该层 stats。
- **按当前源重切**:无论该区域的行是新增、修改、删除还是移动,重切都反映最新状态——故能清掉被删/移出的旧要素。
- 全程单事务,失败回滚,主表始终一致。

## 边界

| 情形 | 结果 |
|---|---|
| 正常 bbox | 重切区域内各层瓦片,返回重建瓦片数 |
| 退化 bbox(clamp 后无有效瓦片 / 转换 NULL) | no-op,`Ok(true)`,0 瓦片 |
| 未知金字塔 | `Ok(false)` |
| 非法 srid / `ST_Transform` 失败 | 干净报错上抛 |
| `name` 非法标识符 | `validate_identifier` 报错 |

## 测试(`#[pg_test]`)

`update_by_extent_rebuilds_region`:
- 建点集金字塔(splitSize:0 物化),记录某瓦片当前 mvt。
- **删除**某点;`ST_UpdatePyramidByExtent(name, <覆盖该点的 bbox>, 4326)`。
- 断言:该点所在瓦片重切后不再含该点(用大小或重切后 `ST_Tile` 与删除前快照不同验证),证明范围更新处理了删除——这是 where_clause 路径做不到的。
- 断言:bbox 之外某瓦片字节未变(只动脏区域)。
- 附:`ST_UpdatePyramidByExtent('unknown', ...)` → `false`(不报错)。

## 影响面

- 改:`src/incremental.rs`(抽 `rebuild_extent` + 新 `update_extent`)、`src/pyramid.rs`(新 `#[pg_extern]`)、`src/lib.rs`(测试)、`README.md`(SQL surface 加一行 + 说明范围更新能处理删除/移动)。
- 不动:`build.rs`、`storage.rs`、`tile.rs`、`config.rs`、`dynamic.rs`。

## 不在范围

- 不改 where_clause 路径的现有行为(只是把内部循环抽成共享函数)。
- 不支持几何参数重载(本轮用四坐标 + srid)。
- 不引入按范围的并行/预算重试。
