# geometry_pyramid 使用文档

本文档说明如何把 `geometry_pyramid` 作为 PostgreSQL/PostGIS 扩展使用：安装或启用扩展、准备源数据、构建矢量瓦片金字塔、读取瓦片、刷新变更，以及选择常见配置模式。

实现细节见[架构概览](architecture/overview.zh-CN.md)。更深入的流程说明见[金字塔生命周期工作流](workflows/pyramid-lifecycle.zh-CN.md)和[配置与 LOD 领域概念](domain/config-and-lod.zh-CN.md)。

## 1. geometry_pyramid 做什么

`geometry_pyramid` 从 PostGIS geometry 表构建 Mapbox Vector Tile（MVT）金字塔。源表通过 `ST_BuildPyramid` 注册，瓦片物化到每个金字塔独立的存储表，应用通过 `ST_Tile` 读取瓦片字节。

扩展把 geometry 处理和 MVT 编码交给 PostGIS（`ST_TileEnvelope`、`ST_AsMVTGeom`、`ST_AsMVT`），并通过 pgrx/Rust 协调整个构建和读取生命周期。

## 2. 使用要求

运行时需要：

- 已安装 `geometry_pyramid` 扩展的 PostgreSQL。
- 目标数据库中启用 PostGIS。
- 一个源表，包含：
  - geometry 列，
  - 稳定的要素 id 列，
  - geometry 列上的空间索引，用于实际构建和动态读取性能。

如果从本仓库本地开发，常用命令是：

```bash
cargo check
cargo pgrx run pg17
cargo pgrx test
cargo pgrx package
```

该 crate 默认使用 PostgreSQL 17 features，并面向 pgrx `0.18.0`（`Cargo.toml`）。README 安装说明覆盖 PostgreSQL 13-18 feature flags。

## 3. 启用扩展

每个数据库执行一次：

```sql
CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;
```

确认安装的扩展版本：

```sql
SELECT geometry_pyramid_version();
```

## 4. 准备源数据

典型源表包含主键或稳定 id 列，以及 PostGIS geometry 列：

```sql
CREATE TABLE buildings (
  gid  serial PRIMARY KEY,
  name text,
  kind text,
  geom geometry(Polygon, 4326)
);

-- 先加载数据，再添加空间索引。
CREATE INDEX buildings_geom_gix ON buildings USING gist (geom);
```

建议：

- 金字塔配置中的 `sourceSRS` 必须匹配源 geometry 的 SRID。
- 要素 id 列应在重建和更新之间保持稳定。
- 保持 geometry 列上的 GiST 索引，尤其是在使用动态读取（`splitSize > 0` 或超过 `maxZoom` 读取）时。
- 当前标识符输入必须是较短的裸标识符；暂不支持 schema-qualified 源表名。

## 5. 构建金字塔

使用 `ST_BuildPyramid(table, geom_column, fid_column, config_json)`：

```sql
SELECT ST_BuildPyramid(
  'buildings',
  'geom',
  'gid',
  '{
     "name": "buildings",
     "sourceSRS": 4326,
     "tileSRS": 3857,
     "minZoom": 0,
     "maxZoom": 14,
     "attributes": ["name", "kind"]
   }'
);
```

重要行为：

- `name` 配置项指定金字塔名称。它默认使用源表名，但建议显式设置。
- 对同一金字塔名称重复运行 `ST_BuildPyramid` 会原地重建。
- 完整重建是原子的：瓦片先写入 shadow 存储，再切换为正式表，因此读请求不应看到半成品。
- 空源或 geometry 全为 NULL 的源会构建为合法的空金字塔。

## 6. 读取瓦片

使用 `ST_Tile(name, z, x, y)`：

```sql
SELECT ST_Tile('buildings', 14, 13488, 6210);
```

返回值：

- 当瓦片存在时，返回包含 MVT/PBF 字节的 `bytea`。
- 当瓦片为空、超出配置范围、被禁用、坐标无效或金字塔名未知时，返回 `NULL`。

读取模式：

- 在 `maxZoom` 范围内，如果存储中存在瓦片，则从 `_gp_tiles_<name>` 读取。
- 使用默认 `splitSize:0` 时，`maxZoom` 内所有非空瓦片都会物化，因此 miss 会直接返回 `NULL`，不会动态切片。
- 使用 `splitSize > 0` 时，稀疏瓦片可能在构建期被跳过；`maxZoom` 内的存储 miss 会从源表实时切片。
- 高于 `maxZoom` 时，只有 `dynamicBeyondMaxZoom` 为 `true`，`ST_Tile` 才会实时切片。

客户端需要瓦片 envelope 时，可使用 PostGIS：

```sql
SELECT ST_TileEnvelope(14, 13488, 6210);
```

## 7. 查看金字塔和构建统计

查看单个金字塔：

```sql
SELECT ST_Pyramid('buildings');
```

列出所有金字塔：

```sql
SELECT * FROM ST_Pyramids();
```

元数据包含配置和逐 zoom 统计，例如瓦片数量、总字节数、最大瓦片字节数、构建耗时和重试次数。

每个金字塔把瓦片存储在 `_gp_tiles_<name>` 中，MVT 字节位于 `mvt` 列。除非明确需要直接访问存储，否则应用读取建议使用 `ST_Tile`。

## 8. 源数据变化后的刷新

常见刷新模式有三种。

### 完整重建

使用空 where clause：

```sql
SELECT ST_UpdatePyramid('buildings', '');
```

当大量行变化、配置变化，或需要最安全的简单修正时使用。

### 谓词刷新

刷新与当前匹配行相交的瓦片：

```sql
SELECT ST_UpdatePyramid('buildings', 'name = ''City Hall''');
```

当谓词确实覆盖变更区域时，这很快。但对于删除或移动出原位置的要素，如果旧瓦片位置不再被谓词覆盖，则该方式不足以清理旧瓦片。

### Bbox 刷新

从当前源表刷新与某个区域相交的所有瓦片：

```sql
SELECT ST_UpdatePyramidByExtent(
  'buildings',
  116.30, 39.90, 116.42, 39.99,
  4326
);
```

如果你知道受影响区域，优先用它处理局部插入、更新、删除和移动。

## 9. 删除金字塔

```sql
SELECT ST_DropPyramid('buildings');
```

它会删除该金字塔的 catalog 行、瓦片存储、统计和遗留 shadow 状态。返回值表示是否删除了 catalog 行。

## 10. 配置参考

`config_json` 是 JSON 文档。传入 `'{}'` 使用默认值。决定每个参数怎么配时，优先看本节：它说明参数含义、什么时候调整、调大/调小的影响和注意事项。

### 10.1 快速参考

| Key | 默认值 | 主要用途 | 什么时候调整 |
|---|---:|---|---|
| `name` | 源表名 | 命名金字塔和存储表。 | 一个源表要构建多个金字塔，或需要稳定的对外瓦片名。 |
| `sourceSRS` | `4326` | 声明源 geometry 列的 SRID。 | 源数据不是 EPSG:4326。 |
| `tileSRS` | `3857` | 声明瓦片坐标系。 | 明确需要非 Web Mercator 瓦片。 |
| `tileSize` | `4096` | MVT 内部坐标 extent。 | 客户端或样式要求不同 extent。 |
| `tileExtend` | `8` | 瓦片边缘 buffer。 | 出现边界裁剪，或边缘重复/瓦片过大。 |
| `minZoom` | `0` | 最低物化 zoom。 | 不提供全球/粗 zoom。 |
| `maxZoom` | `18` | 最高物化 zoom。 | 需要更多细节、更小构建，或更严格服务范围。 |
| `attributes` | `[]` | 全局输出的 MVT 属性列。 | 客户端需要属性，或需要减小瓦片。 |
| `dynamicBeyondMaxZoom` | `true` | 超过 `maxZoom` 时实时切片。 | 需要严格只服务预构建瓦片，或需要灵活高 zoom 读取。 |
| `splitSize` | `0` | 稀疏瓦片延迟到动态切片。 | 想减少构建/存储，并能接受 miss 读取变慢。 |
| `parallel` | `1` | 构建 worker 数量。 | 构建慢且数据库主机有空余 CPU/I/O。 |
| `preparedBuild` | `false` | 切片前预转换源 geometry。 | 重复转换/简化成为主要构建成本。 |
| `featureMajorPoints` | `false` | 纯点源稀疏高 zoom 路径。 | 源是纯点，且高 zoom tile-major 空格探测很贵。 |
| `featureMajorMinZoom` | `13` | `featureMajorPoints` 启用门槛。 | 想让该路径更早或更晚启用。 |
| `featureDrivenEnum` | `false` | 全 geometry 稀疏高 zoom 路径。 | 源 bbox 覆盖大量空瓦片。 |
| `simplifyPixels` | `1.0` | geometry 简化容差。 | 瓦片太大或 geometry 细节过多。 |
| `minFeatureSizePixels` | `0.0` | 丢弃很小的线/面。 | 小要素让粗 zoom 杂乱或瓦片变大。 |
| `dropRate` | `1.0` | 按 zoom 保留点要素。 | 密集点图层在低 zoom 需要抽稀。 |
| `tileSizeBudgetBytes` | `0` | 单瓦片字节数保护。 | 必须限制瓦片 payload 大小。 |
| `maxTileSizeAttempts` | `5` | tile-size budget 重试次数。 | budget 控制太早停止或重试太久。 |
| `buildTimeoutMs` | `0` | 每个 zoom 的并行 worker 等待上限。 | 需要限制并行构建运行时间。 |
| `buildRules` | `[]` | 按 zoom 做 LOD 过滤、属性和采样。 | 不同 zoom band 需要不同数据密度/属性。 |

### 10.2 身份和坐标参数

#### `name`

- **含义：** 金字塔逻辑名称。SQL 调用如 `ST_Tile('buildings', z, x, y)` 会使用它，派生存储表如 `_gp_tiles_<name>` 也会使用它。
- **默认值：** 源表名。
- **什么时候改：** 对外瓦片图层名需要稳定、一个源表要构建多个金字塔，或希望名称比表名更短时。
- **如何调整：** 选择短的裸标识符，例如 `buildings_z14` 或 `roads_public`。避免 schema 前缀、标点、空格和很长的名称。
- **取舍/注意：** 重用已有名称会重建同名金字塔。当前标识符校验不支持 schema-qualified 名称。

#### `sourceSRS`

- **含义：** 源 geometry 列的 SRID。构建管线用它解释源 bbox，并把 geometry 转换到瓦片 SRS。
- **默认值：** `4326`。
- **什么时候改：** 源列真实 SRID 不是 4326 时，例如 `3857`、`4490` 或本地投影坐标系。
- **如何调整：** 不要把它当性能参数调；它必须匹配数据。源表混合 SRID 时，应先清洗/统一数据再构建。
- **取舍/注意：** 配错会导致瓦片位置错误或为空。

#### `tileSRS`

- **含义：** 瓦片坐标和 tile envelope 使用的 SRID。默认 Web Mercator 匹配标准 Web 地图瓦片。
- **默认值：** `3857`。
- **什么时候改：** 只有当客户端和瓦片方案明确使用其他坐标系时才改。
- **如何调整：** MapLibre、Web 地图和多数 XYZ 客户端保持 `3857`。
- **取舍/注意：** 一些加速路径要求 `tileSRS=3857`。非标准 tile SRS 可能无法直接用于常见 Web 客户端。

#### `tileSize`

- **含义：** MVT extent，即每个矢量瓦片内部使用的整数坐标网格。它不是 HTTP 瓦片像素尺寸。
- **默认值：** `4096`。
- **什么时候改：** 很少需要改。仅当 renderer/style 明确要求其他 extent 时调整。
- **如何调整：** 默认保留 `4096`。更大值保留更多坐标精度，但可能增大瓦片；更小值降低精度。
- **取舍/注意：** 必须大于 0 且不超过 `16384`。`tileExtend` 必须小于 `tileSize`。

#### `tileExtend`

- **含义：** 瓦片边缘 buffer，单位是 MVT extent，用于减少跨瓦片要素的边界裁剪。
- **默认值：** `8`。
- **什么时候改：** 线/面在瓦片边界被裁剪时调大；边缘重复或瓦片大小敏感时调小。
- **如何调整：** 先用默认值；如需调大，逐步小幅增加。
- **取舍/注意：** buffer 越大，瓦片边界外 geometry 越多，瓦片可能更大，边缘重复工作也更多。必须小于 `tileSize`。

### 10.3 Zoom 范围和输出属性

#### `minZoom`

- **含义：** 构建物化的最低 zoom。低于该 zoom 的请求返回 `NULL`。
- **默认值：** `0`。
- **什么时候改：** 不提供全球/粗 zoom，或低 zoom 对该数据没有视觉意义时调高。
- **如何调整：** 城市建筑类数据可从 `10` 或更高开始；国家/全球底图图层通常保持较低。
- **取舍/注意：** 调高可减少构建和存储，但客户端不能再期待低于该 zoom 的瓦片。

#### `maxZoom`

- **含义：** 构建物化的最高 zoom。在该范围内，瓦片主要从存储读取。
- **默认值：** `18`。
- **什么时候改：** 需要更多预构建细节时调高；需要减少构建时间/存储，或希望超过范围走实时切片时调低。
- **如何调整：** 选择应用会频繁从存储服务的最高 zoom。如果高 zoom 访问很少，用较低 `maxZoom` 加 `dynamicBeyondMaxZoom:true`；如果高 zoom 流量高，应物化这些 zoom。
- **取舍/注意：** 调高 `maxZoom` 可能显著增加瓦片格网规模和构建成本。最大为 `24`。

#### `attributes`

- **含义：** 全局输出为 MVT feature properties 的非 geometry 源列。
- **默认值：** `[]`，表示不输出额外属性。
- **什么时候改：** 客户端样式、标注、过滤、弹窗或业务逻辑需要字段时添加；不需要的字段应移除。
- **如何调整：** 保持最小集合。优先短文本/类别字段，避免大文本、大 JSON 或无用字段。粗 zoom 可用 `buildRules.attrFields` 输出更少属性。
- **取舍/注意：** 属性越多，瓦片越大，编码成本越高。字段名必须是合法源列/标识符。

### 10.4 读取/构建形态参数

#### `dynamicBeyondMaxZoom`

- **含义：** 控制 `ST_Tile(name, z, x, y)` 在 `z > maxZoom` 时的行为。true 表示从源表实时切片；false 表示返回 `NULL`。
- **默认值：** `true`。
- **什么时候改：** 生产服务需要行为可预测、只服务预构建瓦片时设为 false；允许用户偶尔超过 `maxZoom` 浏览时保持 true。
- **如何调整：** 高流量 API 建议 false 或加外部缓存；探索/内部地图保持 true 更方便。
- **取舍/注意：** 扩展内部不缓存动态切片，性能依赖源表索引。

#### `splitSize`

- **含义：** 稀疏存储阈值。`0` 表示物化所有非空瓦片。大于 `0` 时，要素数低于阈值的瓦片构建时不存储，后续请求实时切片。
- **默认值：** `0`。
- **什么时候改：** 构建产生大量低密度瓦片，且你愿意用较慢 miss 读取换取更小/更快构建时调高。
- **如何调整：** 从 `0` 开始。如果稀疏数据构建/存储太大，尝试 `2`、`4`、`8` 这类小值，并测量读取延迟。
- **取舍/注意：** 值越大，延迟瓦片越多，构建/存储越少，但实时读取负载越高。即使空区域 miss 也可能触发源表切片。请保持 GiST 索引，并为重复请求使用外部缓存。

#### `parallel`

- **含义：** 构建 worker 数量。大于 `1` 时通过 PostgreSQL background worker 分发构建；`1` 为串行；`0` 在配置处理中自动解析为受限的主机并行度。
- **默认值：** `1`。
- **什么时候改：** 构建 CPU-bound 或有足够独立瓦片工作，且数据库主机有空余资源时调高。
- **如何调整：** 先试 `2` 或 `4`，再根据目标数据 benchmark 决定是否继续增加。
- **取舍/注意：** worker 越多，CPU、I/O 和数据库竞争越高。少量重瓦片 workload 可能无收益。`tileSizeBudgetBytes` 不能与 `parallel > 1` 组合。

#### `buildTimeoutMs`

- **含义：** 每个 zoom 等待并行 worker 的上限，单位毫秒。`0` 表示不限时。
- **默认值：** `0`。
- **什么时候改：** 运维任务必须在超时后失败，而不是无限运行时设置。
- **如何调整：** 手工构建和 benchmark 通常保持 `0`。定时任务可按观测到的每 zoom 构建时间加安全余量设置。
- **取舍/注意：** 过低会在正常负载下中断合法构建。只在 `parallel > 1` 时相关。

### 10.5 构建提速路径参数

#### `preparedBuild`

- **含义：** 预转换并准备源 geometry，让后续瓦片切片复用准备好的数据。
- **默认值：** `false`。
- **什么时候改：** 重复坐标转换/简化占据主要构建成本，尤其是较大源表时启用。
- **如何调整：** 设置 `true`，然后比较 `ST_Pyramid` 构建统计或 benchmark 结果。它通常也是 feature-derived 稀疏路径的前置条件。
- **取舍/注意：** 准备阶段本身消耗时间和临时存储。对小型/集中数据可能收益中性或很小。

#### `featureMajorPoints`

- **含义：** 纯点源专用的稀疏高 zoom 构建路径。它从点坐标计算 tile id，而不是探测 bbox 中每个瓦片。
- **默认值：** `false`。
- **什么时候改：** 纯 `POINT` 数据集、高 `maxZoom`，且 tile-major 空格探测成为主要成本时启用。
- **如何调整：** 需要 `preparedBuild:true`、`parallel=1`、`tileSRS=3857`，且源为合格纯点。配合 `featureMajorMinZoom` 避免浅 zoom 回退。
- **取舍/注意：** 混合 geometry、线、面、非 Web Mercator、并行构建都不适用或会 fallback。

#### `featureMajorMinZoom`

- **含义：** `featureMajorPoints` 启用所需的最小 `maxZoom`。
- **默认值：** `13`。
- **什么时候改：** benchmark 显示浅 zoom 也受益时调低；浅层构建回退时调高。
- **如何调整：** 没有实测数据时保留默认。设为 `0` 表示只要其他条件满足就启用。
- **取舍/注意：** 这是启用门槛，不是要构建的 zoom 范围。它控制整次合格构建是否启用该路径。

#### `featureDrivenEnum`

- **含义：** 适用于所有 geometry 类型的稀疏/高 zoom 路径。它从 feature envelope 推导候选瓦片，而不是枚举 bbox 内每个瓦片格子。
- **默认值：** `false`。
- **什么时候改：** 稀疏数据、高 zoom，或大 bbox 中有大量空瓦片时启用。
- **如何调整：** 需要 `preparedBuild:true`、`parallel=1`、`tileSRS=3857`。它会在密集枚举更便宜时自我关闭，因此通常适合测试。
- **取舍/注意：** 准备和 gating 也有开销。密集范围或跨很多瓦片的大 geometry 可能收益不明显。

### 10.6 泛化和瓦片大小控制

#### `simplifyPixels`

- **含义：** 以瓦片像素为单位的 Douglas-Peucker 简化容差。值越大，geometry 简化越强。
- **默认值：** `1.0`。
- **什么时候改：** 瓦片太大或目标 zoom 不需要那么多细节时调高；形状失真过明显时调低。设为 `0` 关闭。
- **如何调整：** 可尝试 `0.5`、`1.0`、`2.0`，然后同时检查视觉效果和瓦片大小。
- **取舍/注意：** 更强简化会降低字节数/构建成本，但可能扭曲线和面。

#### `minFeatureSizePixels`

- **含义：** 丢弃小于指定瓦片像素尺寸的线/面。
- **默认值：** `0.0`，关闭。
- **什么时候改：** 低 zoom 小要素造成杂乱，或没有视觉价值却显著增加瓦片大小时调高。
- **如何调整：** 对粗 zoom 图层从小的非零值开始试，并对比视觉结果。业务关键小面应保持 `0`。
- **取舍/注意：** 被丢弃的要素不会出现在瓦片中；需要按 zoom 控制时使用 `buildRules`。

#### `dropRate`

- **含义：** tippecanoe 风格的点要素保留因子：保留比例近似为 `dropRate^(maxZoom - z)`。
- **默认值：** `1.0`，关闭。
- **什么时候改：** 密集点图层在低 zoom 需要抽稀时使用。
- **如何调整：** 小于 `1.0` 会在低 zoom 更强抽稀。例如 `0.4` 比 `0.8` 强得多。
- **取舍/注意：** 这是全局点抽稀旋钮。如果需要明确的按 zoom 或按 geometry 类型采样，优先使用 `buildRules.sample`。

#### `tileSizeBudgetBytes`

- **含义：** 单瓦片 MVT 字节上限。瓦片超过预算时，该 zoom 会用更强的简化/min-size/drop 设置重试。`0` 关闭。
- **默认值：** `0`。
- **什么时候改：** 客户端、网关或存储策略要求瓦片 payload 低于硬限制时设置。
- **如何调整：** 把它作为输出保护，而不是首选加速手段。具体值取决于应用；README 提到 `512000` 类似 tippecanoe 的起点。
- **取舍/注意：** budget retry 会显著增加构建时间，并可能降低视觉细节。不能与 `parallel > 1` 组合。

#### `maxTileSizeAttempts`

- **含义：** 执行 `tileSizeBudgetBytes` 时，每个 zoom 的最大重试轮数。
- **默认值：** `5`。
- **什么时候改：** budget 经常没达到就停止时调高；构建在重试上花太久时调低。
- **如何调整：** 没有 tile-size budget 日志/统计问题时保持默认。
- **取舍/注意：** 更多重试可能提高预算达成率，但会延长构建时间。如果重试后仍超限，应使用 LOD filter/sampling 或减少属性。

### 10.7 `buildRules` 与嵌套 LOD 参数

`buildRules` 是按 zoom 生效的规则数组，是表达层级细节（LOD）行为的首选方式。

每条规则可以包含：

- **`minZoom` / `maxZoom`**：规则生效的闭区间 zoom band。小范围用于精准调整，大范围用于粗/中/细层级策略。
- **`filter`**：针对源列求值的原始 SQL boolean 表达式。用于在粗 zoom 只保留重要要素，例如 `rank <= 3` 或 `importance >= 80`。更严格的 filter 会降低瓦片大小和构建工作，但这些 zoom 会缺少被过滤掉的要素。它是受信任管理员 SQL。
- **`attrFields`**：覆盖该 zoom band 输出的属性字段。低 zoom 用更少字段降低字节数，高 zoom 再输出更多字段。多条覆盖同一 zoom 的规则如果都设置 `attrFields`，必须一致。
- **`sample.rate`**：确定性采样百分比，范围 `0` 到 `100`。值越低抽稀越强。`100` 保留 filter 后所有要素；`0` 对被采样 geometry 集合不保留要素。
- **`sample.key`**：用于确定性 hash 的源列。默认 `["fid"]`。应使用稳定唯一 id，避免经常变化的字段。
- **`sample.geometryTypes`**：可选的 geometry family，例如 `["POINT"]`。当你只想抽稀密集点而不采样线/面时使用。

规则组合行为：

- 覆盖同一 zoom 的规则会按 AND 组合 filter 和 sampling。
- 未被任何规则覆盖的 zoom 使用全部源要素和全局 `attributes`。
- 动态读取使用与物化构建相同的 zoom-effective 规则，因此存储瓦片和动态瓦片应遵循一致的 LOD 语义。

示例：

```json
{
  "attributes": ["class", "name", "importance", "area"],
  "buildRules": [
    {"minZoom": 0, "maxZoom": 4, "filter": "importance >= 80", "attrFields": ["class", "name"]},
    {"minZoom": 5, "maxZoom": 7, "sample": {"rate": 50, "key": ["gid"]}},
    {"minZoom": 8, "maxZoom": 12, "attrFields": ["class", "name", "importance"]}
  ]
}
```

### 10.8 校验要点

- `minZoom` 必须小于等于 `maxZoom`。
- `maxZoom` 最大为 `24`。
- `tileSize` 必须为正且不超过 `16384`。
- `tileExtend` 必须小于 `tileSize`。
- 类似标识符的值必须是合法裸标识符。
- `buildRules.sample.rate` 必须在 `0` 到 `100` 之间。
- `tileSizeBudgetBytes` 当前不能与 `parallel > 1` 组合。

## 11. LOD 示例

使用 `buildRules` 按 zoom band 改变输出要素或属性。

### 过滤粗 zoom

```sql
SELECT ST_BuildPyramid(
  'places', 'geom', 'gid',
  '{
     "name": "places",
     "minZoom": 0,
     "maxZoom": 12,
     "attributes": ["name", "rank", "kind"],
     "buildRules": [
       {"minZoom": 0, "maxZoom": 5, "filter": "rank <= 3"}
     ]
   }'
);
```

### 低 zoom 输出更少属性

```json
{
  "minZoom": 0,
  "maxZoom": 6,
  "attrFields": ["name", "kind"]
}
```

### 确定性采样

```json
{
  "minZoom": 0,
  "maxZoom": 4,
  "sample": { "rate": 25, "key": ["gid"], "geometryTypes": ["POINT"] }
}
```

覆盖同一 zoom 的规则会按 AND 组合 filter 和 sampling。如果多条覆盖规则都设置了 `attrFields`，它们在该 zoom 必须一致。

安全提示：`buildRules.filter` 是以调用者权限执行的原始 SQL。应把它视为受信任管理员输入，类似 view definition。不要通过 `SECURITY DEFINER` wrapper 或向低权限用户暴露带 filter 的构建 API。

## 12. 常见配置模板

### 读取最快、服务行为可预测

使用默认存储行为：

```json
{
  "name": "buildings",
  "minZoom": 0,
  "maxZoom": 14,
  "splitSize": 0,
  "dynamicBeyondMaxZoom": false
}
```

这会物化 `maxZoom` 内所有非空瓦片，并在超过 `maxZoom` 时返回 `NULL`。

### 面向稀疏数据的小型构建

```json
{
  "name": "sparse_points",
  "minZoom": 0,
  "maxZoom": 14,
  "splitSize": 4,
  "dynamicBeyondMaxZoom": true
}
```

这可以减少构建和存储工作，但会把存储 miss 变成实时 PostGIS 切片。请保持源空间索引健康；如果是高流量重复读取，应增加外部缓存或避免该配置。

### 高 zoom 稀疏构建加速

```json
{
  "name": "roads_sparse",
  "minZoom": 0,
  "maxZoom": 14,
  "preparedBuild": true,
  "featureDrivenEnum": true
}
```

`featureDrivenEnum` 会自我判断是否启用，适合稀疏/高 zoom workload。对于纯点源，在满足条件时可考虑 `featureMajorPoints:true`。

### 按业务重要性稀释粗 zoom

```json
{
  "name": "lucc",
  "minZoom": 0,
  "maxZoom": 12,
  "attributes": ["class", "name", "importance"],
  "buildRules": [
    {"minZoom": 0, "maxZoom": 4, "filter": "importance >= 80", "attrFields": ["class", "name"]},
    {"minZoom": 5, "maxZoom": 7, "sample": {"rate": 50, "key": ["gid"]}}
  ]
}
```

## 13. 向客户端提供瓦片

`ST_Tile` 返回原始 MVT 字节。瓦片服务通常把 HTTP 路径映射到 SQL：

```text
GET /tiles/{name}/{z}/{x}/{y}.pbf
```

并执行：

```sql
SELECT ST_Tile($1, $2, $3, $4);
```

仓库中的 `demo/server.py` 提供了带 MapLibre 资源的本地验收示例。它是同步、单连接 demo 代码，不应作为生产服务基础设施。

生产环境中，建议在 PostgreSQL 前放置真正的 HTTP 服务和缓存，尤其在使用动态切片时。

## 14. 运维限制和关注点

- 名称被限制为较短裸标识符，以保证生成的 PostgreSQL 标识符安全。
- 当前不支持 schema-qualified 源表。
- 输出仅为 2D MVT/PBF。
- 扩展内部不缓存动态切片。
- 谓词刷新不会自动清理删除或移动要素的旧位置。
- 许多操作遇到未知金字塔时会返回 `NULL` 或 `false`，而不是抛出存储错误。
- `buildRules.filter` 和 `ST_UpdatePyramid(where_clause)` 接受受信任的原始 SQL 谓词。

## 15. 排障

| 现象 | 检查项 |
|---|---|
| `ST_Tile` 返回 `NULL` | 检查金字塔名、zoom/x/y 边界、`minZoom`/`maxZoom`、`dynamicBeyondMaxZoom`，以及瓦片是否确实为空。 |
| 构建很慢 | 确认源 geometry 有 GiST 索引，降低 `maxZoom`，使用 LOD `buildRules`，或评估 `preparedBuild` / `featureDrivenEnum`。 |
| 重复读取很慢 | 检查是否因 `splitSize > 0` 或 `dynamicBeyondMaxZoom` 触发实时切片。增加外部缓存或物化更多瓦片。 |
| 删除或移动后有旧瓦片 | 对旧受影响区域使用 `ST_UpdatePyramidByExtent`，或执行完整重建。 |
| 并行构建与 tile budget 冲突 | `tileSizeBudgetBytes` 与 `parallel > 1` 会被拒绝；改用串行预算控制或关闭字节预算。 |
| Filter 在构建前报错 | 根据源表校验 `buildRules.filter`，避免分号或不存在的列引用。 |

## 16. 延伸阅读

- [快速开始](quickstart.zh-CN.md)
- [金字塔生命周期工作流](workflows/pyramid-lifecycle.zh-CN.md)
- [配置与 LOD 领域概念](domain/config-and-lod.zh-CN.md)
- [运维、测试与 Benchmark](operations/testing-and-benchmarks.zh-CN.md)
- [源码地图](source-map.zh-CN.md)
