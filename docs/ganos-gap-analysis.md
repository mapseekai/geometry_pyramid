# 现状矢量金字塔 vs. 阿里云 Ganos 差距分析与优化方案

> 日期：2026-06-26
> 范围：`geometry_pyramid`（本项目）对标阿里云 GanosBase `ganos_geometry_pyramid`（矢量金字塔 / 矢量快显）。
> 性质：差距分析 + 优化策略，不含本轮代码改动。

> 当前状态（2026-07-02）：这是历史差距分析文档，部分结论已被后续实现覆盖。阅读下文前先看
> “0. 当前实现核对”；正文中“未落地/缺失”的判断保留为当时背景。

---

## 0. 当前实现核对（2026-07-02）

| 项目 | 当前状态 | 说明 |
|---|---|---|
| `buildRules.filter` | ✅ 已实现 | 支持按 zoom band 注入 raw SQL filter，构建路径、prepared 路径、并行路径和动态切片路径共享语义。 |
| `buildRules.attrFields` | ❌ 未实现 | 当前仍是全局 `attributes`，没有每条 rule 的属性列选择。 |
| `buildRules.merge` / coalesce | ❌ 未实现 | README roadmap 仍保留 coalesce-densest。 |
| `preparedBuild` | ✅ 已实现并增强 | 已有 `src/prepared.rs`，并支持 parallel worker 可见的 committed prepared table。 |
| `featureMajorPoints` | ✅ 已实现 | 纯 POINT、`tileSRS=3857`、单 backend fast path；`featureMajorMinZoom` 控制启用门槛。 |
| `featureDrivenEnum` | ✅ 已实现 | all-geometry envelope-driven enumeration，自带全局和 per-zoom gate。 |
| `parallel` + fast path | ✅ 已实现 | prepared / feature-driven 路径用 worker 可见 prepared table 和 Morton blocks；baseline 保留 x-strips。 |
| `ST_Tile(name, 'z_x_y')` | ❌ 未实现 | 当前仍只有 `ST_Tile(name, z, x, y)`。 |
| `ST_Tile(name, x, y, z)` Ganos 顺序 | ❌ 未实现 | 当前签名顺序仍是 `z,x,y`。 |
| `ST_DeletePyramid` alias | ❌ 未实现 | 当前公开接口仍是 `ST_DropPyramid`。 |
| `ST_AsPng` | ❌ 未实现 | 仍只输出 MVT。 |
| 3D 快显 | ❌ 未实现 | 仍是 2D MVT 管线。 |
| schema-qualified source table | ❌ 未实现 | 仍使用受限裸标识符和 `search_path`。 |
| 动态切片缓存 | ❌ 未实现 | `ST_Tile` 动态路径仍按请求即时切片。 |

## 1. 结论速览（TL;DR）

本项目已经把 Ganos 矢量金字塔的**主干能力**做得相当完整：原子影子表 swap、稀疏金字塔
（`splitSize`）、超出 `maxZoom` 的动态切片、按 zoom × x-strip 的并行构建、谓词/范围两条
增量刷新路径、以及一整套 **tippecanoe 风格**的泛化旋钮（简化、最小要素过滤、点抽稀、单瓦
片字节预算重试）。在“构建-存储-取瓦片”这条核心链路上，二者是对齐的。

真正的差距集中在三类：

| 级别 | 差距 | 一句话 |
|---|---|---|
| **P0 功能** | `buildRules` 分级规则 | Ganos 能按层级配置 `filter`/`attrFields`/`merge`，本项目所有 zoom 共用一套全局规则。**最大差距**。 |
| **P0 功能** | `ST_AsPng` 服务端渲染 | Ganos 能直接出 PNG 栅格瓦片（带 style），本项目只出 MVT。 |
| **P0 功能** | 3D 矢量快显 | Ganos 有 `ST_AsMVT3D` / `Geometry3D`，本项目纯 2D。 |
| **P1 兼容** | `ST_Tile` 参数顺序 / key 重载 | Ganos 是 `ST_Tile(name,x,y,z)` 与 `ST_Tile(name,'z_x_y')`；本项目是 `ST_Tile(name,z,x,y)`，**顺序相反且无 key 形式**——`.control` 自称 “Ganos-compatible API” 但此处不兼容。 |
| **P1 兼容** | `ST_DeletePyramid` 命名 | Ganos 叫 `ST_DeletePyramid`，本项目叫 `ST_DropPyramid`。 |
| **P2 默认值** | `splitSize` / `tileExtend` / `parallel` 默认值 | 默认值与 Ganos 取向不同，影响开箱体验与读放大。 |
| **P3 性能** | 首次全量构建速度 | 每瓦片重复投影/过滤；`preparedBuild` 方案已设计但未落地。 |
| **P4 运维** | 动态切片无缓存、swap OID 瞬时窗口、无 schema 限定名 | 高并发瓦片服务下的工程细节。 |

下文逐项展开“现状 → Ganos → 差距 → 方案 → 改动点 / 工作量 / 风险”。

---

## 2. 对比基线：两边能力盘点

### 2.1 本项目 `geometry_pyramid` 现状

**SQL 接口（`src/pyramid.rs`、`src/tile.rs`、`sql/bootstrap.sql`）**

| 函数 | 签名 | 说明 |
|---|---|---|
| `ST_BuildPyramid` | `(table, geom, fid, config jsonb) -> bool` | 注册并构建，影子表原子 swap。 |
| `ST_UpdatePyramid` | `(name, where_clause) -> bool` | 谓词增量；空串=全量重建。 |
| `ST_UpdatePyramidByExtent` | `(name, xmin, ymin, xmax, ymax, srid) -> bool` | 按 bbox 重切，正确处理删除/跨瓦片移动。 |
| `ST_DropPyramid` | `(name) -> bool` | 删除金字塔及其存储表。 |
| `ST_Pyramid` | `(name) -> jsonb` | 元数据 + **逐层构建统计**（tile/byte/ms/attempts）。 |
| `ST_Tile` | `(name, z, x, y) -> bytea` | MVT；≤maxZoom 读物化，>maxZoom 动态切。 |
| `ST_Pyramids` | `() -> SETOF(name, source_table, built_at, tile_count)` | 列举。 |
| `geometry_pyramid_version` | `() -> text` | 版本号。 |

**`config` 配置键（`src/config.rs`）**

`name`、`sourceSRS`(4326)、`tileSRS`(3857)、`tileSize`(4096)、`tileExtend`(8)、
`minZoom`/`maxZoom`(0/18)、`dynamicBeyondMaxZoom`(true)、`parallel`(1)、`attributes`([])、
`simplifyPixels`(1.0)、`minFeatureSizePixels`(0.0)、`dropRate`(1.0)、
`tileSizeBudgetBytes`(0)、`maxTileSizeAttempts`(5)、`buildTimeoutMs`(0)、`splitSize`(10000)。

**构建管线（`src/build.rs::tile_pipeline_from_sql`，README “Generation rules”）**
bbox 预过滤（源 SRS 下 `&&` 缓冲扩边瓦片包络，走源表 GiST）→ `ST_Transform` 投影 →
`ST_SimplifyPreserveTopology`（按 zoom 像素容差，点跳过）→ 最小要素过滤（面按面积/线按长度）
→ 确定性点抽稀（`hashtextextended`，与 tippecanoe `-r` 同义）→ `ST_AsMVTGeom` 量化 →
`ST_AsMVT` 聚合 → 仅非空且要素数 ≥ `splitSize` 的瓦片入库 → 可选字节预算重试。

**工程亮点（本项目相对 Ganos 的“正资产”）**
- **原子影子表 swap**（`src/shadow.rs`）：读者永不见半构建状态，重建失败保留旧瓦片。
- **逐层统计**进 `ST_Pyramid`（`src/stats.rs`）：Ganos 文档未见同等粒度。
- **`ST_UpdatePyramidByExtent`**：显式 bbox 重切，覆盖删除/移出场景。
- **tippecanoe 级泛化旋钮 + 字节预算自动收紧**（`src/build.rs::ZoomKnobs::tighten`）：
  这套“按目标字节自适应收紧简化/最小尺寸/抽稀”的能力，比 Ganos 暴露的 config 更细。

**输出格式**：仅 MVT（`format='pbf'`），存储于 `_gp_tiles_<name>(id,z,x,y,format,srid,mvt)`。

### 2.2 阿里云 Ganos `ganos_geometry_pyramid`

依据官方文档与开发者文章（见 §7 参考资料）：

**SQL 接口**

| 函数 | 签名（文档原型） | 本项目对应 |
|---|---|---|
| `ST_BuildPyramid` | `(table, geom_col, id_col, config)`；`id` 需 int4/int8 | ✅ 有 |
| `ST_Tile` | `(name, key)` 与 `(name, x, y, z)`；key 为 `'z_x_y'` | ⚠️ 顺序/形式不一致 |
| `ST_AsPng` | `(name, key, style)` 与 `(name, x, y, z, style)`；style=JSON | ❌ 无 |
| `ST_UpdatePyramid` | 表更新后刷新；支持 bbox 局部更新（<1% 面积） | ✅ 有（两条路径） |
| `ST_DeletePyramid` | 删除金字塔 | ⚠️ 本项目叫 `ST_DropPyramid` |
| `ST_AsMVT` / `ST_AsMVTGeom` | 增强版 MVT 编码 | 〜 复用 PostGIS |
| `ST_AsMVT3D` / `ST_AsMVTGeom3D` | 3D 矢量瓦片（`Geometry3D`） | ❌ 无 |

**`config` 配置键（Ganos）**
`sourceSRS`、`tileSRS`、`tileSize`(4096)、`tileExtend`(示例 128/256)、`splitSize`、
`maxLevel`（金字塔层高；超出实时读）、`parallel`（默认 0 = 不限）、以及 **`buildRules`**。

**`buildRules`（分级构建规则，数组，逐条对一组 `level` 生效）**
- `level`：作用层级，如 `[0,1,2,3,4,5]`。
- `filter`：该层 SQL 过滤，如 `"code=1"`（甚至 `"1!=1"` 生成空瓦片）。
- `attrFields`：该层写入瓦片的属性，如 `["name","width"]`。
- `merge`：按属性合并相邻/同值要素，降低粗层级要素量（coalesce）。

> 典型示例：0–5 层 `filter:"1!=1"` 出空瓦片；6–9 层仅 `code=1` 且带 `name`；
> 10–15 层不过滤、带 `name,width`。

**核心机制**
- **稀疏金字塔** = 预切片 + 动态瓦片：仅对“≥2 个要素”的稠密瓦片预生成入库，稀疏区动态生成。
- **分级规则**：不同层级不同 `filter` / `attrFields` / `merge`。
- **服务端 PNG 渲染**：`ST_AsPng` 按 style（`point_size`/`line_width`/`line_color`…）出栅格。
- **2D / 3D 矢量快显**：3D 走 `Geometry3D` + `ST_AsMVT3D`。

### 2.3 能力对照表

| 维度 | 本项目 | Ganos | 差距 |
|---|:--:|:--:|---|
| 构建 / 删除 / 取瓦片 | ✅ | ✅ | 命名/签名细节（P1） |
| 稀疏金字塔（密度边界） | ✅ `splitSize` | ✅ `splitSize` | 默认值取向不同（P2） |
| 超 `maxZoom` 动态切片 | ✅ `dynamicBeyondMaxZoom` | ✅ `maxLevel` | 对齐 |
| 增量刷新 | ✅ 谓词 + bbox | ✅ bbox 局部 | 本项目更全 |
| 并行构建 | ✅ x-strip / bgworker | ✅ `parallel` | 默认值 + 与预算重试互斥（P2/P3） |
| 逐层构建统计 | ✅ | 〜 未见 | 本项目更强 |
| 原子重建 | ✅ 影子表 swap | 〜 未明确 | 本项目更强 |
| tippecanoe 泛化旋钮 + 字节预算 | ✅ | 〜（靠 `merge`/`filter`） | 取向不同 |
| **分级规则 `buildRules`** | ❌ | ✅ | **P0** |
| **要素合并 `merge` / coalesce** | ❌（路线图#7） | ✅ | **P0** |
| **逐层属性 `attrFields`** | ❌（仅全局 `attributes`） | ✅ | **P0** |
| **PNG 服务端渲染 `ST_AsPng`** | ❌（路线图#10） | ✅ | **P0** |
| **3D 矢量（`ST_AsMVT3D`）** | ❌ | ✅ | **P0（可延后）** |
| `ST_Tile` key 形式 `'z_x_y'` | ❌ | ✅ | P1 |
| `ST_Tile` 参数顺序 | `z,x,y` | `x,y,z` | **P1（不兼容）** |
| schema 限定表名 | ❌（裸名 + search_path） | 〜 | P4 |
| 动态切片缓存 | ❌ | 〜 | P4 |

---

## 3. 差距详解与优化方案

每项给出：**现状 / 目标 / 方案 / 改动点 / 工作量 / 风险**。工作量为相对量级（S<1天、M 几天、L 1-2周、XL 月级）。

### P0-1 `buildRules` 分级规则（filter / attrFields / merge）—— 最大差距

- **现状**：`PyramidConfig.attributes: Vec<String>` 是**全局**属性；`filter` 概念缺失；
  所有 zoom 用同一套旋钮。粗层级想“只显示主干道路 / 只显示大要素 / 合并同色块”做不到。
- **目标**：支持 `buildRules: [{ level:[..], filter:"...", attrFields:[..], merge:{...} }]`，
  逐层级解析出“有效规则”，注入构建与动态切片管线。
- **方案（分三个可独立交付的子项）**：
  1. **逐层 `filter`（先做，收益最大、最低风险）**
     - `config.rs` 增 `BuildRule { level: Vec<u8>, filter: Option<String>, attr_fields: Option<Vec<String>> }`
       与 `build_rules: Vec<BuildRule>`；提供 `rule_for(z)`（命中优先级：精确层 > 默认；可定义“最后命中”或“合并”语义并文档化）。
     - `build.rs::tile_pipeline_from_sql` 增加可选 `WHERE (<filter>)` 注入到源子查询；
       `dynamic.rs` 走**同一**管线，保证物化/动态边界视觉一致。
     - **安全红线**：`filter` 与现有 `where_clause` 同属“调用者 SQL 拼接”，必须沿用
       `ST_UpdatePyramid` 的信任模型——**绝不可** `SECURITY DEFINER` 暴露（见 `pyramid.rs` 注释）。
  2. **逐层 `attrFields`**
     - 把 `attribute_projections(cfg)` 改为 `attribute_projections_for(rule, …)`，按层级选择列；
       缺省回落到全局 `attributes`，保持向后兼容。
     - 注意 worker 路径：`WorkerSpec` 已整体序列化 `PyramidConfig`，扩展字段天然随之分发。
  3. **`merge` / coalesce-densest（路线图#7，最难）**
     - 在 `ST_AsMVTGeom` 之前，对面/线按 `merge` 指定属性分组做 `ST_Union`（或
       `ST_ClusterDBSCAN`/`ST_ClusterIntersecting` 聚类后 union），粗层级显著降要素量与字节。
     - 与 `simplifyPixels`/`dropRate` 协同：merge 解决“小多边形过多”，抽稀解决“点过多”。
     - 建议先支持 polygon 的 `ST_Union(... GROUP BY <attr>)`，线/点后续。
- **改动点**：`config.rs`（新结构 + 校验）、`build.rs`（管线注入 filter/attr + merge 步骤）、
  `dynamic.rs`（动态切片同步）、`pyramid.rs`（信任模型注释延伸）、README。
- **工作量**：filter=M，attrFields=M，merge=L。
- **风险**：filter 注入安全（高，已有同类先例可循）；merge 改变要素语义需等价测试；
  动态切片必须与构建用同一规则，否则边界跳变。

### P0-2 `ST_AsPng` 服务端 PNG 渲染

- **现状**：只出 MVT，渲染完全交给前端（MapLibre）。无 `ST_AsPng`，无 style。
- **目标**：`ST_AsPng(name, z, x, y, style jsonb) -> bytea`，style 支持
  `point_size`/`line_width`/`line_color`/`fill_color` 等，返回 PNG。
- **方案（两条路线，建议 A 起步）**：
  - **A. PostGIS 栅格化（无新重依赖，落地快）**：取该瓦片要素 → `ST_AsRaster` 栅格化 →
    按 style 着色（`ST_MapAlgebra` / 波段赋值）→ `ST_AsPNG`。先支持单一描边/填充色，
    再扩展按属性分类着色。
  - **B. Rust 端渲染（质量高、可控）**：引入 `tiny-skia`/`resvg` 在扩展内把几何画到位图，
    支持抗锯齿、线宽、点符号、标注。依赖更重、构建链更复杂。
- **改动点**：新增 `src/render.rs` + `ST_AsPng` 接口；style 解析复用 `serde`。
- **工作量**：A=M，B=L。
- **风险**：栅格化性能（每瓦片实时渲染需配合缓存，见 P4-1）；style 表达力与 Ganos 对齐程度。

### P0-3 3D 矢量快显（`ST_AsMVT3D` / `Geometry3D`）

- **现状**：纯 2D。投影/简化/量化均假设 2D（Web Mercator 平面）。
- **目标**：支持 3D 要素的快显瓦片（携带高程/体量）。
- **方案（分阶段，整体偏战略，可延后）**：
  1. **Z 感知 MVT（轻量）**：把高程作为属性带入 MVT，前端按属性做 2.5D 拉伸。
  2. **真 3D 瓦片（重）**：对齐 `ST_AsMVT3D` 语义，或产出 3D Tiles/glTF；涉及 3D 索引、
     LOD、坐标空间变换，工作量 XL。
- **改动点**：config 增 3D 开关；新建 3D 管线模块；存储表格式扩展。
- **工作量**：阶段1=M，阶段2=XL。
- **风险**：与现有 2D 管线的复用边界；是否有真实 3D 需求决定 ROI——**建议按需启动**。

### P1-1 `ST_Tile` 参数顺序与 key 重载（兼容性硬伤）

- **现状**：`ST_Tile(name, z, x, y)`（`src/tile.rs:25`，demo 同此 XYZ 习惯）。
- **Ganos**：`ST_Tile(name, x, y, z)` 与 `ST_Tile(name, 'z_x_y')`。`.control` 自称
  “Ganos-compatible API”，但 4 个整型参数**顺序相反**——按位置传参会静默错位（z↔x）。
- **方案**：
  1. **加 key 重载（无歧义、最安全）**：`ST_Tile(name text, key text)`，解析 `'z_x_y'`，
     内部转调，立即获得 Ganos 兼容的一种调用形式。
  2. **整型顺序**：因 `(int,int,int)` 同型，无法靠重载并存两种顺序。三选一并文档化：
     (a) 保持现状 `z,x,y`（符合 XYZ 习惯，**破坏 Ganos 兼容**）；
     (b) 新增 `ST_TileXYZ(name,x,y,z)` 显式兼容；
     (c) 提供兼容开关/独立兼容函数集。**建议 (a)+(1)+(b)**：保留习惯顺序、补 key 形式与显式 XYZ 函数。
  3. 校准 `.control` 的兼容性表述，避免“自称兼容、实际错位”。
- **改动点**：`src/tile.rs`、`bootstrap.sql`（或新 `#[pg_extern]`）、README、`.control`。
- **工作量**：S。
- **风险**：现有调用方（如 demo）已按 `z,x,y`，若改顺序是破坏性变更——故倾向“新增而非改签名”。

### P1-2 `ST_DeletePyramid` 命名

- **现状**：`ST_DropPyramid`。
- **方案**：`bootstrap.sql` 加薄封装 `ST_DeletePyramid(name)` 转调 `ST_DropPyramid`，两名并存。
- **工作量**：S。**风险**：无。

### P1-3 `id` 列类型约束与 schema 限定名

- **现状**：`fid` 走 `::bigint`（聚合 id）与 `::text`（抽稀哈希），比 Ganos 的 int4/int8 限制更宽，
  属正资产；但**表名为裸标识符**（`validate_identifier` 仅允许 `[A-Za-z0-9_]`，≤45 字符），
  不支持 `schema.table`，强依赖 `search_path`。
- **方案**：扩展标识符模型支持可选 schema 限定（`(schema, table)` 双段，分别引用），
  长度上限随派生名（`_gp_tiles_…__shadow`）重新核算。
- **工作量**：M。**风险**：动态 SQL 引用面较广，需统一改造 + 注入测试。

### P2 默认值与行为对齐

| 键 | 本项目默认 | Ganos 取向 | 建议 |
|---|---|---|---|
| `splitSize` | **10000** | “≥2 要素即预生成”（≈2） | 默认偏高→绝大多数瓦片走动态切片、读放大严重（README 自身已警告）。建议**调低默认**（或显著文档化），并配合动态缓存（P4-1）。 |
| `tileExtend` | **8** | 示例 128/256 | 8/4096 缓冲极小，跨瓦片要素易被裁出现接缝。建议**抬高默认**（如 64–256）或随数据自适应。 |
| `parallel` | **1** | 0（不限/自动） | 默认串行最稳，但首次大表慢。可考虑“自动按 CPU/`max_worker_processes` 取值 + 上限保护”。 |

- **改动点**：`config.rs` 默认值 + README；属行为变更，需基准对照后决定。
- **工作量**：S（改值）/ M（自适应）。**风险**：默认值变更影响既有用户产物，需在 release note 标注。

> ⚠️ 说明：本项目动态切片是**每次请求实时扫描源表**（`dynamic.rs`，无缓存），而 Ganos
> 动态瓦片为 C 级实现。因此“高 `splitSize` + 无缓存”在本项目下的读代价比在 Ganos 下更高，
> 这放大了 P2-`splitSize` 与 P4-1 缓存的优先级。

### P3 首次全量构建性能

- **现状**：每个 (zoom, tile) 都在 LATERAL 子查询里重复做源过滤、`ST_Transform`、
  `GeometryType` 判断、简化与 MVT 编码（`build.rs::tile_pipeline_from_sql`）。大表多层级时，
  重复投影/候选过滤是主成本。`docs/superpowers/` 已有 **`preparedBuild`** 设计与实现计划，
  但**尚未落地**。
- **目标**：一次性把源几何预投影到 tileSRS、建临时 GiST 索引，后续各层级直接读预处理表。
- **方案**：
  1. **落地 `preparedBuild`**（已设计）：`parallel=1` 路径先建 `CREATE TEMP TABLE … (fid, geom_tile, geom_type, attrs)` + GiST + ANALYZE，
     新增 `tile_pipeline_from_prepared_sql`。先 opt-in、基准验证再考虑默认开启。
  2. **消除重复 MVT 聚合**：现 `INSERT` 在 SELECT 与 HAVING 各算一次 `ST_AsMVT`，
     改 `WITH grouped AS (… count(*), ST_AsMVT …)` 单次聚合后过滤（计划 Task 3）。
  3. **扩展到并行**：设计 worker 可见的“普通（非 temp）预处理影子表”，处理并发命名、
     成功/失败清理、跨事务可见性、与字节预算重试的交互。
  4. **持久化可选**：允许用户维护持久 3857 生成列/物化中间表，跳过每次构建的投影成本。
- **改动点**：新增 `src/prepared.rs`、`build.rs` 管线分派、`config.rs` 开关、`bench/build_speed.sql`。
- **工作量**：M（单后端）/ L（并行 + 持久化）。
- **风险**：MVT 聚合输入顺序变化导致字节不稳（用语义等价测试兜底）；属性列类型须保真（勿全转 text）。

### P4 健壮性与运维

1. **动态切片无缓存（P4-1，建议优先）**：`ST_Tile` 对未物化/超层瓦片每次实时切。
   建议加可选**瓦片缓存表/LRU**（按 `(name,z,x,y)` 命中，带失效），显著降低热点读代价；
   与 P2-`splitSize` 联动。**工作量 M**。
2. **影子 swap 的 OID 瞬时窗口（`shadow.rs:52` 注释）**：并发读者在 swap 提交瞬间可能遇到
   一次性 “relation does not exist”。如需完全读透明，加视图间接层或读重试。**工作量 M**，优先级低。
3. **`parallel>1` 与 `tileSizeBudgetBytes>0` 互斥**（`build.rs:63`）：当前直接报错。
   可设计 worker 轮次间的可见性协议解除限制，或文档化取舍。**工作量 M**。
4. **增量刷新仅串行**（`incremental.rs::rebuild_extent` 逐 zoom 串行）：大范围更新可并行化。**工作量 M**。
5. **空区域回落仍实时扫描**：`splitSize>0` 时命中 miss（含真正空白区）也会触发一次源表扫描
   （README 已注明）。可用稀疏位图/已知空瓦片标记短路。**工作量 S–M**。

---

## 4. 优先级路线图

> 排序依据：兼容性收益 / 用户可感知度 / 投入产出比。

**第一梯队（高 ROI、低-中风险，建议先做）**
1. **P1-1 `ST_Tile` key 重载 + 显式 XYZ 函数 + 校准 `.control`**（S）——立刻消除“自称兼容实则错位”。
2. **P1-2 `ST_DeletePyramid` 别名**（S）。
3. **P0-1a 逐层 `filter`**（M）——`buildRules` 中收益最大的一块，复用现有 `where_clause` 信任模型。
4. **P3-2 去重 MVT 聚合 + P3-1 落地 `preparedBuild`（单后端）**（M）——构建提速，计划已就绪。

**第二梯队（核心功能补齐）**
5. **P0-1b 逐层 `attrFields`**（M）。
6. **P4-1 动态切片缓存**（M）+ **P2 默认值校准**（S）——成对解决读放大。
7. **P0-2a `ST_AsPng`（PostGIS 栅格化版）**（M）。

**第三梯队（高投入 / 战略）**
8. **P0-1c `merge` / coalesce-densest**（L）。
9. **P3-3 并行预处理表 / 持久化中间层**（L）。
10. **P0-3 3D 矢量快显**（按需，XL）；**P0-2b Rust 端渲染**（L）。

---

## 5. 风险与注意事项（贯穿各项）

- **SQL 注入信任边界**：`buildRules.filter` 与 `where_clause` 一样是调用者 SQL 拼接，
  必须以**调用者权限**执行，严禁 `SECURITY DEFINER` 暴露（见 `pyramid.rs` 现有注释）。
- **构建 / 动态切片一致性**：任何新规则（filter/attr/merge/3D）都必须**同时**接入
  `build.rs` 与 `dynamic.rs`，否则物化区与动态区在 `maxZoom`/`splitSize` 边界出现视觉跳变。
- **向后兼容**：默认值变更（`splitSize`/`tileExtend`/`parallel`）、`ST_Tile` 顺序都属可感知变更，
  倾向“新增而非改签名”，并在 release note 标注。
- **等价测试护栏**：`merge` 与 `preparedBuild` 改变产物，须用“解码后要素数/属性/几何 bbox”做
  语义等价比较（MVT 字节受聚合顺序影响不稳定，参考现有 `parallel_build_matches_serial` 测试思路）。
- **流程合规**：按仓库 `CLAUDE.md`，改动任一函数/方法前先跑 GitNexus `impact`，提交前跑 `detect_changes`。

---

## 6. 一句话总览

本项目在**核心链路与工程健壮性**（原子重建、统计、双增量路径、tippecanoe 旋钮）上已达到甚至
**局部超过** Ganos；差距主要是 **`buildRules` 分级规则、PNG 渲染、3D**三块功能，以及
**`ST_Tile` 签名兼容**这一处“硬伤”。建议按 §4 路线图，先用低成本补齐兼容性与逐层 `filter`，
再推进渲染与 `merge`，3D 按真实需求决定是否启动。

---

## 7. 参考资料

- [How to use the vector pyramid model of the spatiotemporal engine — PolarDB for PostgreSQL](https://www.alibabacloud.com/help/en/polardb/polardb-for-postgresql/vector-pyramid)
- [How to use GanosBase Geometry Pyramid — PolarDB for Oracle](https://www.alibabacloud.com/help/en/polardb/polardb-for-oracle/vector-pyramid)
- [ST_BuildPyramid — AnalyticDB for PostgreSQL](https://www.alibabacloud.com/help/en/analyticdb-for-postgresql/latest/st-buildpyramid)
- [ST_Tile — GanosBase Spatio-temporal Database](https://www.alibabacloud.com/help/en/doc-detail/183336.htm)
- [ST_AsPNG — GanosBase Spatio-temporal Database](https://www.alibabacloud.com/help/en/doc-detail/183249.htm)
- [矢量金字塔 — 阿里云 RDS for PostgreSQL（中文）](https://help.aliyun.com/document_detail/183163.html)
- [如何使用 ST_BuildPyramid 创建矢量金字塔 — PolarDB（中文）](https://help.aliyun.com/zh/polardb/polardb-for-postgresql/st-buildpyramid-2)
- [Ganos 矢量快显功能上手系列二：增强的 MVT 能力](https://help.aliyun.com/zh/polardb/polardb-for-postgresql/ganos-vector-fast-display-function-upper-hand-series-2-enhanced-mvt-capability)
- [Ganos 矢量快显功能 — 阿里云开发者社区](https://developer.aliyun.com/article/1613207)
- [基于 Ganos 百行代码实现亿级矢量空间数据在线可视化](https://blog.csdn.net/weixin_43970890/article/details/114131255)
- 本项目内部设计文档：`docs/superpowers/specs/2026-06-17-build-speed-prepared-build-design.md`、
  `docs/superpowers/plans/2026-06-17-build-speed-prepared-build.md`
</content>
</invoke>
