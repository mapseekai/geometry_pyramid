# splitSize 设计:稀疏瓦片构建期跳过 + 查询期动态生成

日期:2026-06-14
状态:已与用户逐节确认

## 背景与目标

对齐 Alibaba GanosBase `ganos_geometry_pyramid` 的 `splitSize`:构建时**要素数低于 splitSize 的瓦片不物化**(不入库),查询时由 `ST_Tile` 动态从源表实时切片生成(密度边界)。Ganos 哲学:贵的密集瓦片预建、廉价的稀疏瓦片现切。

本特性高度复用已落地的混合 ST_Tile 基础设施(共享管线 `tile_pipeline_from_sql` + 单瓦片 `dynamic::generate_tile`)。核心新增逻辑只有构建侧的一个 HAVING 阈值。

## 已确认的决策

- **in-range 未命中行为**:splitSize>0 时,`z ≤ maxZoom` 的任意存储未命中都调 `generate_tile` 实时生成(真正空区切出空→None;稀疏区切出真瓦片)。不维护 skip 集——空瓦片走一次廉价空切是可接受代价。
- **默认值**:`splitSize` 默认 **10000**(高阈值:默认只物化要素数 ≥10000 的密集瓦片,其余查询时动态生成)。这显著改变默认构建行为(此前物化所有非空瓦片);设 `splitSize=0` 关闭、恢复"物化所有非空"。
- **计数口径**:过滤/抽稀后落入瓦片的最终要素数(`count(*)` over `features` 组,即 MVT 中的要素数),非源表原始行数。
- **门控独立**:`splitSize` 管 z≤maxZoom 的密度动态;`dynamicBeyondMaxZoom` 管 z>maxZoom 的层级动态。两者正交,可单独开关。

## 配置(`src/config.rs`)

`PyramidConfig` 新增:
```rust
/// Tiles whose feature count is below splitSize are NOT materialized at build
/// time; they are sliced on-demand by ST_Tile (density boundary, mirrors Ganos).
/// 0 disables (materialize every non-empty tile). Default 10000.
#[serde(default = "default_split_size", rename = "splitSize")]
pub split_size: u32,
```
`impl Default` 设 `split_size: 10000`;`default_split_size() -> u32 { 10000 }`。

## 构建侧阈值(`src/build.rs`)

`build_zoom_level` 组装 INSERT 的 HAVING 子句改造。当前:
```sql
GROUP BY t.x, t.y
HAVING ST_AsMVT(features.*, $2, $3, 'geom', 'fid') IS NOT NULL
```
改为(splitSize 是 config 整数,直接内联,安全):
```sql
GROUP BY t.x, t.y
HAVING count(*) >= {split_size} AND ST_AsMVT(features.*, $2, $3, 'geom', 'fid') IS NOT NULL
```
- `count(*)` = 该瓦片 `features` 组的最终要素数。
- `splitSize=0` 时退化为 `count(*) >= 0`(恒真)+ 原条件 = 现状(物化所有非空)。一行覆盖开/关两态。
- 阈值只作用于**多瓦片 build 路径**。`generate_tile`(单瓦片动态)**不应用**阈值——无条件切出请求的瓦片(稀疏也切),这正是回退所需。两路继续共用 `tile_pipeline_from_sql` 的中段管线,仅外层分叉(build 有 GROUP BY + 阈值 HAVING;dynamic 是无 GROUP BY 的全局聚合)。

正交性:splitSize 阈值与 `tileSizeBudgetBytes` 重试、`parallel`(每 worker 应用同一 HAVING)、原子 swap 都不冲突——它只改变"哪些瓦片入库"。stats 的 `tile_count` 自然变为已物化(密集)瓦片数。

## ST_Tile 回退控制流(`src/tile.rs`)

仅改 `z ≤ maxZoom` 分支:
```
z ≤ maxZoom:查存储表(to_regclass 守卫)
   命中 → MVT
   未命中:
     cfg.split_size > 0 → generate_tile(...) 实时切
     否则             → None(现状)
```
`z > maxZoom` 分支不变(`dynamicBeyondMaxZoom`)。catalog-first、未知金字塔→None、z<minZoom→None、x/y 越界→None、z∉[0,24]→Err 均不变。`generate_tile` 已存在,自带源表 to_regclass 守卫与标识符再校验,直接复用;其参数(source_table/geom_column/fid_column)来自 catalog-first 已解析的字段。

## 边界清单

| 情形 | 结果 |
|---|---|
| z≤maxZoom 命中物化瓦片 | 存储 MVT |
| z≤maxZoom 未命中, splitSize>0, 该块有要素 | 实时 MVT(稀疏块) |
| z≤maxZoom 未命中, splitSize>0, 该块空 | None |
| z≤maxZoom 未命中, splitSize=0 | None(现状) |
| z>maxZoom | 不变(dynamicBeyondMaxZoom) |
| 未知金字塔 / z<minZoom / x,y 越界 | None |
| z∉[0,24] | Err |

## 测试(`#[pg_test]`)

新增 `split_size_defers_sparse_tiles`:
- 建稀疏金字塔:少量散布点(如 5 个),`splitSize=100`、`maxZoom=4`。各瓦片要素数 < 100 → 几乎不物化。
- 断言存储表 `_gp_tiles_<name>` 在 z≤4 的行数为 0(或远少于点对应瓦片数)——证明 build 跳过稀疏瓦片。
- `ST_Tile(name, 4, <覆盖某点的瓦片>)` → 非空 MVT(in-range 稀疏块动态生成);某空瓦片 → None。
- 对照:`splitSize=0` 重建同数据 → 同一 z=4 瓦片已物化(存储表有行),`ST_Tile` 仍返回该瓦片。
- 复用上一特性的 tile-coord 计算 SQL(从点的 EPSG:3857 投影算 z=4 的 x/y)定位覆盖某点的瓦片。

build 路径阈值无法纯单测(需 PostGIS),由上述 pg_test + 现有 30 个测试(共用同一管线)共同保护。

## 新增 SQL 可见面

| 项 | 类型 | 说明 |
|---|---|---|
| `splitSize` | 新增配置键(默认 10000) | 要素数低于此值的瓦片不物化,查询时动态生成;0 关闭 |

`ST_Tile` / `ST_BuildPyramid` 签名不变,Ganos 兼容。

## 不在本轮范围

- `destSRS` 别名(`tileSRS` 的兼容名)——独立小补丁,另议。
- `buildRules`(逐层 filter/字段规则)。
