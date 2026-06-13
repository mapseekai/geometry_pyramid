# 混合 ST_Tile 设计:预处理优先 + 超 maxZoom 实时切片

日期:2026-06-13
状态:已与用户逐节确认

## 背景与目标

当前 `ST_Tile(name, z, x, y)` 只查预物化的 `_gp_tiles_<name>` 存储表,未命中(含表不存在)行为不符合文档承诺。对齐 Alibaba GanosBase `ganos_geometry_pyramid` 的混合行为:**金字塔物化到 `maxLevel`,超过该层级的瓦片实时从源表切片生成 MVT**(Ganos 文档:"超过 maxLevel 则会实时读取数据生成 mvt")。

本特性把 `ST_Tile` 改为:物化范围内查表,`z > maxZoom` 时(开关开启)用与构建相同的 PostGIS 管线对单个瓦片实时切片返回。

## 已确认的范围决策

- **触发条件**:仅按层级(`z > maxZoom`)。不实现 Ganos 的 `splitSize`(密度阈值动态构建)与 `buildRules`(逐层规则)——列为可选后续。
- **缓存**:不缓存、每次现算。`ST_Tile` 保持只读、无副作用,与稀疏金字塔"省存储"初衷一致。
- **边界参数**:复用现有 `maxZoom` 作为物化上限(= Ganos `maxLevel`)。不新增层级参数。
- **启用方式**:新增配置开关 `dynamicBeyondMaxZoom`,默认 `true`(对齐 Ganos);关闭后 `z > maxZoom` 返回 NULL,供需要严格金字塔/限制查询开销的用户使用。

## 架构与模块布局

```
src/
├── build.rs       # 抽出可复用的"单瓦片特征管线 SQL";build 路径继续用它做多瓦片 INSERT
├── dynamic.rs     # 新增:generate_tile() —— 用同一段管线对单个 (z,x,y) 实时切片,返回 MVT bytea
├── tile.rs        # ST_Tile 改为混合:物化优先 + z>maxZoom 实时回退
├── config.rs      # 新增 dynamicBeyondMaxZoom 开关
```

### SQL 核心抽取(关键)

现有 `build_zoom_level` 的 INSERT SQL 结构:

```
INSERT INTO storage(z,x,y,mvt)
SELECT $1, t.x, t.y, ST_AsMVT(features.*, ...)
FROM generate_series(xmin,xmax) xs CROSS JOIN generate_series(ymin,ymax) ys
     CROSS JOIN LATERAL (SELECT xs.x, ys.y) t
     CROSS JOIN LATERAL ( <features: ST_AsMVTGeom + prep(transform/simplify) + 过滤谓词> ) features
WHERE features.geom IS NOT NULL
GROUP BY t.x, t.y HAVING ST_AsMVT(...) IS NOT NULL
```

把 `t` 之后的整段(`CROSS JOIN LATERAL (features...)` + WHERE + 过滤谓词 + `ST_AsMVT` 包裹)做成参数化构造器 `tile_pipeline_sql(table, geom_column, fid_column, attrs)`。两条路径只在两处分叉:

| | 瓦片来源 `t` | 外层动作 |
|---|---|---|
| build(多瓦片) | `generate_series × generate_series` | `INSERT ... SELECT ..., ST_AsMVT(...) GROUP BY t.x,t.y` |
| dynamic(单瓦片) | `(SELECT $x::int x, $y::int y) t` | `SELECT ST_AsMVT(...)` 直接返回 bytea |

中间几何管线(投影 / 简化 / 最小要素 / 抽稀 / `ST_AsMVTGeom` / `$11..$14` knob 绑定)**两路共用同一份**——这是保证 maxZoom 边界两侧视觉一致的根本。参数绑定槽位($1 z、$3 extent、$8 tileSRS、$9 tileExtend、$10 sourceSRS、$11..$14 knobs)在两路保持一致,dynamic 额外绑 x/y。

高 zoom 处管线天然正确:`drop_threshold_at(z)` 在 `z >= max_zoom` 返回 1.0(全保留),`simplify_tol_m` 随 z 增大趋近 0(几乎不简化),动态瓦片自动高保真,无需特殊处理。

## 配置

`config.rs` 的 `PyramidConfig` 新增:

```rust
/// 当请求 z > maxZoom 时,实时从源表切片而非返回 NULL。默认 true(对齐 Ganos)。
#[serde(default = "default_dynamic_beyond_max_zoom", rename = "dynamicBeyondMaxZoom")]
pub dynamic_beyond_max_zoom: bool,
```

`Default` 设 `true`;`default_dynamic_beyond_max_zoom()` 返回 `true`。

## ST_Tile 控制流(签名不变)

```
1. validate_identifier(name);  z ∉ [0,24] → Err;  x,y 越界 → Ok(None)
2. 查 catalog 取该 pyramid 行(PK 查询):
   - 不存在 → Ok(None)               # 未知金字塔(顺带修掉旧的"表不存在报错"缺口)
   - 存在 → 解析 source_table / geom_column / fid_column / config(cfg)
3. 分支:
   a. z ≤ cfg.max_zoom:查存储表 _gp_tiles_<name>(to_regclass 守卫);命中→MVT;未命中→None
   b. z > cfg.max_zoom:
        cfg.dynamic_beyond_max_zoom == false → None
        否则 → dynamic::generate_tile(...) 实时切片(源表那块无数据 → None)
   c. z < cfg.min_zoom → None         # 低于物化下限,罕见配置
```

**catalog-first 取舍**:分支前必须知道 `max_zoom` 与 `dynamic` 开关(都在 config 里),故先查目录。代价是物化命中热路径多一次目录 PK 查询(对小表纳秒级)。若日后实测为瓶颈,可优化为"存储表优先,miss 再查目录决定动态"——当前按 catalog-first 求清晰。

`dynamic::generate_tile(pyramid_name, source_table, geom_column, fid_column, cfg, z, x, y) -> Result<Option<Vec<u8>>>`:`ZoomKnobs::from_config(cfg, z)` 重建 knob → 第 1 节抽取的单瓦片 SQL → `Spi::get_one_with_args` 取 `Option<Vec<u8>>`;源表用 to_regclass 守卫。

## 错误处理与边界

`ST_Tile` 保持只读、无副作用,标注 `STABLE`。除 `z ∉ [0,24]` 报 `Err` 外,所有"取不到"语义统一收敛为 `Ok(None)`。

| 情形 | 结果 |
|---|---|
| 未知金字塔(不在 catalog) | `None` |
| z∈物化范围、命中 | 存储表 MVT |
| z∈物化范围、稀疏空 | `None`(不动态生成) |
| z>maxZoom、开关开、源表有数据 | 实时 MVT |
| z>maxZoom、开关开、源表那块无数据 | `None` |
| z>maxZoom、开关关 | `None` |
| z<minZoom | `None` |
| z∉[0,24] | `Err` |

## 测试

`#[pg_test]`(需 PostGIS):

**`dynamic_tile_beyond_max_zoom`**:
- 建 maxZoom=4 的金字塔(若干点);`ST_Tile(name,4,…)` 命中物化瓦片非空;
- `ST_Tile(name,6,x,y)`(覆盖某点的高 zoom 瓦片)→ 非空 MVT(动态生成走通);源表无数据的瓦片 → `None`;
- 关掉开关重建(`{"maxZoom":4,"dynamicBeyondMaxZoom":false}`)→ 同一 z=6 请求 → `None`;
- `ST_Tile('no_such_pyramid',0,0,0)` → `None`(回归旧缺口,且不报错)。

**边界一致性(轻量)**:`ST_Tile` 在 z=maxZoom(物化)和 z=maxZoom+1(动态)各取覆盖同一点的瓦片,断言两者都非空且能被 PostGIS 解码(逐字节一致不可能,zoom 不同)。

SQL 抽取无法纯单测,由上述 pg_test + 现有 28 个 build 测试(共用同一管线)共同保护。

## 新增 SQL 可见面

| 项 | 类型 | 说明 |
|---|---|---|
| `dynamicBeyondMaxZoom` | 新增配置键(默认 true) | 控制 z>maxZoom 的实时切片 |

`ST_Tile` 签名不变,Ganos 兼容。

## 不在本轮范围(可选后续)

- `splitSize`:要素数低于阈值的瓦片查询时动态构建(密度边界)。
- `buildRules`:逐层 filter / 字段规则。
- 动态瓦片缓存回写。
