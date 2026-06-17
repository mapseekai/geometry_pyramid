# 首次全量构建加速设计: benchmark 先行 + 构建期预处理表

日期: 2026-06-17
状态: 已与用户确认设计, 待实现计划

## 背景与目标

当前 `ST_BuildPyramid` 的全量构建路径已经支持影子表原子 swap、`splitSize`
稀疏瓦片跳过、动态切片、按 x-strip 并行、简化/最小特征过滤/点抽稀、以及
`tileSizeBudgetBytes` 重试。用户本轮明确优先优化 **首次全量构建速度**,
数据类型按点线面混合处理, 数据规模暂不预设。

目标是先让构建耗时可测量、可比较, 再减少全量构建中的重复工作。当前瓦片
管线每个 tile 都在 LATERAL 子查询中执行源几何过滤、`ST_Transform`、类型
判断、简化和 MVT 编码。对于大范围、多 zoom、混合几何数据, 重复投影和候选
过滤很可能是主要成本。第一轮优化的核心是: 在一次全量 build 中把源几何预
投影到 tile SRS, 给预处理结果建空间索引, 后续切片直接读预处理表。

## 非目标

- 不实现完整 Ganos `buildRules`、按层级字段规则、按层级 filter 或 merge。
- 不实现 Tippecanoe 级别的 density-aware dropping、cluster 或 coalesce。
- 不修改源表 schema, 不要求用户维护生成列或持久 3857 几何列。
- 不改变 `ST_Tile`、动态切片、`ST_UpdatePyramid`、`ST_UpdatePyramidByExtent`
  的行为。
- 第一版不把预处理表扩展到 parallel worker 可见的共享/持久中间表。

## 已确认路线

优先级为构建速度, 场景为首次全量 `ST_BuildPyramid`, 几何类型为混合数据,
数据规模未知。采用路线 B:

1. 先建立 benchmark 和观测口径。
2. 做低风险 SQL 热点优化。
3. 增加构建期临时预处理表, 第一版限定在 `parallel=1` 路径。
4. 用 benchmark 决定是否继续扩展到 parallel worker 可见的预处理表。

## 阶段 1: benchmark 和观测

新增一个手动运行的 benchmark 脚本或 SQL 文件, 不进入默认慢单测路径。数据集
应覆盖:

- 点、线、面三种几何。
- 稠密区域和稀疏区域。
- 至少一个能触发 `splitSize` 跳过的稀疏块。
- 可配置 `minZoom` / `maxZoom`, 默认覆盖一个中等 zoom 范围。

输出指标:

- 源表总行数和几何类型分布。
- 构建配置: zoom 范围、`splitSize`、`simplifyPixels`、`parallel`、
  `preparedBuild`。
- `ST_BuildPyramid` 总耗时。
- 每 zoom 的 `tile_count`、`total_bytes`、`max_tile_bytes`、`build_ms`、
  `attempts`。

第一版不写死性能百分比目标。验收条件是能稳定比较 baseline 与
`preparedBuild=true` 的同一数据集结果。

## 阶段 2: 低风险 SQL 优化

在引入新路径前, 先处理现有 SQL 的明确浪费, 作为 benchmark 后的第一轮小步
优化:

- 避免 `build_zoom_level` 中 `ST_AsMVT(features.*, ...)` 同时出现在 SELECT
  和 HAVING, 导致同一 tile 聚合表达式重复计算。
- 保持 `splitSize` 语义不变: 仍然用最终进入 MVT 的 feature 组计数。
- 保持输出列 `(z, x, y, format, srid, mvt)` 和现有 stats 行为不变。

该阶段必须有行为等价测试保护, 作为后续预处理表改造的安全基线。

## 阶段 3: preparedBuild 配置

新增配置键:

```json
{
  "preparedBuild": false
}
```

语义:

- `false`: 默认值, 完全使用现有构建路径。
- `true` 且 `parallel=1`: 使用构建期临时预处理表路径。
- `true` 且 `parallel>1`: 第一版自动回退现有 parallel 路径并发出 NOTICE。

选择默认 `false` 的原因是该能力改变构建内部执行计划, 需要 benchmark 和行为
等价测试证明稳定后再考虑默认开启。自动回退 parallel 的原因是它是优化开关,
不应该破坏已有并行用户的构建。

## 阶段 4: 构建期临时预处理表

新增内部步骤 `prepare_source_for_build`, 仅用于全量 build 且
`preparedBuild=true && parallel=1`。

预处理表建议使用 PostgreSQL session-local temporary table:

```sql
CREATE TEMP TABLE <prep_table> ON COMMIT DROP AS
SELECT
  src."<fid_column>"::bigint AS fid,
  ST_Transform(src."<geom_column>", $tile_srs) AS geom_tile,
  GeometryType(src."<geom_column>") AS geom_type,
  <requested attributes>
FROM "<source_table>" src
WHERE src."<geom_column>" IS NOT NULL;
```

随后:

```sql
CREATE INDEX ON <prep_table> USING gist (geom_tile);
ANALYZE <prep_table>;
```

字段边界:

- `fid`: 用作 MVT feature id, 保持现有 `fid_column` 语义。
- `geom_tile`: 已投影到 `tileSRS` 的 geometry, 后续所有 build tile 直接使用。
- `geom_type`: 用于跳过点简化、区分 polygon/line/point 过滤, 避免每瓦片重复
  `GeometryType(...)`。
- attributes: 只复制当前 `cfg.attributes` 中通过标识符校验的列。

第一版不单独存 bbox 列。候选过滤先依赖 `geom_tile` 的 GiST 索引和
`geom_tile && ST_TileEnvelope(...)`。如果 benchmark 表明 bbox 仍是瓶颈, 后续
再评估显式 bbox / tile coverage 表。

## 阶段 5: build 专用 tile pipeline

保留现有 `tile_pipeline_from_sql` 给动态切片和非 prepared build 使用。新增
build 专用 SQL 构造器, 例如 `tile_pipeline_from_prepared_sql(prep_table, attrs)`,
核心差异:

- `FROM "<source_table>" src` 改为 `FROM <prep_table> prep_src`。
- 候选过滤从
  `src.geom && ST_Transform(ST_TileEnvelope(...), sourceSRS)` 改为
  `prep_src.geom_tile && ST_TileEnvelope(..., margin => tileExtend / tileSize)`。
- 简化输入从 `ST_Transform(src.geom, tileSRS)` 改为 `prep_src.geom_tile`。
- 类型判断使用 `prep_src.geom_type`。
- `ST_AsMVTGeom` 和 `ST_AsMVT` 的输出语义保持不变。

动态切片不复用 prepared 表。`ST_Tile` 对 splitSize 未物化瓦片和
`z > maxZoom` 的实时生成仍然从源表现切, 保持只读、无缓存、无副作用。

## 事务、并行和清理

第一版 prepared path 只在 `parallel=1` 启用。此时 shadow 表和 temp prep 表都在
协调者事务/session 内可见, 失败后事务回滚, temp 表也会在 commit/rollback 或
session 结束时清理。

`parallel>1` 暂不启用 prepared path, 因为 background worker 是独立 backend,
看不到协调者的 temp table。若要支持, 后续应设计 worker 可见的普通 shadow
prepared 表, 并处理:

- 并发命名。
- build 成功和失败后的清理。
- worker 在不同事务中对 prepared 表和 shadow 表的可见性。
- 与 `tileSizeBudgetBytes` 重试的交互。

## 测试设计

行为等价测试:

- 用小型混合几何数据集分别构建 `preparedBuild=false` 与
  `preparedBuild=true, parallel=1`。
- 比较同一批 `(z, x, y)` 上 `ST_Tile` 的非空性和输出 byte。
- 如果 PostGIS 聚合顺序导致 byte 不稳定, 退到解码后 feature 数/属性/几何
  bbox 的语义比较。

配置边界测试:

- `preparedBuild=false` 保持现有路径。
- `preparedBuild=true, parallel=1` 走 prepared path。
- `preparedBuild=true, parallel>1` 自动回退并 NOTICE, 构建仍成功。
- `splitSize=0` 和默认 `splitSize=10000` 都覆盖。

benchmark 验收:

- 同一数据集下能输出 baseline 与 prepared build 的总耗时和 per-zoom stats。
- 构建结果的 materialized tile 数与关键瓦片可读性保持一致。
- benchmark 脚本文档化运行方式和输出解释。

## 实现顺序

1. 增加 benchmark SQL/脚本和运行说明。
2. 小步优化现有 `build_zoom_level` 的重复 MVT 聚合。
3. 在 `PyramidConfig` 增加 `preparedBuild` 配置和校验/默认值。
4. 新增 `prepare_source_for_build` 和 prepared table SQL 构造器。
5. 在全量 build 的 `parallel=1` 路径接入 prepared table。
6. 增加行为等价、配置边界和 benchmark 文档。
7. 运行 pgrx 测试和 benchmark, 用结果决定后续是否默认开启或扩展 parallel。

## 风险与缓解

- **MVT byte 顺序差异**: prepared 表改变查询计划后, 聚合输入顺序可能变化。
  测试先尝试 byte 相等, 不稳定时改为语义比较。
- **属性列类型保持**: attributes 从源表复制到 temp 表时必须保留 PostgreSQL 类型,
  不能全部转 text, 否则 MVT 属性类型会变化。
- **temp 表名称冲突**: 使用 pg temp schema 和 pid/递增序号命名, 并通过 quoted
  identifier 构造。
- **parallel 回退不明显**: `preparedBuild=true, parallel>1` 必须 NOTICE, 避免
  用户误以为 prepared path 已生效。
- **预处理表建索引成本过高**: benchmark 要同时记录预处理阶段和 per-zoom 构建
  阶段耗时。若小数据集上索引成本高于收益, 保持默认 false。
