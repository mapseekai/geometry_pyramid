# MVT 金字塔生成性能审查（2026-09-08）

后续实现与优化前后实测见 [优化结果](mvt-performance-results-2026-09-08.md)。以下保留优化前分析。

审查代码：`9b1a605`。未修改构建实现。证据分为当前源码、仓库历史 benchmark、本轮隔离 SQL 微基准；未重跑用户全量数据，原 pgrx 28817 实例未启动。

## 判断

还有明显优化空间。优先顺序是：消除重复几何计算和排序宽记录，按数据类型设计 LOD，最后优化候选瓦片枚举和并行调度。数据量不能只看行数，还要看顶点数、feature 与 tile 的交叠次数、空候选数量、属性字节和输出瓦片大小。

粗略成本分解：

`总时间 = 范围计算 + 准备表/索引 + 各zoom(候选枚举 + 几何处理 + 分组编码 + 写入 + 重试) + 发布`

现有 `geometry_pyramid_stats.build_ms` 从逐层预算函数开始计时，不包含前面的 source bbox、prepared CTAS、建索引和最终 shadow swap。不能将各层时间之和当成完整调用耗时。

## 已有能力与实际边界

- baseline：逐层枚举整个 bbox 的 tile 网格，空间过滤后投影、简化、裁剪、MVT 聚合。
- prepared：将投影和几何类型预存，创建 GiST 并 ANALYZE；**没有预存简化结果**。`src/prepared.rs:194`、`:218` 与 `src/build.rs:1023`。
- feature-major points：纯 POINT、3857、prepared、单后端；按点坐标计算所属 tile。门槛检查的是整个构建的 maxZoom，触发后所有层都使用该路径，并非只切换高层。`src/build.rs:142`、`:167`、`:220`。
- feature-driven：按 feature envelope 展开候选格网，过滤、裁剪、聚合。复杂长线或大面可能展开大量候选。它不是线穿网格算法。
- parallel：当前已有 worker 可见的 prepared 表和 Morton 分块；不能把这两项再次列为待实现优化。小于等于64个格子的层在协调进程执行，但并行准备工作此前已发生。`src/build.rs:124`、`:580`、`:602`。
- `buildRules` 已支持逐层过滤、属性投影与确定性采样。
- budget：超限后删除整个 zoom 并重建；当前禁止与 `parallel > 1` 组合。上限耗尽时可能仍有超限瓦片，并非硬大小保证。

## 本轮新验证：几何表达式重复执行

prepared tile-major 的 `ST_AsMVTGeom(...)` 结果既供 `WHERE features.geom IS NOT NULL` 判空，又传入 `ST_AsMVT(features.*)`。SQL 子查询会被优化器展开，别名不保证结果只计算一次。[PostgreSQL CTE 文档](https://www.postgresql.org/docs/17/queries-with.html#QUERIES-WITH-CTE-MATERIALIZATION)说明了内联与 `MATERIALIZED` 的区别。

隔离环境使用本机 PostgreSQL17/PostGIS3.6.4、GEOS3.14.1、Wagyu0.5.0；不是启动原业务实例。测试2000个面，每面513顶点：

```sql
CREATE TABLE probe AS
SELECT i::bigint AS fid,
       ST_Buffer(
         ST_SetSRID(ST_MakePoint(1000+i%100,1000+i/100),3857),
         50,128
       ) AS geom_tile,
       'POLYGON'::text AS geom_type
FROM generate_series(1,2000) i;
ANALYZE probe;
```

从 `tile_pipeline_from_prepared_sql` 提取实际 SQL，绑定 z8/x128/y127、extent4096、buffer256、simplify tolerance1、面积/长度过滤0、点保留率1。单独测试其 SELECT/GROUP BY 聚合阶段，关闭 JIT 和 SQL parallel；使用 `EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, FORMAT JSON)`、`track_functions=all` 与事务内函数计数。

对照组仅把 tile 坐标、裁剪后的 geom、fid 放入 `MATERIALIZED` CTE，然后判空与聚合。各跑4次，剔除首轮后取3次中位数：

| 项目 | 原 SQL | 裁剪后物化 |
|---|---:|---:|
| 四轮执行时间 ms | 981.792 / 1001.264 / 988.207 / 980.436 | 505.298 / 493.869 / 490.353 / 495.089 |
| 后三轮中位数 ms | 988.207 | 493.869 |
| ST_SimplifyPreserveTopology 调用 | 4000 | 2000 |
| ST_AsMVTGeom 调用 | 4000 | 2000 |
| ST_TileEnvelope 调用 | 6000 | 4000 |
| 输出要素数 | 2000 | 2000 |

原计划还把原始 geom 带入排序，产生约10728kB的 external merge 临时磁盘排序。物化已裁剪的小几何同时减少了排序负担。

解码两个 MVT：图层名、版本、extent相同，2000个 feature 的二进制内容多重集相同。整体瓦片哈希不同，来自 feature 顺序不同；不能宣称 byte-identical。

**含义：这是已经复现的局部约2倍机会，不是整座金字塔2倍承诺。** 尚须在真实 INSERT/SPI、prepared 临时表与 GiST、点线面、各 zoom、属性宽度、并行和 budget 场景复测。其他生成路径结构相似，需要逐一检查计划，不直接套用本轮倍数。

生产方案应选择性保存已过滤的窄记录；无差别给所有 CTE 加 MATERIALIZED 可能导致整层中间结果落盘、失去谓词下推。

## 点

### 集中分布、低 zoom

主要成本是把大量点和属性编码进少数瓦片。原数据250,988点、z0–10仅24个tile，baseline3857为4.481s，prepared为5.097s；并非缺少更复杂的索引。

已有分层采样将4.481s降到0.830s，输出约71MiB降到4.2MiB。这是有损LOD的收益。推荐优先按业务重要性过滤，再考虑全局像素网格代表点或聚合点，保留count等统计；固定比例随机采样不能保证空间均匀与热点表达。

网格必须以全局投影原点对齐；相邻tile、zoom和动态读取使用同一选择规则。采样key/hash可在准备阶段算一次，避免每层重复拼接字段、文本转换与hash。准备阶段可以剔除在所有请求zoom都不可能保留的点，但必须计算各层保留条件的并集，不能只看某一层。

### 稀疏分布、高 zoom

feature-major和feature-driven已有真实价值。历史宽bbox点基准：baseline17.1s、prepared8.8s、feature-major2.9s（maxZoom14），不是当前业务数据的保证。

进一步优化：按层比较格网数R(z)、有效点数N(z)、候选数量与准备成本，替代只看maxZoom的门槛。可在准备表缓存maxZoom的整数归属坐标，低层归属由位移推导；buffer邻居仍需单独正确处理。这不是把子tile的PBF拼成父tile。

### 正确性前置条件

`src/build.rs:1062`快速路径只产生点的唯一所属tile，普通路径查询buffer覆盖的相邻tile，两者当前不等价。

验证点`POINT(1 1000000)`、3857、z1：在x0/y0及x1/y0的buffer256内，`ST_AsMVTGeom`都非NULL；快速路径assigned_x仅1。源表再有西侧点时构建rect能覆盖两列。应补齐边缘邻居、角点和世界边界测试，再把它作为等价加速方案推广。只对比tile总数不能证明feature集合一致。

## 线

历史线测试是由建筑面 `ST_Boundary` 派生的短边界，不能代表全国路网、河流或海岸线。3857的baseline14.951s、prepared14.628s，收益约2.2%。

1. 先消除同一线的重复简化/裁剪。对跨很多tile的长线，按zoom预简化一次，再供tile查询；保存中间结果的成本需小于省下的重复工作。
2. 在允许改变拓扑的纯显示线层，测试 `ST_Simplify`/其他轻量概化算法。当前所有非点都使用 PreserveTopology，可能为并不需要的约束付费。普通简化保留线端点，但不保证网络拓扑或不新增交叉；道路交叉口应先分段并保留节点。[官方说明](https://postgis.net/docs/ST_Simplify.html)
3. 低zoom按道路等级、河流等级过滤，保留主干；随机删线会切断网络。字段不足时不能将采样包装成无损优化。
4. 对长线先做可复用的空间分段或网格穿越候选算法。斜线的bbox可能覆盖k×k个格子，实际穿过的tile仅约O(k)；feature-driven的矩形展开仍会支付O(k²)候选成本。
5. 分段后保留原fid，区分part id，验证虚线相位、符号、重复标签、点击拾取和合并语义。

## 面

### 建筑与独立小面

低zoom先按可见面积/业务等级过滤，必要时按街区聚合表达；高zoom保留原面。只减少顶点可能不够，因为每个fid及属性仍有固定编码开销。历史25万建筑面z0–10约18s，prepared没有稳定收益。

### LUCC与复杂大面

历史9384个LUCC面，z0–6约75.125s；增加构建期简化/面积门槛后142.525s；预先Subdivide(2048)后的切片40.315s；采样10%后4.492s。必须区分：Subdivide属于几何重组，采样会删除覆盖区，后者不宜直接用于必须完整覆盖的土地利用或行政区面层。

优先路线：

1. 消除重复求值。
2. 为粗层建立可复用的概化几何，避免每个tile重新处理十万顶点级大面。
3. 对大的MultiPolygon先评估拆component，再只对复杂部分Subdivide；测试256/512/1024/2048等顶点上限。分块降低bbox误命中和局部裁剪顶点量，但增加行数、属性复制、fid管理与输出内部边界。[Subdivide官方说明](https://postgis.net/docs/ST_Subdivide.html)
4. 对真正有效的面coverage，考虑 `ST_CoverageSimplify`；不要以为逐面 `ST_SimplifyPreserveTopology` 就能保留相邻面共享边界。官方明确不保证此性质。Coverage工具有版本、GEOS和输入有效性前提，需检查。[拓扑简化](https://postgis.net/docs/ST_SimplifyPreserveTopology.html)、[Coverage简化](https://postgis.net/docs/ST_CoverageSimplify.html)
5. 只有样式与属性语义允许时按类别dissolve，避免丢失原实体身份。比较整体预处理成本，不能只报后续切片时间。

本机PostGIS已含Wagyu0.5.0，不能把“换用Wagyu”列为这里尚未获得的收益。其他部署可检查 `PostGIS_Wagyu_Version()`；官方也提示可能丢弃小面。[MVTGeom官方说明](https://postgis.net/docs/ST_AsMVTGeom.html)

## 通用实现优化

### 1. 精简准备阶段

`src/build.rs:542`先对所有几何ST_Transform后计算extent，随后prepared CTAS再投影一次。prepared模式可以先准备，然后在geom_tile上计算精确extent。不能直接将源extent矩形的投影当作所有投影后几何的精确范围。

prepared表不必保存所有属性与永远不被任一zoom选中的feature。按各层真正需要的字段并集和保留条件并集准备；其中raw SQL filter与重叠rule的AND语义须保留。

prepared已有投影/索引，并不等于已有LOD几何缓存。先实现单次构建内的按层复用，再按重建频率判断是否值得做跨构建持久缓存。

### 2. 消除重复整层计数

四条build路径都在每块INSERT前后 `count(*) WHERE z=...`，调用者又重新统计整层。B个块、T个tile的场景可能形成随B×T增长的额外行访问。

改用SPI影响行数，或窄 `INSERT RETURNING` 汇总，层结束统一一次统计。并行时前后计数之差还会包含其他worker新提交的行，不能当成本块可靠计数；目前最终总数另算，尚未证实最终总数错误。`src/build.rs:1271`、`:1329`、`:1384`、`:1441`、`:1465`。

### 3. 修正feature-driven成本模型

`src/build.rs:299`估计pairs时没有buffer及±1tile安全pad，实际`:1141`有。一个短小feature可能被估计为1个pair，实际展开9个或更多候选，最后才裁剪判空。

应使用与执行一致的候选计数模型，按zoom考虑规则过滤后数据规模。可测试增加安全的廉价空间预过滤，但不能直接删除保护边界正确性的pad。对于少量超大feature，可评估小feature展开、大feature单独处理；收益需用真实分布证明。

### 4. 降低budget重试成本

现在单个tile超限也会重建整层；历史mixed27.073s在1MB预算下变为163.967s。优先用LOD让第一次构建接近目标，再考虑只重建超限tile或分块。

局部重试会改变现有“整层统一阈值”的语义，可能造成相邻tile细节不一致，因此不是无条件等价替换。若仍要求整层统一，则缓存可复用几何并优化阈值预测更合适。

### 5. 并行按实际工作量决策

现有Morton分块是静态分配，按空间位置分散工作，尚不是按顶点/候选/字节估计的负载均衡。后续可测试成本加权分块、动态队列、复用worker；先确认CPU计算占比高于worker启动和准备成本。

少数巨大tile不会因增加tile worker而自然拆成多个并行任务。PostGIS支持并行ST_AsMVT聚合，但当前INSERT/SPI路径不能据此假设已有数据库内部并行；普通INSERT与临时表访问有约束，需检查实际Gather/Partial Aggregate计划。[ST_AsMVT](https://postgis.net/docs/ST_AsMVT.html)、[PG并行查询条件](https://www.postgresql.org/docs/17/when-can-parallel-query-be-used.html)、[并行安全](https://www.postgresql.org/docs/17/parallel-safety.html)

历史mixed LOD从serial4.436s退化到parallel4的22.147s，说明不能默认“核心越多越快”。

### 6. 参数单位与存储

`simplifyPixels`除以的是MVT extent（默认4096），不是256px显示瓦片的宽度。在256px显示假设下，simplifyPixels=2只相当于0.125屏幕像素；0.5–1屏幕像素对应8–16个MVT单位。这个换算有助于理解参数，**不意味着应直接把值调大**，不同显示tile宽度、缩放和质量目标需另行校准。

提高work_mem有时能降低排序落盘，但应先让排序只携带裁剪后的小几何与必需字段，并按worker数与每query节点数核算总内存。

并行prepared当前使用普通logged表，可评估可重建中间表使用UNLOGGED；收益应测WAL和端到端时间。最终tile表持久性不应一并降级；UNLOGGED影子表转LOGGED的成本也需算入。延后建输出主键、去除未使用id序列等仅在写入已成为主要成本时优先。

`splitSize`目前在MVT聚合之后过滤，调高它主要减少存储，不能保证减少之前的几何和编码成本。也不能简单从“父tile没有物化”推断所有子tile都为空：LOD、采样、简化坍塌与splitSize会破坏这种推断。

## 优先级与验收

| 顺序 | 工作 | 主要收益对象 | 证据 |
|---|---|---|---|
| P0 | 几何结果只求值一次，排序窄化 | 线、复杂面 | 本轮局部SQL约2倍，整库待测 |
| P0 | 修正point buffer邻居一致性 | 点 | 本轮PostGIS边界复现 |
| P1 | 省去每块重复count与重复extent投影 | 大量tile、复杂投影 | 代码确认冗余 |
| P1 | 点粗层网格/重要性LOD | 集中点 | 历史采样约5.4倍，聚合待测 |
| P1 | 复杂面/长线按zoom预处理复用 | 复杂线面 | 历史Subdivide切片约1.86倍 |
| P2 | 候选成本模型与重几何分流 | 稀疏高zoom | 代码确认估计偏差 |
| P2 | 加权任务/worker复用/budget重试 | 大构建 | 需完整benchmark |

每项优化必须同时报告：总wall time、bbox/prep/index时间、每层时间、CPU、buffers/temp IO、WAL、worker分布、几何调用次数、输出tile数、要素数、最大/总字节。预处理方案分别报告首次全成本与复用成本。

数据矩阵至少包括集中点、稀疏点、真实路网、长斜线、小建筑、复杂LUCC。设置低中高zoom与buffer0/默认/大值，覆盖serial/parallel和规则。相同输入、参数和数据快照下，先检查每tile解码后的ID/属性/几何，再比较性能；有损LOD另外评估道路连通性、覆盖空洞、面积误差和标签/拾取。

本轮新增Multica：MAP-203重复几何求值、MAP-204点buffer缺失、MAP-205重复整层计数、MAP-206候选估计遗漏扩张。已有MAP-117/118/119/120覆盖并行选择、LOD缓存、点聚合和复杂面预处理，不重复创建。

历史数据来源：[用户数据性能报告](user-data-build-performance.md)、[构建速度分析](build-speed-deep-dive.md)。历史文档中“prepared已经一次性simplify”“只支持serial”等描述与当前代码不完全一致，本报告以代码为准。
