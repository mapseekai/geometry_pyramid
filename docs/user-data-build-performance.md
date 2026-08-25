# 用户数据构建性能分析

测试日期：2026-07-16<br>
测试对象：`geometry_pyramid` `ST_BuildPyramid`

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
