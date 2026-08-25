# 首次全量构建提速（feature-major）实现计划

> **For agentic workers:** 按 PR 顺序逐个交付，每个 PR 内按 Task / Step 勾选推进。
> 每个 PR 都是 opt-in、可独立回滚，并以 benchmark + 等价测试为闸门。
> 背景与根因见 `docs/build-speed-deep-dive.md`；整体差距见 `docs/ganos-gap-analysis.md`。

> 当前状态（2026-07-02）：这是历史实现计划。后续 main 没有按本文的
> `buildStrategy` / `src/build_fm.rs` 形态落地，而是用 `preparedBuild`、
> `featureMajorPoints`、`featureDrivenEnum` 三个 opt-in 开关逐步实现。下方
> “实现核对”是当前可信状态；PR 分解保留为历史设计记录。

**Goal:** 把首次全量 `ST_BuildPyramid` 从「瓦片主序（传统切片）」改造为 Ganos 式
「特征主序（按密度）」构建，对齐「亿级 ≈ 10 分钟、完全并行」量级，同时保持产物语义不变。

**Architecture:** 旧路径（`build.rs` 瓦片主序）完全保留；新增 `buildStrategy` 开关与
`src/build_fm.rs` 特征主序路径（一次投影 + 算术分箱 + 稀疏前置）。`ST_Tile` 动态切片
（`dynamic.rs`）与增量（`incremental.rs`）不动，但新路径必须复用同一套 `ZoomKnobs`
与过滤语义，保证物化区/动态区视觉一致。并行在 PR4 扩展到 worker 可见的预处理表。

**Tech Stack:** Rust, pgrx 0.18, PostgreSQL/PostGIS, SPI 动态 SQL, `cargo check` /
`cargo test`（单测）、`cargo pgrx test`（pg_test 等价测试）、`bench/build_speed.sql`（速度闸门）。

## 实现核对（2026-07-02）

| 计划项 | 当前状态 | 当前实现位置 / 说明 |
|---|---|---|
| PR1 基准 harness | ✅ 已实现 | `bench/build_speed.sql` 已存在。 |
| PR1 单次 MVT 聚合 | ✅ 已实现 | `build_zoom_level` / prepared path 用 `WITH grouped`，避免重复 `ST_AsMVT`。 |
| PR2 `buildStrategy` / `src/build_fm.rs` | ❌ 未按此形态实现 | 该设计被替换为 `preparedBuild`、`featureMajorPoints`、`featureDrivenEnum`。 |
| PR2 一次投影 prepared table | ✅ 已实现 | `src/prepared.rs` 创建 temp/committed prepared table，保存 `geom_tile`、`geom_type`、属性和 rule 列。 |
| PR2 纯 POINT feature-major | ✅ 已实现 | `featureMajorPoints=true` 时按点坐标算 tile id；纯 POINT、`tileSRS=3857`、`parallel=1`、`maxZoom >= featureMajorMinZoom` 才启用。 |
| PR2 all-geometry feature-major 泛化 | ✅ 已实现 | 以 `featureDrivenEnum=true` 形式落地，按 feature envelope 派生候选 tile。 |
| PR3 splitSize 稀疏前置 | ⚠️ 部分实现 | `splitSize` 语义已实现；但本文计划的“先 dense CTE 再编码、避免编码稀疏 tile”没有完整按原方案落地。 |
| PR4 worker 可见预处理表 | ✅ 已实现 | `prepare_source_committed` 创建 worker 可见 prepared table。 |
| PR4 空间块并行 | ✅ 已实现但形态不同 | prepared / feature-driven 路径使用 Morton blocks 并 interleave 到 workers；纯 POINT `featureMajorPoints` 仍保持单 backend。 |
| PR5 翻默认值 / `buildStrategy` 默认 | ❌ 未实现 | 默认仍是保守路径：`parallel=1`、`preparedBuild=false`、`featureMajorPoints=false`、`featureDrivenEnum=false`。 |

## 执行环境说明（本会话实测）

- ✅ `cargo`/`rustc` 1.94、✅ PostgreSQL 16 dev headers + `psql`（cluster 默认 down，可拉起）。
- ❌ `cargo-pgrx` 未预装、`~/.pgrx` 未初始化 → `cargo pgrx test` 与基准前需先 `cargo install
  cargo-pgrx` + `cargo pgrx init --pg16`（分钟级，且本机硬件≠目标硬件）。
- ❌ GitNexus 运行器/MCP 不可用 → `CLAUDE.md` 的 `impact`/`detect_changes` 以**手动调用图审查**
  替代：动手前读全 callers/callees + grep，在 PR 描述列出 blast radius；提交前用 `git diff`
  + 人工核对受影响符号。**此替代须在每个改动 PR 中显式标注。**

## 全局不变量（每个 PR 必须守住）

1. **opt-in**：`buildStrategy` 默认 `legacy`；新路径仅在显式选择时启用。
2. **语义等价**：新旧路径在同一数据上，逐 `(z,x,y)` 的「非空性 + 解码后要素数/属性/几何 bbox」
   一致。**不比字节**（ST_AsMVT 聚合顺序不稳，参考既有 `parallel_build_matches_serial`）。
3. **`splitSize` 语义不变**：仍以「最终进入 MVT 的要素组计数 ≥ splitSize」为准（PR3 仅把判定
   前移到编码前，不改阈值含义）。
4. **`ZoomKnobs` 单一真源**：简化/最小尺寸/点抽稀的 per-zoom 计算继续走 `config.rs` +
   `build.rs::ZoomKnobs`，新路径只消费，不另起一套。
5. **边缘缓冲等价**：feature-major 分箱时按 `tileExtend` 对应的米级 margin 扩展要素覆盖范围，
   复刻旧路径 `margin => tileExtend/tileSize` 的无缝边效果（见 PR2 Task 2 的 margin 处理）。
6. **坐标系前提**：算术分箱只对米制 Web Mercator（3857）成立；`tileSRS` 非 3857 时新路径
   回退 legacy 并 NOTICE（或拒绝），不得静默出错。
7. **原子性**：仍写 shadow 表后原子 swap（`shadow.rs`），失败回滚，不留 shadow。

---

## PR1 — 阶段 0+1：基准 harness + 零风险 SQL 去重

- Create: `bench/build_speed.sql`
- Modify: `src/build.rs::build_zoom_level`、`README.md`
- Test: `cargo test build::tests`、（可选）`cargo pgrx test`、手动 `psql` 基准

- [ ] **Task 1.1 基准脚本**：落地既有计划的 `bench/build_speed.sql`（混合点/线/面、可配
  `max_zoom`、打印 per-zoom `tile_count/total_bytes/max_tile_bytes/build_ms`）。在其基础上增加：
  - 几何类型分布与总 bbox；
  - 各 zoom 的「矩形 cell 数 R(z) 估算」（量化空瓦片占比，作为 feature-major 收益预测）；
  - 一个 `scale` 变量（默认 1e6，可调到 1e7/1e8）用于不同规模。
- [ ] **Task 1.2 单次 MVT 聚合**：把 `build_zoom_level` 的 INSERT 改为
  `WITH grouped AS (SELECT x,y,count(*) c, ST_AsMVT(...) mvt … GROUP BY x,y)
   INSERT … SELECT … FROM grouped WHERE c >= split_size AND mvt IS NOT NULL`，
  消除 SELECT/HAVING 双算 `ST_AsMVT`。参数绑定与输出列不变。
- [ ] **Task 1.3 预存 GeometryType**（可选小优化）：在 prep 子查询里物化 `GeometryType(...)`
  为一列，过滤处引用列而非逐行重算。
- [ ] **Task 1.4 测试 + 闸门**：现有 `build::tests` 与 pg_test 全绿；`bench` 出 baseline 数字。
- [ ] **Task 1.5 提交**
  ```
  perf: single-pass MVT aggregation + build-speed benchmark harness
  ...
  Confidence: high | Scope-risk: narrow | Tested: cargo test build::tests (+bench or Not-tested)
  ```
- **回滚**：纯局部 SQL 改写，revert 即可。

---

## PR2 — 阶段 2+3：feature-major + 一次投影（核心杠杆，opt-in）

- Modify: `src/config.rs`（`buildStrategy`）、`src/build.rs::build_into`（分派）、`src/lib.rs`（注册模块）、`sql/bootstrap.sql`（瓦片算术函数）
- Create: `src/build_fm.rs`
- Test: `cargo test`、`cargo pgrx test`（新增等价测试）、`bench` A/B

- [ ] **Task 2.0 手动 impact**：读 `build()`/`build_into`/`build_zoom_level_with_budget` 的全部
  调用方（`ST_BuildPyramid`、`ST_UpdatePyramid` 全量分支、worker 路径），在 PR 描述列 blast radius。
- [ ] **Task 2.1 配置开关**：`config.rs` 增 `BuildStrategy { Legacy, FeatureMajor }`
  （`#[serde(rename_all=...)]`，键 `buildStrategy`，默认 `legacy`），纳入 `validate()`：
  `FeatureMajor` 且 `tileSRS != 3857` → 校验期报错或标记回退。单测覆盖解析与默认值。
- [ ] **Task 2.2 瓦片算术函数**：`bootstrap.sql` 增 IMMUTABLE：
  ```sql
  CREATE FUNCTION _gp_tile_x(m float8, z int) RETURNS int IMMUTABLE ...  -- floor((m+H)/(2H/2^z))
  CREATE FUNCTION _gp_tile_y(m float8, z int) RETURNS int IMMUTABLE ...  -- floor((H-m)/(2H/2^z)) (y 反转)
  ```
  对齐 `tile_rect.rs::from_bbox_m`，结果 clamp 到 `[0, 2^z-1]`。加 SQL 级单测对拍。
- [ ] **Task 2.3 `src/build_fm.rs`**：
  - `prepare_projected(src)`：`CREATE TEMP TABLE … AS SELECT fid::bigint fid,
    ST_Transform(geom,$tileSRS) g, GeometryType(geom) gt, <attrs> FROM source WHERE geom IS NOT NULL`。
    **不建 GiST**（feature-major 不需要）。属性列保真（勿转 text）。
  - `build_feature_major(src, storage_table)`：核心查询（见 `build-speed-deep-dive.md` §4 阶段3），
    `WITH prep MATERIALIZED → assigned(每要素按 margin 扩展后的 envelope 算瓦片范围,
    generate_series(minZoom,maxZoom) 跨 zoom 复用) → enc(ST_AsMVTGeom, 复用 ZoomKnobs) →
    GROUP BY z,x,y HAVING count(*)>=splitSize`。
  - **margin 处理（不变量 5）**：算瓦片范围前，对要素 envelope 按 `tileExtend*pixel_size_m(z)`
    扩展，确保边缘要素进入相邻瓦片缓冲带。
  - **点快通道**：`gt IN ('POINT','MULTIPOINT')` 时单瓦片，跳过 range 的 LATERAL。
- [ ] **Task 2.4 分派**：`build_into` 按 `cfg.build_strategy` 选 `run_zoom_strips`（legacy）或
  `build_fm`（单后端）；`FeatureMajor + parallel>1` 本 PR 先回退单后端 + NOTICE（并行留 PR4）。
- [ ] **Task 2.5 等价测试**：`feature_major_matches_legacy`（混合点/线/面，同数据建 legacy 与
  feature-major 两个金字塔，逐 (z,x,y) 比较非空性 + 解码要素数/属性/bbox）。覆盖 `splitSize=0`
  与默认值、含 `attributes`、含简化/抽稀的配置。
- [ ] **Task 2.6 闸门**：`bench` 上 legacy vs feature-major 的 per-zoom build_ms 对比；高 zoom
  应出现数量级下降（稀疏数据尤甚）。记录数字。
- [ ] **Task 2.7 提交**（含手动 impact 说明、Rejected: GiST-on-prep | feature-major 不需要索引）。
- **回滚**：默认 legacy，翻开关或 revert PR 即恢复。

---

## PR3 — 阶段 4：稀疏前置（splitSize 在编码前剪枝）

- Modify: `src/build_fm.rs`
- Test: `cargo pgrx test`（等价 + 稀疏行为）、`bench`

- [ ] **Task 3.1**：在 feature-major 查询中插入 `dense` CTE：先用「仅分箱」的廉价
  `GROUP BY z,x,y HAVING count(*)>=splitSize` 选出稠密瓦片，`enc` 仅 `JOIN dense` 后再做
  `ST_AsMVTGeom`/`ST_AsMVT`。使昂贵编码只发生在最终入库的瓦片上。
- [ ] **Task 3.2 语义守恒**：验证「先数后编码」与「编码后 HAVING」对同一 splitSize 产物一致
  （计数口径都为最终要素组），扩展 `feature_major_matches_legacy` 的断言。
- [ ] **Task 3.3 闸门**：在高 `splitSize`（如默认 10000）数据集上，PR3 相对 PR2 再降编码耗时。
- [ ] **Task 3.4 提交**。
- **回滚**：纯查询结构调整，revert 即可。

---

## PR4 — 阶段 5：按空间块并行（worker 可见预处理表）

- Modify: `src/build_fm.rs`、`src/worker.rs`（新 `WorkerMode`）、`src/build.rs`（分派）
- Test: `cargo pgrx test`（并行==串行==legacy 语义）、`bench`（多 worker 近线性）

- [ ] **Task 4.0 手动 impact**：worker 控制面（`run_spec`/`spawn`/`wait_for`）+ shadow 提交路径。
- [ ] **Task 4.1 预处理表 worker 可见**：`prepare_projected` 改为提交后可见的普通表
  （pid/序号命名、成败清理、与 shadow 同走 autonomous DDL），解决 MVCC 可见性。
- [ ] **Task 4.2 空间块分区**：按 Z-order / 低 zoom 瓦片前缀把 prep 切成 K 块；新增
  `WorkerMode::BuildFeatureMajorBlock { block, … }`，每 worker 跑「一块的全部 zoom」，写各自
  (z,x,y) 子集到 shadow，天然不重叠、不重复扫源。复用 `write_spec/spawn/wait_for`。
- [ ] **Task 4.3 约束**：沿用「`parallel>1` 与 `tileSizeBudgetBytes>0` 互斥」；并行度默认值
  评估按 `max_worker_processes` 自适应（对齐 Ganos 不限），带上限保护。
- [ ] **Task 4.4 测试**：parallel feature-major == serial feature-major == legacy（语义）；
  worker 不可见预处理表的退化路径有 NOTICE。
- [ ] **Task 4.5 闸门**：固定数据集下 worker 数 1→N 的近线性加速曲线。
- [ ] **Task 4.6 提交**。
- **回滚**：默认 `parallel=1`；feature-major 并行是 parallel>1 才触发，回退安全。

---

## PR5 —（可选）阶段 6 + 默认值评估

- [ ] **Task 5.1**（可选）自底向上聚合 / Z-order：maxZoom 建好后低 zoom 复用上层简化几何，
  减少低 zoom 重复处理全量；对稠密面/线收益大，纯点收益小——由 PR2/PR4 的 per-zoom 数字决定是否做。
- [ ] **Task 5.2 默认值评估**：跨 1e6/1e7/1e8 三档、点/线/面混合，等价测试 + benchmark 稳定后，
  评估是否把 `buildStrategy` 默认翻为 `feature-major`，并校准 `splitSize`/`tileExtend` 默认
  （配合 gap-analysis P2 与动态切片缓存 P4-1）。翻默认须在 release note 标注为可感知变更。
- [ ] **Task 5.3** 更新 README（`buildStrategy` 文档 + 基准说明）、roadmap 勾选。

---

## 落地顺序与闸门

```
PR1(基准+去重) ─▶ PR2(feature-major+一次投影) ─▶ PR3(稀疏前置) ─▶ PR4(并行) ─▶ PR5(可选/翻默认)
   gate:baseline      gate:高zoom数量级↓          gate:编码↓        gate:近线性       gate:跨规模达标
```

每个 PR 不达闸门不进下一个；PR2 是从「传统切片」跨到「稀疏密度构建」的分水岭。

## 验证与合规小结

- **本会话可跑**：`cargo check`、`cargo test`（config/tile 数学/旋钮单测）。
- **需先装 cargo-pgrx + 起 PG**：`cargo pgrx test`（等价测试）、`bench/build_speed.sql`（速度）。
  绝对「亿级≈10 分钟」数字应在**目标硬件**上测；本会话仅出相对加速。
- **每个改动 PR**：开头手动 impact（列 blast radius）、结尾 `git diff` 自查受影响符号，
  替代不可用的 GitNexus `impact`/`detect_changes`，并在 PR 描述显式声明此替代。
</content>
