# buildRules:逐层要素过滤(LOD)设计

状态:已批准,待实现计划
日期:2026-06-29
关联:`ST_BuildPyramid` 构建路径;对齐 Alibaba Ganos `buildRules`。

## 1. 目标与动机

`buildRules` 是金字塔的**逐层 Level-of-Detail 机制**:把一个 zoom 区间绑定到一个
布尔条件,只有满足条件的源要素才在该层级范围内参与建瓦片。典型用法是粗层只保留
"重要"要素(如 `scalerank < 5`),从源头减少粗层的要素数与瓦片字节,而无需在
`ST_AsMVTGeom`/编码阶段做无效功。

这是真实数据上构建提速这条线唯一剩下的**真实能力缺口**:`dropRate` 只对点生效,
`minFeatureSizePixels` 在 z1 以后近乎 0 效果,二者都不能按属性/层级缩减线/面的要素数。
`buildRules` 是**有损**的(它改变粗层瓦片内容),这是其设计意图,不是缺陷。

**非目标(本期不做,留待后续 spec):** 逐层不同的属性列子集;逐层采样率/抽稀。
本期只做"逐层要素过滤"这一最小集。

## 2. 配置 schema

`ST_BuildPyramid` 的 JSON 配置新增可选字段 `buildRules`:

```jsonc
"buildRules": [
  { "minZoom": 0, "maxZoom": 4, "filter": "scalerank < 5" },
  { "minZoom": 5, "maxZoom": 8, "filter": "scalerank < 8" }
]
```

- `src/config.rs` 新增:
  ```rust
  #[derive(Debug, Clone, Deserialize)]
  pub struct BuildRule {
      #[serde(rename = "minZoom")] pub min_zoom: u8,
      #[serde(rename = "maxZoom")] pub max_zoom: u8,
      pub filter: String,
  }
  ```
  并在 `PyramidConfig` 增 `#[serde(default, rename = "buildRules")] pub build_rules: Vec<BuildRule>`。
- 默认空数组 = 现状,**完全向后兼容**。
- 字段名复用现有 `minZoom`/`maxZoom` 风格。

## 3. 语义

- **未被任何规则覆盖的 zoom → 纳入全部要素**(向后兼容;只有写了规则的层才过滤)。
- **多条规则覆盖同一 zoom → AND 全部命中谓词**。可组合:一条全局
  `[0-22] geom IS NOT NULL` 叠一条粗层 `[0-4] scalerank < 5`,在 z3 两者 AND。比
  "首条命中"更可预测,与顺序无关。
- 形式化:对 zoom `z`,适用规则集 `R(z) = { 规则 r | r.minZoom ≤ z ≤ r.maxZoom }`;
  该层过滤条件 = `AND_{r ∈ R(z)} r.filter`;`R(z) = ∅` 时不加任何过滤。

## 4. 架构:预计算布尔列

核心约束:prepared 表只携带 `fid, geom_tile, geom_type` 加 `attributes` 列;过滤
表达式引用的任意源列在 prep 里不可见。解法是**把"每层不同的谓词"降维成"建表期一次性
求值的布尔列"**:

- **Prepared / 并行 / feature-driven / feature-major 路径(读 prep):**
  prepared 表 CTAS 时,为每条规则增列 `(<rule.filter>) AS _gp_rule_k`(`k` 为规则在
  数组中的下标)。每层查询对 `R(z)` 追加 `AND _gp_rule_i AND _gp_rule_j …`(引用预计算
  布尔列,不重算表达式)。
- **基线 tile-major 路径(读源表):** 源表本就有全部列,每层直接追加
  `AND (<rule.filter>)`(对 `R(z)` 的各 filter 用 `AND` 连接)。
- 两条路径对相同输入产生**相同结果**。

**为什么不是其他方案:** (B) 把表达式引用的源列搬进 prep——需要从任意 SQL 里解析列名
(脆弱)或搬全部列(昂贵)。(C) 每层直接查源表绕过 prep——废掉 prepared/并行优化、
worker 改读源表。均否决。预计算布尔列把注入面也收敛到一个**构建期、受信任、即时校验**
的点(与现有 `attributes` 投影同姿态)。

## 5. 求值与安全

- **护栏(`PyramidConfig::validate()` 内):** 每条规则 `min_zoom ≤ max_zoom`;`filter`
  非空(trim 后);`filter` **拒绝含 `;`**(强制单表达式,挡掉多语句注入)。
- **预校验探针:** 构建开始、任何瓦片/影子表动作之前,对每条规则跑一次
  `SELECT 1 FROM <src> WHERE (<filter>) AND false LIMIT 1`。语法错误/未知列以**指明
  是第几条规则**的清晰错误立即失败,不破坏现有原子 swap(失败发生在影子表建立前)。
- **安全姿态:** `filter` 以 `ST_BuildPyramid` 调用者(通常是表 owner)的权限求值,等同
  视图定义/函数体的受信任管理员 DDL 输入。该姿态在 README 与字段文档中显式说明。这是与
  全局"参数化查询防注入"规则的**有意例外**——任意谓词天生无法参数化,Ganos 亦然;护栏
  (单语句 + 构建期校验 + 仅 owner 权限)是补偿控制。

## 6. 并行交互

规则布尔列存在于 **committed prepared 表**(`_gp_prep_<name>`)里,后台 worker 直接
继承——**无需改 worker 协议**。Morton 读过滤(`geom_tile && <block envelope>`)与规则
过滤正交,在同一 `WHERE` 里叠加即可。

## 7. 受影响代码

- `src/config.rs`:`BuildRule` 结构、`build_rules` 字段、`validate()` 护栏;单测。
- `src/prepared.rs`:`prepared_select_sql`(及两处 CTAS 装配 `build_prepared_table_sql`
  /`exec`)增加规则布尔列投影。新增 `rule_projections_for(cfg) -> String` 生成
  `, (<f0>) AS _gp_rule_0, (<f1>) AS _gp_rule_1 …`(类比 `attribute_projections_for`)。
- `src/build.rs`:
  - 新增 helper:`rules_clause_for_zoom(cfg, z, target) -> String`,`target ∈ {Prep, Source}`。
    Prep 返回 `" AND _gp_rule_i AND …"`,Source 返回 `" AND (<fi>) AND …"`,`R(z)=∅` 返回 `""`。
  - 四个 per-zoom 调度函数把该 clause 线程进对应 pipeline builder:
    `build_zoom_level`→`tile_pipeline_from_sql`(Source);
    `build_zoom_level_prepared`→`tile_pipeline_from_prepared_sql`(Prep);
    `build_zoom_level_prepared_points_feature_major`→`point_feature_major_pipeline_sql`(Prep);
    `build_zoom_level_prepared_feature_driven`→`feature_driven_pipeline_sql`(Prep)。
  - 四个 pipeline builder 各加一个 `rule_clause: &str` 参数,拼进既有 `WHERE`。
  - 预校验探针在 `build_into` 早段(prepared 表/影子表建立前)执行。
- `README.md`:`buildRules` 配置说明 + 安全姿态注记。

## 8. 测试

- **单测(`config.rs`):** `buildRules` 解析——合法;非法:`minZoom > maxZoom`、空
  `filter`、含 `;` 被拒。
- **pg_test:**
  1. 粗层加规则后该层要素数 **<** 无规则;深层(未覆盖层)要素数不变。
  2. `filter:"true"`(或恒真)与无规则 **字节一致**(等价回归)。
  3. 并行 `buildRules` == 串行(逐瓦片 `octet_length` FULL JOIN,0 mismatch)——
     验证布尔列经 committed prep 流到 worker。
  4. 多规则覆盖同层 → AND 语义(两条规则交集生效)。
  5. 非法 filter(未知列)→ 干净报错 + **无残留金字塔**(原子性:`to_regclass` 为 NULL)。
- 复用现有 Natural Earth roads/urban 或合成带属性数据。

## 9. 向后兼容

- `build_rules` 默认空 = 现状。所有现有路径在空规则下生成的 SQL 与 clause 为 `""`,
  零行为变化(由测试 2 的字节一致保证)。
- 不改 worker 协议、不改存储 schema。
- **`ST_Tile` 读路径(实现期修订):** 原计划"不改读路径",但 `dynamic::generate_tile`
  与构建基线共用 `tile_pipeline_from_sql`,该参数是编译期强制的;传 `""` 会让按需切片
  (splitSize 延迟或超出 maxZoom)的瓦片与物化瓦片**不一致**。因此按需读路径同样应用
  `RuleTarget::Source`(原始 src 谓词),使按需瓦片与物化瓦片在同一 zoom 上一致;未被任何
  规则覆盖的 zoom 子句为空,读路径行为不变。
