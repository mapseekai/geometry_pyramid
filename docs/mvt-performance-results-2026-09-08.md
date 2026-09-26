# MVT 构建优化实测（2026-09-08）

相同输入和配置下，10 个真实数据/子集案例及 2 个合成案例的逐瓦片解码结果均保持一致。完整调用中位数：真实全量点降低 6.6%–15.7%，建筑边界线降低 19.4%–21.8%，建筑面降低 13.8%–30.8%；建筑 prepared 的优化前波动较大，30.8% 不宜直接外推。250 个 LUCC 面子集降低约 44%，不代表全量 LUCC。稀疏合成面 feature-driven 案例约快 7.71 倍；稀疏点 feature-major 耗时基本不变。

## 方法与版本

- 基线源码：`9b1a60558e1c92b7d7a1b90944dbbc7521596565`。16 个源文件/构建配置与该提交逐字节核对，使用与优化版相同的 `Cargo.lock`，均以 `cargo build --release --locked --offline` 构建。
- 基线库 SHA256：`73981eeaf6ecf00a8c34574fad0a42323bc7f70f8c862fbb70047dfac27e3412`。
- 优化库 SHA256：`f814b363a065874cdce744322455eff23c2e3186a14e98f1ae13c5177d42d179`。
- 本机 aarch64 macOS，PostgreSQL 17.10 / PostGIS 3.6.4 / GEOS 3.14.1 / PROJ 9.8.1 / Wagyu 0.5.0。`work_mem=4MB`、`jit=off`、`max_parallel_workers_per_gather=0`，扩展构建 `parallel=1`。
- 每案例先热身 1 次，再测 3 次，取后三次中位数。使用持久数据库连接，通过隔离 C wrapper 分别加载两份不可变库，避免扩展重装污染版本。测试期间停止本任务的编译、测试及其他数据库工作。
- 时间包围完整 `ST_BuildPyramid` 调用，包含准备表、索引、bbox、编码、发布及提交；不包含输入统计、删除上次输出及解码校验。各层 `build_ms` 不等同于该完整时间。
- 原始 `gpbench` 数据只读；LUCC 子集按 `ogc_fid` 排序取前 250 行。构建输出和合成数据使用独立 schema，测后清理。

## 完整调用结果

单位为秒；范围为三次正式测量的最小值–最大值。source 表示 `preparedBuild=false`，prepared 表示 `true`；主矩阵均关闭 feature-major 和 feature-driven。

| 输入 | 路径 | 优化前中位数 | 优化后中位数 | 时间降低 | 优化前范围 | 优化后范围 |
|---|---|---:|---:|---:|---|---|
| points_3857 | source | 4.582 | 3.862 | 15.7% | 4.537–4.668 | 3.802–3.867 |
| points_3857 | prepared | 5.161 | 4.551 | 11.8% | 5.146–5.562 | 4.541–4.607 |
| big_line_3857 | source | 16.936 | 13.248 | 21.8% | 16.874–17.151 | 13.208–13.891 |
| big_line_3857 | prepared | 17.056 | 13.756 | 19.4% | 16.947–17.836 | 13.366–15.505 |
| building_3857 | source | 19.499 | 16.815 | 13.8% | 19.424–19.747 | 16.709–16.876 |
| building_3857 | prepared | 23.910 | 16.555 | 30.8% | 21.055–29.253 | 16.481–16.579 |
| lucc_sample | source | 0.689 | 0.384 | 44.2% | 0.685–0.698 | 0.378–0.393 |
| lucc_sample | prepared | 0.698 | 0.388 | 44.4% | 0.695–0.708 | 0.386–0.389 |
| points_4326 | source | 5.086 | 4.288 | 15.7% | 5.005–5.102 | 4.216–4.514 |
| points_4326 | prepared | 5.176 | 4.837 | 6.6% | 5.162–5.729 | 4.756–4.844 |

## 输入和输出规模

默认 extent 为 4096、`tileExtend=8`、`simplifyPixels=1`。除 LUCC 为 z0–6，主矩阵均为 z0–10。点保留 `id` 属性，其余案例无附加属性。要素实例数包含跨瓦片和跨 zoom 的重复实例。

| 输入/路径 | 源行数 | 源顶点数 | 输出 tile | 输出要素实例 | 总 MVT 字节 | 最大 tile 字节 |
|---|---:|---:|---:|---:|---:|---:|
| points_3857（两路径） | 250988 | 250988 | 24 | 2769411 | 74093741 | 6727175 |
| big_line_3857（两路径） | 251014 | 1534849 | 21 | 765669 | 17304488 | 3752972 |
| building_3857（两路径） | 251014 | 1534849 | 19 | 320152 | 7164159 | 2479794 |
| lucc_sample（两路径） | 250 | 71042 | 10 | 1079 | 100782 | 42809 |
| points_4326 source | 250988 | 250988 | 24 | 2769383 | 74093011 | 6727175 |
| points_4326 prepared | 250988 | 250988 | 24 | 2769411 | 74093741 | 6727175 |

`big_line_3857` 是建筑面派生边界线，不能代表真实路网或长河流。原 LUCC 有 9384 行，本次仅测确定性的 250 行、71042 顶点子集。

## 稀疏高 zoom 合成补充

在 z14 的 40×25 个位置，每隔 8 个 tile 放一个 tile 中心。面为 `ST_Buffer(center,20,128)`：1000 个面、513000 顶点。点为相同 1000 个中心各放 10 个独立 fid：10000 点。均为 3857、z12–14、prepared、buffer 8、simplifyPixels 1；这些中心在所有测试 zoom 均远离边界。面启用 feature-driven，点启用 feature-major。准备输入不计入构建时间，扩展自己的准备阶段计入。

| 输入 | 优化前中位数 | 优化后中位数 | 优化前正式范围 | 优化后正式范围 |
|---|---:|---:|---|---|
| 稀疏面 feature-driven | 7.357 s | 0.954 s | 7.312–7.444 s | 0.950–1.049 s |
| 稀疏点 feature-major | 55.064 ms | 55.493 ms | 52.283–56.054 ms | 55.042–56.027 ms |

面输出 3000 tile / 3000 要素实例 / 199616 字节 / 最大 72 字节；点输出 3000 tile / 30000 要素实例 / 500616 字节 / 最大 167 字节。前后均相同。合成面体现重复简化成本较高时的收益；不是所有面数据的倍数承诺。稀疏点约 0.8% 的时间差处于本轮波动范围，不能宣称提速。

## 正确性与边界

12 个案例逐一比较相同输入、配置的前后结果：逐瓦片 layer 名称/版本/extent、fid、解析字典后的属性、几何命令序列及重复要素均相同；tile 坐标参与 SHA256，feature 顺序和属性字典顺序不参与。每个案例的逐层 tile 数、总字节、最大字节也一致。该校验比只比 tile 数严格，但不宣称整个 PBF byte-identical。

4326 点的 source 与 prepared **在基线中已存在** 28 个要素实例差异；优化保留了各自结果，因此不能将不同路径互相当成无条件的等价基准。本轮不声称修复该既有差异。

小范围 `parallel=4` 的补充基线在隔离库 wrapper 下未完成热身，已取消并清理；未保存有效计时，也不据此判断优化前并行性能。并行调度收益由回归测试验证行为，本报告没有并行加速数据。

本轮主环境仅 PG17；优化代码在该环境通过 124 项回归测试，未运行 PG13–18 全版本矩阵。没有独立记录 CPU、buffers/temp IO、WAL、几何函数调用次数及 bbox/prep/index 分段时间；本报告不能用于推断这些成本各下降多少。未重跑全量复杂 LUCC、真实道路、budget 重试或生产并发负载。

## 复现与原始记录

运行器：[bench/mvt_optimization.py](../bench/mvt_optimization.py)。需要已安装的 `psycopg` 和现有 `gpbench` 数据；不创建或重建原始业务 fixture。

```sh
PGHOST="$BENCH_SOCKET" PGPORT=28817 PGDATABASE=postgres \
  python3 bench/mvt_optimization.py before.json "$BASELINE_LIBRARY"
PGHOST="$BENCH_SOCKET" PGPORT=28817 PGDATABASE=postgres \
  python3 bench/mvt_optimization.py after.json "$OPTIMIZED_LIBRARY"
# 合成补充：在上述命令末尾加入 --sparse。
```

库参数为不可变本地扩展库路径（不带动态库后缀）；必须保留相同依赖锁文件并停止并发工作。运行器拒绝覆盖既有 benchmark schema/pyramid。每个 JSON 包含四轮原始时间、完整参数、源行/顶点数、最后一轮逐 zoom 统计、要素数及语义 SHA256。

原始记录：[主矩阵优化前](../bench/mvt_optimization_before.json)、[主矩阵优化后](../bench/mvt_optimization_after.json)、[合成优化前](../bench/mvt_optimization_sparse_before.json)、[合成优化后](../bench/mvt_optimization_sparse_after.json)。
