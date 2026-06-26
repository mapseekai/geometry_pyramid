# 首次全量构建提速：对标 Ganos「亿级 10 分钟」深度分析与方案

> 日期：2026-06-26
> 目标：把首次全量 `ST_BuildPyramid` 提到 Ganos 量级——**1 亿要素 ≈ 10 分钟**。
> 性质：根因分析 + 分阶段实现方案（含可落地 SQL 形态、度量口径、风险）。
> 关联：本文**取代/扩展** `docs/superpowers/specs/2026-06-17-build-speed-prepared-build-design.md`
> 中的保守方案——后者只做「一次投影」，**不足以达标**，原因见 §3.2。

---

## 1. Ganos 的事实依据（官方口径）

阿里官方与开发者文章对「亿级矢量在线可视化」的核心描述（见 §8）：

- **构建速度**：1 亿地块数据，金字塔生成**约 10 分钟**；
- **存储**：金字塔索引仅占原表**约 5%**；
- **方法**：用空间索引**按空间密度划分数据**，**基于密度**建立稀疏矢量金字塔（SVP），
  **相比传统切片减少 90% 的数据计算量**；
- **并行**：**完全并行**处理模式；
- **稀疏**：只保存 **≥2 个 Geometry** 的瓦片，稀疏区在查询时**动态生成**；
- **显示**：Z-order 排序 + 可见性剔除，秒级响应。

**关键句**：「相比**传统切片**减少 90% 计算量」。本项目当前的构建方式，正是 Ganos
所说的「传统切片」（逐瓦片枚举网格）。这 90% 就是我们要拿回来的空间。

---

## 2. 现状构建路径回顾（代码事实）

全量构建入口 `build.rs::build → build_into`：对 `z ∈ [minZoom, maxZoom]`，每层调用
`build_zoom_level`，执行 `tile_pipeline_from_sql`（`src/build.rs:388` 起）拼出的 SQL：

```sql
INSERT INTO "_gp_tiles_<name>" (z,x,y,format,srid,mvt)
SELECT $1, t.x, t.y, 'pbf', $8, ST_AsMVT(features.*, $2, $3, 'geom','fid')
FROM generate_series($4::int, $5::int) AS xs(x)          -- xmin..xmax  ← 整个 bbox 矩形
CROSS JOIN generate_series($6::int, $7::int) AS ys(y)    -- ymin..ymax  ← 的【每一个】网格 cell
CROSS JOIN LATERAL (SELECT xs.x, ys.y) t
CROSS JOIN LATERAL (
   SELECT ST_AsMVTGeom(prep.gtile, ST_TileEnvelope($1,t.x,t.y), $3,$9,true) AS geom, prep.fid
   FROM (
      SELECT CASE WHEN simplify THEN ST_SimplifyPreserveTopology(ST_Transform(src.geom,$8),$11)
                  ELSE ST_Transform(src.geom,$8) END AS gtile,          -- ← 每瓦片重复投影
             src.fid
      FROM "<source>" src
      WHERE src.geom IS NOT NULL
        AND src.geom && ST_Transform(ST_TileEnvelope($1,t.x,t.y,margin=>$9/$3), $10)  -- ← 每 cell 一次 GiST 探测
   ) prep
   WHERE <min-size 过滤> AND <点抽稀>
) features
WHERE features.geom IS NOT NULL
GROUP BY t.x, t.y
HAVING count(*) >= {split_size} AND ST_AsMVT(...) IS NOT NULL;             -- ← splitSize 后置
```

这是**瓦片主序（tile-major）**：外层枚举瓦片网格，内层对每个瓦片回源表做 `&&` 候选过滤。

---

## 3. 为什么慢——逐条根因

### 3.1 根因清单（按影响排序）

1. **【最大】枚举整张瓦片网格，含海量空瓦片**
   `generate_series(xmin..xmax) × generate_series(ymin..ymax)` 是数据 bbox 在该 zoom 的
   **完整矩形**。高 zoom 下该矩形里绝大多数 cell 是空的，但每个 cell 都要：生成一行 +
   算 `ST_TileEnvelope` + 投影包络 + 一次 GiST `&&` 探测。**这正是 Ganos 说的「传统切片」**，
   也是「减少 90% 计算」的主要来源——Ganos 按密度只碰有数据的瓦片，我们碰全网格。
   - 成本量级：z 层矩形 cell 数 R(z) 随层级约 ×4 增长；高 zoom 的 R(z) 可达 10⁵–10⁸。

2. **每瓦片、每 zoom 重复投影**
   `ST_Transform(src.geom, tileSRS)` 对「每个候选要素 × 它覆盖的每个瓦片 × 每个 zoom」重复执行。
   1 亿要素 × ~15 层，仅低/中 zoom 的全量重投影就是**十亿级** `ST_Transform` 调用。

3. **每瓦片重复简化**
   `ST_SimplifyPreserveTopology` 同理：同一要素在某 zoom 的简化结果与落在哪个瓦片无关，却被反复算。

4. **每层独立从全分辨率源构建，无跨 zoom 复用**
   低 zoom 仍然处理全量精细几何，没有「自底向上聚合」或「投影几何复用」。

5. **`splitSize` 是 `HAVING` 后置过滤（`build.rs:469`）——只省存储、不省计算**
   所有瓦片都先完成扫描 / 投影 / `ST_AsMVTGeom` / `ST_AsMVT` 聚合，**之后**才按 `count(*) >= splitSize`
   丢弃。Ganos 是「**先**按密度决定建哪些瓦片」，我们是「**后**扔」。默认 `splitSize=10000`
   下，我们为大量最终不入库的瓦片白白付了全部计算。

6. **同一 MVT 聚合算两次 + 逐行 `GeometryType()`**
   `ST_AsMVT(...)` 同时出现在 `SELECT` 与 `HAVING`；`GeometryType()` 在过滤里逐行调用。
   （已在既有计划 Task 3 识别。）

### 3.2 成本模型与「为什么只做一次投影不够」

总成本 ≈ `Σ_z [ R(z)·探测 + 候选(z)·(投影+简化+MVTGeom) + 瓦片(z)·MVT编码 ]`

- **低/中 zoom**：候选(z) ≈ 全量 N，**投影/简化主导** → 由 **一次投影（§4 阶段 2）** 消除。
- **高 zoom**：R(z) 巨大且多为空，**空瓦片探测主导** → 必须由 **feature-major（§4 阶段 3）** 消除。
- **全程**：稀疏瓦片的 **编码浪费** → 由 **稀疏前置（§4 阶段 4）** 消除。

> 既有 `preparedBuild` 设计只解决了「低 zoom 重复投影」（第 2 条根因），**保留了瓦片主序**，
> 因此第 1 条（空瓦片枚举）与第 5 条（splitSize 后置）仍在。对稀疏 / 高 zoom 数据，这两条往往
> 才是大头。**所以要达标，必须从「瓦片主序」改成「特征主序」**——这是本文的核心结论。

### 3.3 一个常被忽略的关键点：feature-major 全量构建**根本不需要空间索引**

瓦片主序之所以要 GiST `&&`，是因为它「拿着瓦片找要素」。改成「拿着要素算瓦片」后，瓦片 x/y
是从要素坐标**纯算术**得到的（Web Mercator → tile index），不需要任何空间索引。于是：

- 省掉对 1 亿行**重建 GiST 索引**的分钟级开销（这本身就是传统切片预处理的一大成本）；
- 省掉每瓦片的索引探测；
- 全量构建退化为「**顺序扫一遍 + 算术分箱 + 编码**」，这正是高性能切片器（tippecanoe 等）的做法。

（源表 GiST 仍用于 `ST_UpdatePyramidByExtent` / 动态切片，不受影响。）

---

## 4. 方案：把「构建规则与方式」对齐 Ganos

五个杠杆，对应 Ganos 的五个特征：

| Ganos 特征 | 本项目动作 | 消除的根因 |
|---|---|---|
| 按密度划分数据（非网格） | **特征主序分箱** | #1 空瓦片、#3 重复简化 |
| 一次扫描一次投影 | **一次投影 + 全 zoom 复用** | #2 重复投影 |
| 只建 ≥2 要素的瓦片 | **稀疏前置（splitSize 提前）** | #5 编码浪费 |
| 完全并行 | **按空间块/ Z-order 并行** | 墙钟时间 |
| Z-order / 自底向上 | **（可选）向上聚合** | #4 跨 zoom 复用 |

### 阶段 0：度量先行（前置，不可跳过）

- 落地 `bench/build_speed.sql`（既有计划已给出脚本）：固定混合点/线/面数据集，输出总耗时
  与 per-zoom `tile_count / total_bytes / max_tile_bytes / build_ms`。
- 增加几何类型分布、bbox、各 zoom 矩形 cell 数 R(z)（估算空瓦片占比）。
- 用 `EXPLAIN (ANALYZE, BUFFERS)` 抓一层的真实热点，**确认**投影/探测/编码各自占比。
- **没有 baseline 不谈提速**；后续每阶段都要在同一数据集上 gate。

### 阶段 1：零风险 SQL 去重（先做，作安全基线）

- 单次 MVT 聚合：`WITH grouped AS (SELECT x,y,count(*) c, ST_AsMVT(...) mvt …) … WHERE c>=splitSize AND mvt IS NOT NULL`（既有 Task 3）。
- 把 `GeometryType(...)` 预先物化成一列，避免过滤里逐行重算。
- 行为等价测试护栏（解码后要素数/属性/几何 bbox 比较，规避 MVT 字节顺序不稳）。

### 阶段 2：一次投影（preparedBuild，已设计，但只是中间步）

- `CREATE TEMP TABLE prep AS SELECT fid::bigint, ST_Transform(geom,tileSRS) AS g,
  GeometryType(geom) AS gt, <attrs> FROM source WHERE geom IS NOT NULL;`
- **本阶段先不建 GiST**（见 §3.3：feature-major 不需要它）。如仍走瓦片主序过渡，才需要 GiST。
- 各 zoom 从 `prep.g` 读，省掉重复投影与重复 `GeometryType`。
- ⚠️ 单独到此**不达标**，必须进入阶段 3。

### 阶段 3【关键】：特征主序分箱，消灭空瓦片枚举

把「枚举全网格、回源探测」改为「扫一遍要素、各自算覆盖瓦片」。理想形态是**一次扫描、所有 zoom**：

```sql
INSERT INTO "_gp_tiles_<name>" (z,x,y,format,srid,mvt)
WITH prep AS MATERIALIZED (                      -- 扫描+投影【仅一次】
   SELECT fid::bigint AS fid, ST_Transform(geom, $tileSRS) AS g,
          GeometryType(geom) AS gt /*, attrs */
   FROM "<source>" WHERE geom IS NOT NULL
),
assigned AS (                                    -- 每要素 → 它自己覆盖的瓦片（小范围）
   SELECT z, tx, ty, p.fid, p.g, p.gt /*, attrs */
   FROM prep p
   CROSS JOIN generate_series($minZoom, $maxZoom) AS z      -- 投影几何在各 zoom 间复用
   CROSS JOIN LATERAL (
      SELECT gx AS tx, gy AS ty
      FROM generate_series(tile_x(ST_XMin(p.g), z), tile_x(ST_XMax(p.g), z)) AS gx,
           generate_series(tile_y(ST_YMax(p.g), z), tile_y(ST_YMin(p.g), z)) AS gy
   ) t                                          -- ← 每要素 O(自身覆盖瓦片数)，不再是全网格
),
dense AS (                                       -- 稀疏前置：先数密度（仅分箱，不编码）
   SELECT tx, ty, z FROM assigned
   GROUP BY z, tx, ty HAVING count(*) >= $splitSize
),
enc AS (                                         -- 仅对稠密瓦片做昂贵的 clip/quantize
   SELECT a.z, a.tx, a.ty,
          ST_AsMVTGeom(simplify(a.g, a.z), ST_TileEnvelope(a.z, a.tx, a.ty),
                       $tileSize, $tileExtend, true) AS geom, a.fid /*, attrs */
   FROM assigned a JOIN dense d USING (z, tx, ty)
)
SELECT z, tx, ty, 'pbf', $tileSRS, ST_AsMVT(enc.*, $layer, $tileSize, 'geom','fid')
FROM enc WHERE geom IS NOT NULL
GROUP BY z, tx, ty;
```

要点：

- `tile_x/tile_y` 是从 Web Mercator 米坐标到瓦片索引的**纯算术**（`tile_rect.rs::from_bbox_m`
  已有同款公式，可下沉为 SQL 表达式或 immutable 函数），**无需 `&&`、无需 GiST**。
- `generate_series(z)` 让同一份投影几何在所有 zoom 复用 → 投影只发生一次（根因 #2 根除）。
- 大要素跨多瓦片仍生成多行（与瓦片主序的非空工作量一致），`ST_AsMVTGeom` 仍负责裁剪，
  凹形几何「bbox 覆盖但实际不相交」的瓦片由 `WHERE geom IS NOT NULL` 滤除——正确性不变。
- **点要素快通道**：点必落单瓦片，`tx/ty` 直接算，`gt='POINT'` 时可跳过 `generate_series` LATERAL，
  进一步省。混合数据按 `gt` 分流。

### 阶段 4：稀疏前置——让 `splitSize` 真正省计算

如上 `dense` CTE：**先**用「仅分箱」的廉价 `count` 选出 ≥`splitSize` 的瓦片，**再**只对它们做
`ST_AsMVTGeom`/`ST_AsMVT`（昂贵步骤）。于是 `splitSize` 同时省「存储 + 编码计算」，对齐 Ganos
「只建 ≥2 要素的瓦片」。稀疏瓦片仍由现有 `ST_Tile` 动态切片兜底（行为不变）。

> 副作用提示：当前默认 `splitSize=10000` 会让绝大多数瓦片走动态切片，配合阶段 4 后构建极快，
> 但读放大大。建议与「动态切片缓存」一起调参（见 gap-analysis P2/P4-1）。

### 阶段 5：完全并行（对齐 Ganos `parallel`）

现状并行是 `per-zoom × x-strip`，且每个 worker **各自回扫源表**（`build.rs::run_zoom_strips`）。
改造方向：

- **按空间块 / Z-order 分区**：将 `prep` 按瓦片前缀（低 zoom 的 tx/ty 或 Z-order 桶）切成 K 块，
  每个 worker 处理一块的**全部 zoom**，写各自的 (z,x,y) 子集——天然不重叠、不重复扫源。
- **worker 可见性**：`prep` 改为普通表（提交后 worker 可见），命名加 pid/序号，成功/失败清理，
  与字节预算重试的交互（既有设计「待扩展」清单已列出这些点）。
- **并行度默认值**：Ganos 默认不限；可按 `CPU / max_worker_processes` 自适应取值 + 上限保护。
- 也可先利用 **PostgreSQL 并行查询**（`max_parallel_workers_per_gather`）让单条 feature-major
  查询内部并行，作为低改动量的过渡。

### 阶段 6（可选）：自底向上聚合 / Z-order

- maxZoom 建好后，低 zoom 用上层「代表要素 / 已简化几何」再分箱，避免低 zoom 重复处理全量
  （根因 #4）。对纯点数据收益有限，对稠密面/线收益大。
- Z-order 排序提升缓存局部性与并行负载均衡。

---

## 5. 预期与验收口径

| 阶段 | 主要消除 | 预期 |
|---|---|---|
| 1 去重 | #5部分/#6 | 小幅，零风险基线 |
| 2 一次投影 | #2 | 低/中 zoom 明显 |
| **3 特征主序** | **#1/#3** | **高 zoom 数量级（稀疏数据尤甚）** |
| 4 稀疏前置 | #5 | 整体再降（按 splitSize 命中率） |
| 5 完全并行 | 墙钟 | ≈ 线性于 worker 数 |

- 阶段 3+4+5 合起来对应 Ganos 的「减少 90% 计算 + 完全并行」。
- **验收**：在固定数据集（建议 1千万 与 1亿 两档，点/线/面混合）上对比 baseline 的总 `build_ms`
  与 per-zoom 明细；目标对齐「亿级 ≈ 10 分钟」**量级**（绝对值受硬件 / PostGIS 版本 / work_mem 影响，
  以相对加速 + 量级达标为准）。
- 正确性：同 (z,x,y) 上 `ST_Tile` 非空性一致；解码后要素数/属性/几何 bbox 语义等价。

---

## 6. 风险与边界

- **大临时表 / 内存**：1 亿几何的投影副本可能数十 GB；全 zoom 单查询的 `GROUP BY` 需要足够
  `work_mem` 或落盘。缓解：按空间块分批 / 分 worker；评估「持久 3857 生成列」免投影副本。
- **MVT 字节顺序不稳**：聚合输入顺序变化导致字节不同 → 用语义等价测试（参考既有
  `parallel_build_matches_serial` 思路）。
- **管线一致性**：`dynamic.rs` 动态切片与 `incremental.rs` 增量必须与新构建管线产出**视觉一致**
  （同样的简化/过滤/分箱），否则物化区 / 动态区在 `splitSize`、`maxZoom` 边界跳变。
- **坐标系前提**：`tile_x/tile_y` 与 `pixel_size_m`（`config.rs:147`）的换算只对米制 Web Mercator
  成立；非 3857 的 `tileSRS` 需单独处理或拒绝。
- **数值边界**：`tileSize`/`tileExtend` 已有上限校验（`config.rs::validate`）；feature-major 下注意
  tile index 的 i32 范围（`maxZoom ≤ 24` 已限制）。
- **流程合规**：按 `CLAUDE.md`，改 `build_into` / `build_zoom_level` / `tile_pipeline_from_sql`
  等符号前先跑 GitNexus `impact`，提交前 `detect_changes`。

---

## 7. 落地顺序（建议）

```
阶段0 度量  →  阶段1 去重  →  阶段2 一次投影  →  阶段3 特征主序【关键】
          →  阶段4 稀疏前置  →  阶段5 完全并行  →（阶段6 可选）
```

每个阶段都设 benchmark gate，达标即可停；阶段 3 是从「传统切片」跨到「稀疏密度构建」的分水岭，
是拿回那「90%」的关键一步。

---

## 8. 参考资料

- [使用 Ganos 构建亿级矢量空间数据在线可视化方案 — PolarDB（中文）](https://help.aliyun.com/zh/polardb/polardb-for-oracle/implementation-of-online-visualization-of-billion-level-vector-spatial-data-based)
- [基于 Ganos 百行代码实现亿级矢量空间数据在线可视化（CSDN）](https://blog.csdn.net/weixin_43970890/article/details/114131255)
- [基于 Ganos 百行代码实现亿级矢量空间数据在线可视化（阿里云开发者社区）](https://developer.aliyun.com/article/782087)
- [Ganos 矢量快显功能上手系列一：2D 和 3D 矢量快显](https://help.aliyun.com/zh/polardb/polardb-for-postgresql/ganosbase-vector-fast-display-function-hands-on-series-1)
- [如何使用 Ganos 的矢量金字塔模型 — PolarDB](https://help.aliyun.com/zh/polardb/polardb-for-oracle/vector-pyramid)
- 本项目内部：`docs/superpowers/specs/2026-06-17-build-speed-prepared-build-design.md`、
  `docs/superpowers/plans/2026-06-17-build-speed-prepared-build.md`、`docs/ganos-gap-analysis.md`
</content>
