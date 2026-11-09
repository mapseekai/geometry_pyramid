# 用户数据构建性能分析

测试日期：2026-07-16<br>
测试对象：`geometry_pyramid` `ST_BuildPyramid`

## 0.1.1 候选版的并行调度实测（2026-11-07）

这组实验回答一个问题：在同一版扩展、同一份数据和同一级别上，把 `parallel` 从 1 调到 4，哪些工作值得分给后台进程？下面各行只构建一个 zoom。后面的 0.1.0 历史结果包含多个 zoom，计时范围不同。

| EPSG:3857 输入 | 源要素数 | 顶点数 | zoom | 输出瓦片 | 1 槽中位数 | 4 槽中位数 | 请求 4 槽时的实际执行 |
|---|---:|---:|---:|---:|---:|---:|---|
| 合成稀疏点 | 64 | 64 | 8 | 64 | 3.06 ms | 3.72 ms | 调用连接 |
| 本地点数据 | 250,988 | 250,988 | 10 | 11 | 569.96 ms | 591.55 ms | 调用连接 |
| Natural Earth 道路 | 56,600 | 709,166 | 8 | 8,146 | 1,180.75 ms | 685.82 ms | 后台进程参与 |
| 本地 LUCC 面 | 9,384 | 4,345,620 | 6 | 4 | 10,802.13 ms | 10,907.98 ms | 调用连接 |

环境为 macOS arm64、PostgreSQL 17.10、PostGIS 3.6.4，使用 release 包。真实数据以 `ogr2ogr` 导入，只参与几何和整数 ID，不附带业务属性。每张源表有几何 GiST 索引并已执行 `ANALYZE`。道路取自 [Natural Earth 1:10m 道路数据](https://www.naturalearthdata.com/downloads/10m-cultural-vectors/roads/)。

每种配置预热 1 次，并按 4→1、1→4、4→1 交错计时 3 次。计时涵盖从空注册开始的 `ST_BuildPyramid` 和提交。清理上一轮和解码校验都在计时之外。按瓦片坐标、要素 ID、属性及几何计算解码摘要，四组数据的 1 槽和 4 槽摘要分别相同。

<details><summary>各次计时与解码摘要</summary>

| 数据 | 1 槽，ms | 请求 4 槽，ms | 含坐标的解码 SHA-256（前 16 位） |
|---|---|---|---|
| 合成点 | 3.06 / 3.36 / 2.68 | 3.71 / 4.55 / 3.72 | `37d0620f5b29eb2e` |
| 本地点 | 569.96 / 567.21 / 571.67 | 591.55 / 584.90 / 603.21 | `2c984b28e6aa8890` |
| 道路 | 1,347.39 / 1,177.68 / 1,180.75 | 979.23 / 685.82 / 553.42 | `89b2f07d71b7c55f` |
| LUCC | 10,802.13 / 10,793.19 / 11,042.97 | 10,936.76 / 10,835.20 / 10,907.98 | `8b686a9b076ae795` |

</details>

复测时在隔离数据库的独立自动提交语句中运行，每轮先删除上一轮的测试金字塔。道路一行的设置如下，其他行替换源表和 zoom 即可。

```sql
SET jit = off;
SET work_mem = '4MB';
SET max_parallel_workers_per_gather = 0;
SELECT ST_BuildPyramid('roads', 'geom', 'gp_id',
  '{"name":"calibrate","sourceSRS":3857,"minZoom":8,"maxZoom":8,"parallel":4,"preparedBuild":true,"featureDrivenEnum":true}');
```

调度器会先排除依赖调用事务或会话的任务。对剩余任务，它最多抽取 256 个几何，估算带 buffer 的候选瓦片对和额外顶点数。候选矩形超过 64 格后，预计至少 524,288 个候选对或 8,192 个额外顶点，可以启用 worker。`splitSize=0` 且逐格探测的路径也可以按至少 8,192 格启用 worker。分散点的最终复测中，256 至 32,768 个点的案例均在调用连接执行。65,536 个点时，两种配置为 520 和 539 ms，131,072 个点时为 789 和 748 ms。交界处已有测量波动，其他硬件、数据分布和并发负载需要单独校准。

## 0.1.1 候选版的 splitSize 与基准模式复测（2026-11-07）

在同一台 macOS arm64 机器上，用 PostgreSQL 17.10 / PostGIS 3.6.4 对照本轮修复前后的 release 二进制。两版都包含此前的事务、空间和调度修复。每版先预热一次，再交错测量三次，计时包含构建和提交。切片后比较瓦片坐标及解码出的要素 ID、属性和几何。逐瓦片构建测试使用 1 个槽位，关闭 JIT 和 SQL 查询并行，`work_mem=4MB`。

| 输入与配置 | 物化瓦片 | 修复前中位数 | 修复后中位数 |
|---|---:|---:|---:|
| 64 个分散点，z8，`splitSize=2`，直接读源表 | 0 | 70.387 ms | 2.011 ms |
| 64 个分散点，z8，`splitSize=2`，准备表 | 0 | 69.099 ms | 3.039 ms |
| 64 个分散点，z8，`splitSize=0`，直接读源表 | 64 | 72.419 ms | 72.099 ms |
| 250,988 个集中点，z10，`splitSize=2`，直接读源表 | 11 | 307.406 ms | 284.711 ms |
| 250,988 个集中点，z10，`splitSize=0`，直接读源表 | 11 | 279.027 ms | 261.819 ms |

64 个点的包围矩形覆盖 61,750 格。在直接读源表、`splitSize=2` 的嵌套 INSERT 计划中，共享缓冲命中从 61,814 次降到 264 次。修复后保留的候选区域继续分块，最终没有叶瓦片进入裁剪。64 张按需瓦片的解码摘要一致，前 16 位是 `1b8e1de787394baf`。集中点对照的三次结果有波动，表中数值仅供此数据和配置参考。

在隔离库中可用下面的 SQL 重建稀疏样本。每次测量前清理上一轮金字塔，输出和解码检查放在计时之外。

```sql
CREATE TABLE sparse_probe AS
SELECT i AS id, ST_Centroid(ST_TileEnvelope(8, 3+(i*37)%250, 2+(i*53)%251)) AS geom
FROM generate_series(0,63) AS i;
CREATE INDEX ON sparse_probe USING gist(geom);
ANALYZE sparse_probe;
SELECT ST_BuildPyramid('sparse_probe', 'geom', 'id',
  '{"name":"p2_probe","sourceSRS":3857,"minZoom":8,"maxZoom":8,"splitSize":2,"parallel":1}');
```

用 `bench/mvt_optimization.py out.json --small-parallel` 复测真实点数据时，JSON 只有一个案例，配置为 `preparedBuild=true, parallel=4`。四次构建均在调用连接执行，z0–10 共 24 张瓦片，修复前后中位数为 4.708 和 4.511 秒，解码摘要相同。`--sparse` 在没有 `gpbench` 的库中运行了两组案例，修复后的稀疏面和稀疏点中位数分别为 848.593 和 52.645 ms，两组修复前后的解码摘要各自一致。

## 默认稀疏构建和点网格抽稀（2026-11-07）

在同一隔离库用两版 release 二进制对照。macOS arm64、PostgreSQL 17.10、PostGIS 3.6.4，`work_mem=4MB`，关闭 JIT 和 SQL 查询并行。每种配置预热一次、交错计时三次；中位数含构建和提交，解码校验在计时之外。四组默认构建都用 `splitSize=0`、原表和一个槽位；前后逐瓦片解码摘要一致，包含坐标、ID、属性、几何和重复要素。

| 输入 | zoom | 瓦片数 | 改动前 | 改动后 |
|---|---:|---:|---:|---:|
| 64 个分散点，EPSG:3857 | 8 | 64 | 74.50 ms | 4.97 ms |
| 同一批点，EPSG:4326 | 8 | 64 | 216.93 ms | 9.05 ms |
| 250,988 个本地集中点，EPSG:3857 | 10 | 11 | 264.65 ms | 268.07 ms |
| 56,600 条合成道路，EPSG:3857 | 8 | 8,208 | 524.06 ms | 521.46 ms |

稀疏点沿用上面的 `sparse_probe` 生成方式，省略 `splitSize`，4326 组对同一几何执行 `ST_Transform(geom,4326)` 后建索引。它们覆盖 61,750 格；构建语句的共享缓冲命中从 62,665 降到 2,658 次（3857），从 186,165 降到 5,348 次（4326）。当前自动剪枝从 8,192 格起判断，只考虑少量源要素并要求带缓冲的候选对上界低于网格的八分之一；没有足够证据则继续逐格扫描。一条对角线覆盖大范围的对照，前后均产出 496 张相同的解码瓦片，耗时为 143.41 和 147.68 ms。

点网格在同一新版二进制上单独做了有损 LOD 对照。10 万个点和 32 字符属性进入一张 z0 瓦片，98,000 点集中，其余分散。设置 `"pointGrid":{"cellPixels":64,"keepField":"important"}`，输入每第 997 条标为重要点。以下 RSS 和临时写入是计时之外另开三次连接的中位数；RSS 包含共享页，采样约每 10 ms，可能错过瞬时峰值。

| z0 结果 | 编码要素 | 未压缩 MVT | 构建中位数 | 观测进程 RSS 峰值 | 临时写入 |
|---|---:|---:|---:|---:|---:|
| 默认，不抽稀 | 100,000 | 5,667,005 B | 174.26 ms | 134.33 MiB | 10.73 MiB |
| 点网格 | 777 | 43,392 B | 134.15 ms | 59.73 MiB | 19.61 MiB |

开启网格后，100 个标记点全部保留。聚合器处理的要素变少，但选点排序增加了临时写盘。这里测的是此数据和配置的取舍；保留的点数会随分布、属性和标记的关键点数量变化。关闭网格时，10 万点的前后解码摘要一致。

最终 release 包另跑了多会话回归。带网格的准备表构建在 1 槽与实际使用 worker 的 4 槽下，8,208 张瓦片的解码摘要一致。原有 `--sparse` 两组中位数为 837.21 ms 和 52.14 ms，`--small-parallel` 为 4.55 秒。

## LUCC：分段测量与大面细分实验（2026-11-07）

用新版 release 包构建 9,384 个面、4,345,620 个源顶点，只跑 z6、单槽位、无业务属性，得到 4 张瓦片。用同样的缓冲和简化容差将候选查找与投影、拓扑简化、裁剪拆成三张临时表，最后按瓦片聚合 MVT。分段查询和完整构建的解码摘要一致。每阶段测三次，以下为中位数；临时表读写和单独提交使分段时间不能直接加总成构建耗时。

| 阶段 | 测量中位数 |
|---|---:|
| 查找带缓冲的候选并投影到 EPSG:3857 | 0.629 s |
| `ST_SimplifyPreserveTopology`（容差为该层的 1 个 MVT 单位） | 7.451 s |
| `ST_AsMVTGeom` 裁剪 | 3.329 s |
| `ST_AsMVT` 聚合编码 | 0.049 s |

对 278 个超过 2,048 顶点的源要素执行 `ST_Subdivide(geom,2048)`，片段保留原 ID。细分、建 GiST 索引和 `ANALYZE` 一次性花了 5.82 秒。随后按相同配置交错复测，完整构建中位数从 10.40 秒降到 4.15 秒（不含上述准备时间）。瓦片与 ID 的集合一致，但解码要素实例从 9,640 增至 15,491；增加的 5,851 个实例在各自瓦片中与其他片段共用 ID。将共享边上每侧同 ID 的线段合并后对照，358 组边、侧与 ID 中有 70 组覆盖变化。因此不在默认构建中启用细分。

## 同层简化复用和纯点选点（2026-11-08）

在同一台 macOS arm64、PostgreSQL 17.10 / PostGIS 3.6.4 上对照两版 release 包。`work_mem=4MB`，关闭 JIT 和 SQL 查询并行，每版预热一次、交错计时三次。各组的逐瓦片解码摘要一致。临时写入和 RSS 来自计时之外的独立连接，RSS 包含共享页。

| 案例 | 完整构建中位数 | 临时写入 |
|---|---:|---:|
| LUCC z6，`preparedBuild` + `featureDrivenEnum` | 10.61 → 8.69 s | 22.42 → 22.20 MiB |
| 56,600 条合成道路 z8，同配置 | 588.16 → 591.09 ms | 12.27 → 12.27 MiB |
| 10 万单点，32 字符属性，z0 `pointGrid` | 129.45 → 105.14 ms | 19.61 → 11.50 MiB |
| 同一批点，1,024 字符属性 | 343.91 → 251.95 ms | 316.90 → 213.90 MiB |
| 1,024 字符属性，`preparedBuild` | 538.89 → 465.82 ms | 316.90 → 213.90 MiB |
| 32 字符属性，另加线、多点和面 | 131.28 → 136.59 ms | 19.61 → 19.61 MiB |

强制让道路使用按要素路径的原型从 595 ms 变为 678 ms。最终实现沿用稀疏选路，在启用了 `featureDrivenEnum` 的单进程构建中，只对 2–4 格、无物化阈值或预算重试的层补做重叠检查。重复简化的候选顶点达到 262,144 且不少于唯一候选顶点的八分之一时，先按要素简化再裁剪。最终 LUCC 选中该路径，输出 4 张瓦片和 9,640 个要素实例。道路仍输出 8,208 张瓦片。

普通表的 geometry 列声明为单点类型时，物化构建的 `pointGrid` 直接对裁剪后的点选代表。长属性组的观测进程 RSS 峰值中位数从 152.94 降到 147.05 MiB。该组仍输出相同的 777 个要素，保留 100 个标记点，物化和按需生成的解码结果一致。

## 结论摘要

- 点数据、建筑面、派生线在 z0-10 的构建耗时稳定，主要落在 **4.5-19.2 秒**。
- `lucc.geojson` 虽然只有 **9,384** 个面，但几何复杂度高，z0-6 已需 **74-77 秒**，z0-8 单次 baseline 需 **131.8 秒**；它是本轮最重的数据。
- 4326、3857、4490 三套源坐标系的差异整体不大；3857 通常略快，但差异多在噪声或个位数百分比内。
- `preparedBuild=true` 在这些数据上没有形成显著稳定收益，多数在 **0.96x-1.06x** 区间。
- 点数据的 `featureMajorPoints=true` 在 z0-10 没有收益，原因是当前数据范围较集中、只生成 24 个瓦片，高 zoom 空格网探测压力不大。
- 用户给定的 `big.fgb` 经 GDAL 识别为 `3D Multi Polygon`，不是线图层；本报告用 `ST_Boundary(big.fgb)` 派生 `line_boundary` 线数据做线性能测试。
- 针对 `lucc.geojson` 的后续优化测试显示，`buildRules.sample` 是最有效的旋钮：`sample10` 在 z0-6 从 **75.1s 降到 4.5s**，z0-8 从 **131.8s 降到 7.0s**。
- 本轮补测显示，`parallel=4` 对当前“瓦片数少、单瓦片重”的数据没有收益；mixed LOD 从 **4.4s** 退化到 **22.1s**。

## 环境

| 项 | 值 |
|---|---|
| Git commit | `a5ae465` |
| PostgreSQL | 17.10 Homebrew, arm64 |
| PostGIS | 3.6.4 |
| PROJ | 9.8.1 |
| GDAL | 3.12.2 |
| geometry_pyramid | 0.1.0 release build |
| 测试数据库 | pgrx pg17, port `28817` |

## 数据导入

源文件位于 `/Users/zhang/Desktop/测试数据/`：

| 文件 | GDAL 识别几何 | 要素数 | 范围 |
|---|---:|---:|---|
| `points.geojson` | Point | 250,988 | 119.566642,29.815521 - 120.680202,30.523720 |
| `building.geojson` | 3D Multi Polygon | 251,014 | 119.566425,29.815384 - 120.680788,30.523823 |
| `big.fgb` | 3D Multi Polygon | 251,014 | 119.566425,29.815384 - 120.680788,30.523823 |
| `lucc.geojson` | Multi Polygon | 9,384 | 108.799125,24.639626 - 114.255996,30.129031 |

每个源文件分别导入为 EPSG:4326、EPSG:3857、EPSG:4490 三套表，均放在 `gpbench` schema 下。导入时使用 `ogr2ogr -t_srs` 重投影，并保留 PostGIS GiST 空间索引。

派生表：

- `gpbench.big_line_<srid>`：由 `gpbench.big_poly_<srid>` 执行 `ST_Boundary` 得到。
- `gpbench.mixed_<srid>`：合并 `points`、`big_line`、`building`，共 753,016 条。

复现实验脚本：

- `bench/user_data_perf.sql`：导入后建派生表，并跑 points / line_boundary / building z0-10 主矩阵。
- `bench/user_data_perf_followup.sql`：补跑 lucc z0-6 和 mixed z0-8。

## 汇总结果

### 点数据 z0-10

| SRID | 策略 | 耗时(s) | speedup vs baseline | 瓦片数 | MVT 总量 |
|---:|---|---:|---:|---:|---:|
| 4326 | baseline | 5.119 | 1.000x | 24 | 74,093,179 |
| 4326 | prepared | 5.019 | 1.020x | 24 | 74,093,909 |
| 4326 | feature_major | 5.593 | 0.915x | 24 | 73,869,707 |
| 3857 | baseline | 4.481 | 1.000x | 24 | 74,093,909 |
| 3857 | prepared | 5.097 | 0.879x | 24 | 74,093,909 |
| 3857 | feature_major | 5.515 | 0.813x | 24 | 73,869,707 |
| 4490 | baseline | 5.717 | 1.000x | 24 | 74,093,179 |
| 4490 | prepared | 5.128 | 1.115x | 24 | 74,093,909 |
| 4490 | feature_major | 5.632 | 1.015x | 24 | 73,869,707 |

点数据只覆盖 24 个瓦片，tile-major 枚举成本很低，`featureMajorPoints` 的额外准备成本抵消了收益。3857 baseline 最快，说明源数据已在瓦片坐标系时可减少一部分投影开销。

### 派生线数据 z0-10

| SRID | 策略 | 耗时(s) | speedup vs baseline | 瓦片数 | MVT 总量 |
|---:|---|---:|---:|---:|---:|
| 4326 | baseline | 15.707 | 1.000x | 21 | 17,304,782 |
| 4326 | prepared | 14.951 | 1.051x | 21 | 17,304,782 |
| 3857 | baseline | 14.951 | 1.000x | 21 | 17,304,782 |
| 3857 | prepared | 14.628 | 1.022x | 21 | 17,304,782 |
| 4490 | baseline | 15.582 | 1.000x | 21 | 17,304,782 |
| 4490 | prepared | 14.753 | 1.056x | 21 | 17,304,782 |

派生线的 prepared 路径有小幅收益，但幅度不大。输出完全一致，说明差异主要来自构建路径开销而不是结果变化。

### 建筑面 z0-10

| SRID | 策略 | 耗时(s) | speedup vs baseline | 瓦片数 | MVT 总量 |
|---:|---|---:|---:|---:|---:|
| 4326 | baseline | 17.884 | 1.000x | 19 | 7,164,330 |
| 4326 | prepared | 17.262 | 1.036x | 19 | 7,164,330 |
| 3857 | baseline | 18.354 | 1.000x | 19 | 7,164,330 |
| 3857 | prepared | 19.219 | 0.955x | 19 | 7,164,330 |
| 4490 | baseline | 18.403 | 1.000x | 19 | 7,164,330 |
| 4490 | prepared | 18.320 | 1.004x | 19 | 7,164,330 |

建筑面数据范围集中，z0-10 只输出 19 个瓦片。prepared 路径没有稳定优势；3857 prepared 反而慢约 4.5%。

### LUCC 面数据

| SRID | 策略 | maxZoom | 耗时(s) | speedup vs baseline | 瓦片数 | MVT 总量 |
|---:|---|---:|---:|---:|---:|---:|
| 4326 | baseline | 6 | 76.589 | 1.000x | 12 | 6,159,193 |
| 4326 | prepared | 6 | 75.619 | 1.013x | 12 | 6,159,193 |
| 3857 | baseline | 6 | 75.125 | 1.000x | 12 | 6,159,193 |
| 3857 | prepared | 6 | 74.046 | 1.015x | 12 | 6,159,193 |
| 4490 | baseline | 6 | 76.961 | 1.000x | 12 | 6,159,193 |
| 4490 | prepared | 6 | 76.180 | 1.010x | 12 | 6,159,193 |
| 4326 | baseline | 8 | 131.751 | n/a | 44 | 17,153,178 |

`lucc.geojson` 的瓶颈不是要素数量，而是复杂面裁剪与 MVT 编码。z0-6 只有 12 个瓦片也需要约 75 秒；z0-8 baseline 增至 44 个瓦片，耗时 131.8 秒。z0-10 曾运行到 z9 后仍未完成，被手动取消，说明高 zoom 成本会继续快速增加。

### LUCC 优化对比

LUCC 字段基本为空，无法做业务语义过滤；本轮改测 `buildRules.sample`、构建期简化和瓦片大小预算。

| 策略 | maxZoom | 耗时(s) | speedup vs baseline | MVT 总量 | 体积下降 |
|---|---:|---:|---:|---:|---:|
| baseline 3857 | 6 | 75.125 | 1.000x | 6,159 kB | 1.000x |
| sample25 | 6 | 19.143 | 3.924x | 1,455 kB | 4.135x |
| sample10 | 6 | 4.492 | 16.723x | 547 kB | 10.995x |
| sample5 | 6 | 3.116 | 24.109x | 333 kB | 18.047x |
| simplifyPixels=2 + minFeatureSizePixels=1 | 6 | 142.525 | 0.527x | 3,998 kB | 1.540x |
| baseline 4326 | 8 | 131.751 | 1.000x | 16 MB | 1.000x |
| sample10 | 8 | 7.011 | 18.792x | 1,506 kB | 11.125x |

结论：

- `sample10` 是当前最平衡的 LUCC 粗层优化：z0-6 提速约 16.7x，z0-8 提速约 18.8x，同时把 MVT 总量压到约 1/11。
- `sample5` 更快、更小，但视觉/业务损失可能更明显，适合极粗 zoom 或预览层。
- `sample25` 仍有 3.9x 提速，但对复杂面来说减负不够彻底。
- 构建期 `ST_SimplifyPreserveTopology` 对这批复杂面不适合作为首选优化；它把 z0-6 从 75.1s 拉长到 142.5s，虽然 MVT 变小，但 CPU 成本更高。
- `tileSizeBudgetBytes=512000` 在 LUCC 上触发多轮重试，前几轮已经出现预算超限并继续 tighten；这类预算适合做输出保护，不适合作为首要提速手段。

### LUCC 进一步优化：Subdivide 与分层采样

后续测试补充了 `ST_Subdivide(geom, 2048)` 预处理、分层采样，以及 mixed/points 的 tile budget。

| 数据 | 策略 | maxZoom | 耗时(s) | 对比基线 | MVT 总量 | 最大瓦片 |
|---|---|---:|---:|---:|---:|---:|
| LUCC 3857 baseline | 原始面 | 6 | 75.125 | 1.000x | 6,159 kB | 1,343 kB |
| LUCC subdivide | `ST_Subdivide(geom,2048)` | 6 | 40.315 | 1.864x | 6,409 kB | 1,368 kB |
| LUCC subdivide + sample10 | subdivide 后采样 10% | 6 | 4.613 | 16.284x | 694 kB | 149 kB |
| LUCC sample10 | 原始面采样 10% | 6 | 4.492 | 16.723x | 560 kB | 149 kB |
| LUCC tiered 5/10/25 | z0-4 5%，z5-6 10%，z7-8 25% | 8 | 22.068 | 5.970x | 3,152 kB | 403 kB |
| LUCC sample10 | 全 z0-8 采样 10% | 8 | 7.011 | 18.792x | 1,542 kB | 184 kB |

结论：

- `ST_Subdivide` 单独使用有收益，把 LUCC z0-6 从 75.1s 降到 40.3s，但不改变输出体积，仍不如采样。
- `ST_Subdivide + sample10` 与原始 `sample10` 耗时接近，但输出更大；这说明本轮最核心收益来自减少进入 MVT 的 feature 数，而不是切碎大面。
- 分层采样保留更多高 zoom 细节，z0-8 耗时 22.1s，比 baseline 快 6.0x，但比全局 `sample10` 慢；这是质量/性能折中方案。
- 如果目标是最快构建和最小瓦片，用全局 `sample10`；如果目标是 z7-z8 多保留细节，用分层 5/10/25。

### Mixed 数据 z0-8

| SRID | 策略 | 耗时(s) | speedup vs baseline | 瓦片数 | MVT 总量 |
|---:|---|---:|---:|---:|---:|
| 4326 | baseline | 29.741 | 1.000x | 9 | 73,492,144 |
| 4326 | prepared | 28.697 | 1.036x | 9 | 73,491,143 |
| 3857 | baseline | 27.073 | 1.000x | 9 | 73,491,143 |
| 3857 | prepared | 28.328 | 0.956x | 9 | 73,491,143 |
| 4490 | baseline | 30.347 | 1.000x | 9 | 73,492,144 |
| 4490 | prepared | 29.088 | 1.043x | 9 | 73,491,143 |

Mixed 表有 753,016 条要素，但范围集中，z0-8 只输出 9 个瓦片。总耗时约 27-30 秒，主要由少数大瓦片编码决定，最大单瓦片约 12.97 MB。

### 大瓦片 Budget 对比

| 数据 | 策略 | maxZoom | 耗时(s) | MVT 总量 | 最大瓦片 | 结论 |
|---|---|---:|---:|---:|---:|---|
| points 3857 baseline | 无 budget | 10 | 4.481 | 74,094 kB | 6,727 kB | 快，但瓦片过大 |
| points budget 1MB | `tileSizeBudgetBytes=1048576` | 10 | 12.294 | 16,536 kB | 1,318 kB | 体积降 4.5x，构建慢 2.7x |
| mixed 3857 baseline | 无 budget | 8 | 27.073 | 73,491 kB | 12,969 kB | 快，但单瓦片很大 |
| mixed budget 4MB | `tileSizeBudgetBytes=4194304` | 8 | 121.322 | 44,660 kB | 5,987 kB | 体积降 1.6x，构建慢 4.5x |
| mixed budget 1MB | `tileSizeBudgetBytes=1048576` | 8 | 163.967 | 11,656 kB | 1,329 kB | 体积降 6.3x，构建慢 6.1x |

Budget 的定位更像“输出保护”而不是“构建加速”。对 points 比较可接受；对 mixed，会显著增加构建耗时。

### Mixed LOD 与拆层对比

针对 mixed 数据继续测试了低 zoom 属性裁剪、分层采样，以及 points/line/building 分开建三个金字塔后叠加。

| 策略 | maxZoom | 耗时(s) | MVT 总量 | 最大瓦片 | 说明 |
|---|---:|---:|---:|---:|---|
| mixed baseline | 8 | 27.073 | 70 MB | 12 MB | 单表、全属性 |
| attr_kind | 8 | 25.469 | 48 MB | 9.5 MB | 粗层只输出 `kind` |
| tiered_sample_5_10_25 | 8 | 4.576 | 8,898 kB | 3,360 kB | z0-4 5%，z5-6 10%，z7-8 25% |
| tiered_sample_attr_kind | 8 | 4.436 | 6,141 kB | 2,373 kB | 分层采样 + 只保留 `kind` |
| split_points_line_building | 8 | 25.492 | 64 MB | 6,570 kB | 三个金字塔分别构建后前端叠加 |

结论：

- mixed 的核心优化仍是分层采样：从 27.1s 降到 4.6s，MVT 从 70 MB 降到 8.9 MB。
- 再叠加 `attrFields:["kind"]` 后体积降到 6.1 MB，最大瓦片降到 2.37 MB，耗时略降到 4.44s，是当前 mixed 最优配置。
- 仅裁剪属性有一定收益，但不大：70 MB → 48 MB，耗时 27.1s → 25.5s。
- 拆成 points/line/building 三个金字塔，总耗时与 mixed baseline 接近，但最大瓦片从 12 MB 降到 6.57 MB；如果前端能接受多 source 叠加，这是一个降低单瓦片峰值的结构性方案。

### Points 与 Mixed 点专属采样

为了验证大瓦片是否主要来自点，继续测试了 points 分层采样，以及 mixed 中只对 `POINT` 几何采样。

| 数据 | 策略 | maxZoom | 耗时(s) | MVT 总量 | 最大瓦片 | 说明 |
|---|---|---:|---:|---:|---:|---|
| points baseline | 无采样 | 10 | 4.481 | 71 MB | 6,570 kB | 25 万点全量 |
| points tiered 1/5/25 | z0-6 1%，z7-8 5%，z9-10 25% | 10 | 0.830 | 4,345 kB | 857 kB | 最小体积 |
| points tiered 5/10/25 | z0-6 5%，z7-8 10%，z9-10 25% | 10 | 0.858 | 6,773 kB | 857 kB | 保留更多粗层点 |
| mixed baseline | 无采样 | 8 | 27.073 | 70 MB | 12 MB | 全量点/线/面 |
| mixed point-only tiered + attr_kind | 只对 POINT 采样 | 8 | 23.345 | 11 MB | 6,022 kB | 只解决点膨胀 |
| mixed all-geometry tiered + attr_kind | 对所有几何采样 | 8 | 4.436 | 6,141 kB | 2,373 kB | 当前最佳 mixed |

结论：

- points 的低 zoom 必须采样；`tiered 1/5/25` 把点瓦片从 71 MB 降到 4.3 MB，构建也从 4.48s 降到 0.83s。
- mixed 的大瓦片不只来自点。只采样 POINT 后，体积从 70 MB 降到 11 MB，但耗时仍有 23.3s，最大瓦片仍 6 MB。
- mixed 要同时提速和减小瓦片，需要对点、线、面都做 LOD；当前最佳仍是 all-geometry `tiered_sample_attr_kind`。

### 最大瓦片归因

本轮继续回查每个测试结果中最大的 MVT 瓦片，并用对应 `z/x/y` 的 `ST_TileEnvelope` 统计源要素覆盖情况。注意：对带采样的结果，表里的 `source_feature_count` 是该瓦片范围内的源表相交数，不等于最终进入 MVT 的采样后要素数。

| 结果 | 最大瓦片 | z/x/y | 源相交要素 | 源顶点数 | 最大源要素顶点 | 主要构成 |
|---|---:|---|---:|---:|---:|---|
| mixed baseline z8 | 12,968,951 B | 8/213/105 | 753,016 | 3,320,686 | 145 | building 251,014；line 251,014；point 250,988 |
| points baseline z10 | 6,727,182 B | 0/0/0 | 250,988 | 250,988 | 1 | 全部点落入世界瓦片 |
| mixed tiered + attr_kind z8 | 2,430,239 B | 8/213/105 | 753,016 | 3,320,686 | 145 | 同一热点瓦片，但采样和属性裁剪压小输出 |
| LUCC baseline z6 | 1,343,449 B | 6/51/26 | 4,618 | 2,258,806 | 125,374 | 少量复杂 MultiPolygon |
| points tiered 1/5/25 z10 | 877,460 B | 9/426/210 | 131,139 | 131,139 | 1 | 采样后峰值转移到高 zoom 局部热点 |
| LUCC sample10 z8 | 184,082 B | 7/103/53 | 3,367 | 1,873,655 | 125,374 | 复杂面仍在，但采样显著压小输出 |

结论：

- mixed 最大瓦片是数据范围集中导致的：z8 的热点瓦片同时覆盖全部点、线、面，单瓦片自然膨胀到 12 MB 级。
- points baseline 最大瓦片出现在 z0 世界瓦片，因为 25 万点全部写入同一个粗层瓦片；低 zoom 采样是直接有效的解决办法。
- LUCC 的大瓦片不是要素数问题，而是复杂面的顶点数问题；最大源要素超过 12.5 万顶点，采样比构建期拓扑简化更实用。

### Parallel=4 对照

| 数据 | 策略 | serial 耗时(s) | parallel=4 耗时(s) | 变化 | MVT 总量 | 最大瓦片 |
|---|---|---:|---:|---:|---:|---:|
| LUCC z8 | sample10 | 7.011 | 7.134 | 0.983x | 1,506 kB | 180 kB |
| mixed z8 | baseline | 27.073 | 27.373 | 0.989x | 70 MB | 12 MB |
| mixed z8 | tiered_sample_attr_kind | 4.436 | 22.147 | 0.200x | 6,141 kB | 2,373 kB |

结论：

- 当前测试数据的输出瓦片数很少：LUCC sample10 z8 为 42 个瓦片，mixed z8 只有 9 个瓦片；这种形态不适合默认开启 `parallel=4`。
- 并行路径需要让 worker 访问提交后的中间表，调度和准备成本在这批数据上无法摊薄。
- mixed LOD 已经把单次任务压到 4.4s，继续并行反而让固定开销变成主成本。
- `parallel` 更适合瓦片数量大、空间范围分散、单层任务足够多的数据；对当前数据建议保持 `parallel=1`。

## 坐标系影响

本轮 4326、3857、4490 的结果差异较小：

- 点数据 baseline：3857 为 4.481s，4326 为 5.119s，4490 为 5.717s。
- 建筑面 baseline：三者为 17.884-18.403s。
- 派生线 baseline：三者为 14.951-15.707s。
- lucc z6 baseline：三者为 75.125-76.961s。

结论：当输出 `tileSRS=3857` 时，源表预先存 3857 有时能减少投影成本，但如果瓶颈是复杂几何裁剪/编码，坐标系差异会被淹没。

## 性能瓶颈判断

1. **大瓦片编码是主要瓶颈。** 点和 mixed 的瓦片数量很少，但 MVT 总量很大；mixed 单瓦片最大约 12.97 MB。
2. **复杂面比要素数更关键。** LUCC 只有 9,384 个面，却比 25 万建筑面慢 4 倍以上。
3. **preparedBuild 对集中范围数据收益有限。** 当前数据集输出瓦片数少，预处理表和索引成本不一定能摊薄。
4. **featureMajorPoints 更适合高 zoom 稀疏点数据。** 当前点数据集中，z0-10 只有 24 个输出瓦片，无法体现该路径优势。
5. **对复杂面，采样比构建期拓扑简化更有效。** LUCC 的 p99 点数为 5,924，最大点数为 137,137；拓扑简化本身也很贵。
6. **Subdivide 是中等收益方案。** 它能降低复杂面裁剪成本，但不能解决低 zoom 要素过多的问题；与采样结合后收益主要仍来自采样。
7. **Tile budget 会用构建时间换输出大小。** 适合有瓦片大小硬约束的发布场景，不适合追求最快构建。
8. **Mixed 的最佳路径也是 LOD。** 分层采样 + 属性裁剪比 budget 更有效，能同时提速和减小瓦片。
9. **拆层构建降低峰值瓦片大小，但不显著提速。** 它更适合作为前端渲染/缓存组织优化，而不是构建加速。
10. **点图层粗 zoom 采样收益极高。** 单独 points 用分层采样后，构建速度和瓦片大小都显著改善。
11. **Mixed 不能只优化点。** 点专属采样能降体积，但线/面仍让构建保持较慢；mixed 的 LOD 应覆盖所有几何类型。
12. **当前数据不适合默认并行构建。** 输出瓦片数太少，`parallel=4` 没有加速，mixed LOD 还明显变慢。

## 建议

- 对 `lucc` 这类复杂面数据，优先配置粗 zoom LOD。当前数据字段为空，建议先用 `buildRules.sample`：粗层可从 `sample10` 起步，极粗层可试 `sample5`。
- 对大瓦片结果，设置 `tileSizeBudgetBytes` 做输出保护，避免 10 MB 级瓦片直接产出；但不要期待它一定提速，因为预算重试会增加构建成本。
- 真正评估 `featureMajorPoints` 时，把点数据测到更高 zoom，或使用空间范围更稀疏的数据；当前 z0-10 不足以触发优势。
- 如果生产数据最终以 3857 存储，仍建议保留 3857 源表 benchmark；但本轮显示收益不是决定性因素。
- 不建议对 LUCC 默认打开较大的 `simplifyPixels` 作为提速方案；本轮 `simplifyPixels=2` + `minFeatureSizePixels=1` 变慢到 142.5s。
- 当前这批数据不建议默认设置 `parallel=4`；先保留 `parallel=1`，只有当数据范围明显扩大、输出瓦片数达到数百或更多时再重新测试并行。

推荐的 LUCC 起始配置：

```json
{
  "sourceSRS": 3857,
  "tileSRS": 3857,
  "maxZoom": 8,
  "buildRules": [
    {
      "minZoom": 0,
      "maxZoom": 8,
      "sample": {
        "rate": 10,
        "key": ["ogc_fid"]
      }
    }
  ]
}
```

如果希望 z7-z8 多保留细节，可以使用分层配置：

```json
{
  "sourceSRS": 3857,
  "tileSRS": 3857,
  "maxZoom": 8,
  "buildRules": [
    { "minZoom": 0, "maxZoom": 4, "sample": { "rate": 5, "key": ["ogc_fid"] } },
    { "minZoom": 5, "maxZoom": 6, "sample": { "rate": 10, "key": ["ogc_fid"] } },
    { "minZoom": 7, "maxZoom": 8, "sample": { "rate": 25, "key": ["ogc_fid"] } }
  ]
}
```

对 points 可以考虑 1MB budget 做发布保护；对 mixed 更建议先做 LOD/属性裁剪，再考虑 budget，否则构建成本过高。

推荐的 points 起始配置：

```json
{
  "sourceSRS": 3857,
  "tileSRS": 3857,
  "maxZoom": 10,
  "attributes": ["id"],
  "buildRules": [
    { "minZoom": 0, "maxZoom": 6, "sample": { "rate": 1, "key": ["ogc_fid"], "geometryTypes": ["POINT"] } },
    { "minZoom": 7, "maxZoom": 8, "sample": { "rate": 5, "key": ["ogc_fid"], "geometryTypes": ["POINT"] } },
    { "minZoom": 9, "maxZoom": 10, "sample": { "rate": 25, "key": ["ogc_fid"], "geometryTypes": ["POINT"] } }
  ]
}
```

推荐的 mixed 起始配置：

```json
{
  "sourceSRS": 3857,
  "tileSRS": 3857,
  "maxZoom": 8,
  "attributes": ["kind", "source_id"],
  "buildRules": [
    {
      "minZoom": 0,
      "maxZoom": 4,
      "sample": { "rate": 5, "key": ["gid"] },
      "attrFields": ["kind"]
    },
    {
      "minZoom": 5,
      "maxZoom": 6,
      "sample": { "rate": 10, "key": ["gid"] },
      "attrFields": ["kind"]
    },
    {
      "minZoom": 7,
      "maxZoom": 8,
      "sample": { "rate": 25, "key": ["gid"] },
      "attrFields": ["kind"]
    }
  ]
}
```

## 复现命令

启动并安装扩展：

```bash
cargo pgrx start pg17
cargo pgrx install --release --pg-config /opt/homebrew/Cellar/postgresql@17/17.10/bin/pg_config
```

启用扩展：

```bash
psql -h /Users/zhang/.pgrx -p 28817 -d postgres \
  -c 'CREATE EXTENSION IF NOT EXISTS postgis; CREATE EXTENSION IF NOT EXISTS geometry_pyramid;'
```

导入数据示例：

```bash
ogr2ogr -overwrite -f PostgreSQL \
  'PG:host=/Users/zhang/.pgrx port=28817 dbname=postgres' \
  '/Users/zhang/Desktop/测试数据/points.geojson' \
  -nln gpbench.points_4326 \
  -t_srs EPSG:4326 \
  -nlt POINT \
  -dim XY \
  -lco GEOMETRY_NAME=geom \
  -lco FID=ogc_fid \
  -lco SPATIAL_INDEX=GIST
```

运行 benchmark：

```bash
psql -h /Users/zhang/.pgrx -p 28817 -d postgres -f bench/user_data_perf.sql
psql -h /Users/zhang/.pgrx -p 28817 -d postgres -f bench/user_data_perf_followup.sql
psql -h /Users/zhang/.pgrx -p 28817 -d postgres -f bench/user_data_lucc_sampling.sql
psql -h /Users/zhang/.pgrx -p 28817 -d postgres -f bench/user_data_next_optimization.sql
psql -h /Users/zhang/.pgrx -p 28817 -d postgres -f bench/user_data_budget_followup.sql
psql -h /Users/zhang/.pgrx -p 28817 -d postgres -f bench/user_data_mixed_optimization.sql
psql -h /Users/zhang/.pgrx -p 28817 -d postgres -f bench/user_data_split_z8.sql
psql -h /Users/zhang/.pgrx -p 28817 -d postgres -f bench/user_data_point_sampling.sql
psql -h /Users/zhang/.pgrx -p 28817 -d postgres -f bench/user_data_tile_profile_parallel.sql
```
