# 配置与 LOD 领域概念

## 配置形态

`ST_BuildPyramid(table, geom, fid, config)` 接收一个 JSON 文档，并解析为 `PyramidConfig`（`src/config.rs`）。空配置表示使用默认值。`README.md` 中的公开选项表是最好的用户参考；源码是校验规则的权威来源。

核心分组：

- **身份和坐标系**：`name`、`sourceSRS`、`tileSRS`、`tileSize`、`tileExtend`、`minZoom`、`maxZoom`。
- **读取/构建形态**：`dynamicBeyondMaxZoom`、`splitSize`、`parallel`、`buildTimeoutMs`。
- **构建提速路径**：`preparedBuild`、`featureMajorPoints`、`featureMajorMinZoom`、`featureDrivenEnum`。
- **泛化和大小控制**：`attributes`、`simplifyPixels`、`minFeatureSizePixels`、`dropRate`、`tileSizeBudgetBytes`、`maxTileSizeAttempts`。
- **层级细节规则**：`buildRules`。

校验会强制 `minZoom <= maxZoom`、`maxZoom <= 24`、`0 < tileSize <= 16384`、`tileExtend < tileSize`、合法规则标识符、兼容的规则属性覆盖和采样范围（`src/config.rs`）。类似标识符的配置字段被有意限制为裸标识符；schema-qualified 源表名不属于当前模型。

## Build rules：唯一的原生 LOD 表面

`buildRules` 是本仓库唯一的层级细节（LOD）表面。每条规则使用项目原生形态：

```json
{
  "minZoom": 0,
  "maxZoom": 4,
  "filter": "scalerank < 5",
  "attrFields": ["name", "kind"],
  "sample": { "rate": 25, "key": ["fid"], "geometryTypes": ["POINT"] }
}
```

来自 `README.md`、`src/config.rs` 和 `src/build.rs` 的语义：

- 规则覆盖闭区间 zoom band `[minZoom, maxZoom]`。
- `filter` 是针对源列求值的原始 SQL boolean 表达式。
- `attrFields` 覆盖被覆盖 zoom 输出到 MVT 的属性。
- `sample.rate` 是 `0` 到 `100` 的百分比；`sample.key` 默认是 `["fid"]` 并驱动确定性 hash；`sample.geometryTypes` 可按 geometry family 限制采样。
- 多条规则覆盖同一 zoom 时，过滤/采样按 AND 组合。如果多条覆盖规则都设置了 `attrFields`，它们在该 zoom 必须一致。
- 未被任何规则覆盖的 zoom 使用全部源要素和全局 `attributes` 列表。

扩展有意**不**实现第二套 Ganos 原生 `level` / `value` 规则形态。历史 Ganos 对比文档可能提到缺失的 LOD 功能；当它们与当前行为冲突时，优先相信当前 `README.md` 和 `src/config.rs`。

## LOD 规则应用位置

近期 `buildRules` 工作让规则行为在构建/读取变体之间保持一致：

- 基线 tile-major 构建在 `src/build.rs` 中应用 zoom-effective 规则子句。
- Prepared 构建在 `src/prepared.rs` 中把所需规则数据投影到 prepared 表，并从 `src/build.rs` 应用 prepared-rule 子句。
- `featureMajorPoints` 和 `featureDrivenEnum` 都有验证过滤输出的测试（`src/lib.rs`）。
- 动态读取（`splitSize` miss 和 `z > maxZoom`）调用 `dynamic::generate_tile`，它使用同一套 tile pipeline 和 zoom-effective `buildRules` 子句（`src/dynamic.rs`）。
- 并行 worker job 携带完整 `PyramidConfig` 和 worker 可见的 prepared table 信息，因此 worker strip 使用同样选中的构建路径（`src/worker.rs`）。

修改此区域时，要同时检查 `src/build.rs` 中 source 与 prepared 两类规则目标；只修一条路径可能导致存储/动态、串行/并行输出不一致。

## Filter 的安全姿态

`buildRules.filter` 是受信任管理员 SQL，类似 view definition。它以调用者权限执行，且不能像普通值表达式那样参数化（`README.md`、`src/config.rs`）。代码拒绝分号作为单语句保护，并在大量构建工作前探测 filter，但这是防御性约束，不是 SQL 沙箱（`src/build.rs`）。

不要通过 `SECURITY DEFINER` wrapper 暴露带 filter 的构建 API，也不要把它开放给不应在源表上运行任意谓词的角色。

## 性能旋钮和取舍

- `splitSize:0` 会物化 `maxZoom` 内所有非空瓦片；读取最快，miss 会直接返回 `NULL`，不会实时切片。
- `splitSize > 0` 会在构建期跳过稀疏瓦片，减少构建/存储工作，但后续每次 miss 都会运行一次实时源表切片。使用它时应保持源表 GiST 索引健康（`src/tile.rs`、`src/dynamic.rs`）。
- `preparedBuild:true` 预转换源 geometry；并行构建中 prepared 表会被提交，并对 worker 可见（`src/prepared.rs`）。
- `featureMajorPoints:true` 仅用于纯点源，要求 `tileSRS=3857`、`preparedBuild:true`、`parallel=1`，并且金字塔足够深（`featureMajorMinZoom`）。
- `featureDrivenEnum:true` 是适用于所有 geometry 的稀疏/高 zoom 路径。它会比较 feature-envelope tile pairs 与逐 zoom lattice 大小，在密集枚举更便宜时自动 fallback（`src/build.rs`）。
- `tileSizeBudgetBytes` 是输出保护循环，会收紧简化、min-size、drop 阈值并重建某个 zoom。它会拒绝与 `parallel > 1` 组合，因为 worker 已提交行与 coordinator retry delete 无法安全组合（`src/build.rs`）。

`docs/user-data-build-performance.md` 发现，在已检查 benchmark 集中，`buildRules.sample` 是复杂 LUCC polygon 在粗 zoom 上最强的优化杠杆，而 `tileSizeBudgetBytes` 主要以显著构建时间成本保护瓦片大小。

## 变更指导

- schema/validation 变更从 `src/config.rs` 开始，然后检查 `src/build.rs`、`src/prepared.rs`、`src/dynamic.rs` 和 `src/worker.rs` 中的每个调用者。
- 对每条被触及的构建路径新增或更新 `src/lib.rs` 中的 pg 测试：baseline、prepared、feature-major points、feature-driven、dynamic，以及相关时的 parallel。
- 保持 README 示例和本页与实际原生 JSON 形态一致。
- 把 `docs/ganos-gap-analysis.md` 视为历史背景；其标题说明部分状态行已过时。
