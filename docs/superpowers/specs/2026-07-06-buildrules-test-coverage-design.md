# buildRules 后续端到端测试覆盖设计

状态: 已批准, 待实现计划
日期: 2026-07-06
关联: MAP-16, MAP-17, MAP-18, MAP-38

## 背景

`buildRules` 已经接入普通构建、prepared 构建、feature-major-points、feature-driven
和动态 `ST_Tile` 按需切片路径。当前剩余风险不是已知运行时缺陷,而是几个高价值路径缺少
端到端回归证据。字符串单测能证明 SQL 片段形态正确,但不能证明实际构建出的 MVT 瓦片在
这些路径上使用了同一组 LOD 规则。

本设计只覆盖 `MAP-16`、`MAP-17`、`MAP-18`,完成后可关闭父项 `MAP-38`。`MAP-21`、
`MAP-22`、`MAP-39` 属于未来 LOD spec,不在本轮实现范围内。

## 目标

- 为动态按需读路径补端到端测试,覆盖 `splitSize` 延迟瓦片和 `z > maxZoom` 动态瓦片。
- 为 prepared tile-major 路径补多规则 AND 端到端测试,证明 `_gp_rule_k` prepared 列真实参与输出过滤。
- 为 `featureMajorPoints` 和 `featureDrivenEnum` 路径补带规则端到端测试,证明优化路径没有漏用 `buildRules`。
- 测试失败时能定位到具体路径没有传入或没有应用 rule clause。

## 非目标

- 不主动重构构建或动态读取运行时代码。
- 不重写现有测试组织方式,不引入新的测试框架或依赖。
- 不为未来逐层属性列子集、逐层采样/抽稀设计配置 API。
- 不依赖随机数据或性能 benchmark 证明正确性。

如果新增测试暴露真实缺陷,实现计划可包含最小运行时修复分支,但修复范围只限失败测试定位到的路径。

## 测试架构

新增测试放在 `src/lib.rs` 的现有 `#[pg_test]` 模块里,沿用当前 pgrx 集成测试风格。fixture 使用小型
PostGIS 表,包含稳定的 `gid`、可过滤属性和明确落点,例如 `rank`、`kind`。测试优先用
`ST_Tile(...) IS [NOT] NULL` 断言 feature 集合,避免只比较 SQL 字符串或脆弱的 MVT 原始字节。

需要确认优化路径确实被触发时,使用现有配置旋钮:

- `featureMajorPoints=true`, `preparedBuild=true`, `featureMajorMinZoom=0` 稳定触发纯点 feature-major 路径。
- `featureDrivenEnum=true`, `preparedBuild=true`, 稀疏数据加 world-corner outlier 让高 zoom 满足
  `R(z) > C * rows`,稳定进入 feature-driven 路径。

如测试涉及 parallel/prepared worker,源数据必须通过已有 autonomous worker 方式提交到 worker 可见的事务边界。
本轮优先使用 serial prepared 测试,避免把 worker 可见性变成本轮主要变量。

## MAP-16: 动态按需读路径

测试构造一张点表,至少包含保留 feature 和剔除 feature,二者落在不同 tile。使用同一数据构建三个 pyramid:

- 物化基线: `splitSize=0`,覆盖目标 zoom 的 `buildRules`。
- `splitSize` 延迟版: `splitSize` 大于目标 tile feature count,使 `ST_Tile` 在 `maxZoom` 内 storage miss 后调用
  `dynamic::generate_tile`。
- beyond-maxZoom 动态版: 较低 `maxZoom`,但规则覆盖请求的高 zoom,使 `ST_Tile` 走 `z > maxZoom` 动态切片。

断言:

- covered zoom 上,保留 feature 的 tile 存在,剔除 feature 的 tile 为 NULL。
- `splitSize` 延迟版与物化基线在同一 covered zoom 上存在性一致。
- `z > maxZoom` 请求在 covered zoom 上同样应用规则。
- 未 covered zoom 不额外过滤,原本被 covered zoom 排除的 feature 应重新出现。

## MAP-17: prepared 多规则 AND

测试使用 `rank` 和 `kind` 两个独立属性,配置两条有重叠 zoom band 的规则:

- z1 只被第一条规则覆盖。
- z2 同时被两条规则覆盖。
- z3 只被第二条规则覆盖。
- 另保留一个未 covered zoom。

构建配置使用 `preparedBuild=true`,避免走基线源表路径。测试数据包含:

- 同时满足两条规则的 feature。
- 只满足第一条的 feature。
- 只满足第二条的 feature。
- 两条都不满足的 feature。

断言:

- z1 只应用第一条规则。
- z2 表现为 AND,只有同时满足两条规则的 feature 进入 tile。
- z3 只应用第二条规则。
- 未 covered zoom 不过滤。

该测试证明 prepared CTAS 生成的 `_gp_rule_k` 列、prepared 别名和 per-zoom clause 在实际 MVT 构建中一起工作。

## MAP-18: 优化路径带规则

新增两个端到端测试。

### feature-major-points

使用纯 POINT 源表和 WebMercator 默认 tile SRS。配置:

- `preparedBuild=true`
- `featureMajorPoints=true`
- `featureMajorMinZoom=0`
- 覆盖目标 zoom 的 `buildRules`

数据让保留点和剔除点落在明确不同 tile。断言 covered zoom 上保留 tile 存在、剔除 tile 为 NULL。测试还应包含非空输出保护,避免路径没有实际生成 tile 时误判通过。

### feature-driven

使用 mixed geometry 或点线面混合稀疏 fixture,加 outlier 扩大 bbox,稳定触发高 zoom 的 feature-driven 路径。配置:

- `preparedBuild=true`
- `featureDrivenEnum=true`
- 覆盖目标 zoom 的 `buildRules`

断言 covered zoom 上被规则排除的 feature 不出现在输出中,保留 feature 对应 tile 存在。默认不增加普通 prepared 对照;只有当 fixture 无法从配置和非空输出证明 feature-driven 路径被触发时,实现计划才补一组同 fixture 的普通 prepared 存在性对照。全量字节等价不作为主要正确性证据。

## 错误处理与修复分支

本轮计划默认只新增测试。如果某个测试失败:

1. 先确认测试稳定触发目标路径,不是 fixture 或 zoom/tile 坐标错误。
2. 将失败缩小到一个路径: dynamic source clause、prepared rule projection、feature-major clause 或 feature-driven clause。
3. 只修改对应路径的 rule clause 传递或别名使用。
4. 保留新增测试作为回归保护。

不做顺手重构,不改变 `buildRules` 的配置语义或安全边界。

## 验证

实现阶段先运行目标测试名筛选,再运行全量:

```bash
cargo pgrx test pg17
```

验收证据:

- 新增目标测试通过。
- 全量 `cargo pgrx test pg17` 通过。
- 如环境缺 PostgreSQL/PostGIS 或全量测试超时,报告具体命令、失败原因和已通过的目标测试范围,不宣称完成。

## 风险

- pgrx `#[pg_test]` 运行在事务中,parallel worker 看不到未提交源表;本轮优先不引入新的 parallel 测试。若必须覆盖 worker,使用已有 `exec_sql_autonomous` 模式。
- Feature-driven 是否触发受 bbox 和 row count 启发式影响;fixture 必须包含稀疏分布和 outlier,并在失败消息中说明目标路径。
- 只用 tile 存在性断言无法解码同 tile 内多个 feature 的属性;因此测试数据应让关键 feature 落在不同 tile,使存在性足够表达 feature 集合。
