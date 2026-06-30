# 待办 / 已知问题

`buildRules`(LOD)工作完成后的后续事项。**均不阻断合并**——功能已完成,
测试全绿(90/90),全分支评审结论为 READY TO MERGE。

## 测试覆盖(后续补充,超出 spec §8)

以下路径在结构上已正确(共用 `rules_clause_for_zoom`、流水线拼接位置一致),
且已被间接覆盖,但缺少专门的端到端回归测试。方便时补上:

- [ ] **按需读路径**经 `dynamic::generate_tile` 的 `buildRules`:一个
      `splitSize > 0` 且在 `maxZoom` 以内的瓦片,以及一个 `z > maxZoom` 且被规则
      覆盖的瓦片——验证它们在被覆盖的 zoom 上与物化瓦片一致。
- [ ] **prepared 路径上的多规则 AND** 端到端测试(当前 AND 测试走的是基线
      源表路径;prepared 的 AND 只有字符串单测覆盖)。
- [ ] **feature-major-points** 与 **feature-driven** 路径带规则的端到端构建
      (目前只接入 + 字符串单测,未做端到端构建)。

## 代码清理

- [ ] SQL 投影 helper 里的 **`push_str(&format!(…))` → `write!(…)`**
      (`rule_projections_for`、`rules_clause_for_zoom`,以及兄弟函数
      `attribute_projections_for`)。**有意暂缓**:冷构建路径;且只改规则 helper
      会与兄弟函数风格分裂。要么三个一起改,要么都不改。

## 工具

- [ ] 刷新 **GitNexus 索引**(自 `9657a03` 起过期):
      在项目根目录运行 `node .gitnexus/run.cjs analyze`。

## 未来 LOD 增强(超出当前 `buildRules` spec 范围)

当前 `buildRules` 只做逐层要素**过滤**。可能的后续 spec:

- [ ] 逐层**属性列子集**(粗层丢弃重型属性列)。
- [ ] 逐层**采样 / 抽稀**(按比例保留要素,而非按谓词)——把 `dropRate`
      (目前仅对点生效)推广到线/面。

## 现有减量旋钮说明(背景,非任务)

- `dropRate` —— 仅对点生效;对线/面要素数无影响。
- `minFeatureSizePixels` —— 基于几何尺寸,对长要素在约 z1 之后近乎无效;
  不感知重要性。
- `simplifyPixels` —— 只减每要素顶点数,不减要素数量。
- `buildRules` —— 唯一能按属性/重要性减少粗层要素**数量**的旋钮。
