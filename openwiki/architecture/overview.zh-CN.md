# 架构概览

## 运行时形态

`geometry_pyramid` 是一个通过 pgrx 加载的 PostgreSQL 扩展。`src/lib.rs` 声明 `pg_module_magic!`，连接各个模块文件，通过 `extension_sql_file!` 引入 `sql/bootstrap.sql`，并暴露 `geometry_pyramid_version()`。大多数面向用户的 SQL 函数由 `src/pyramid.rs` 和 `src/tile.rs` 中的 `#[pg_extern]` 函数实现。

这个扩展有意作为 PostGIS SQL 的协调层：

1. pgrx 把 Rust 函数暴露为 SQL 函数。
2. Rust 校验标识符和配置，并生成 SQL 片段。
3. SPI 执行 PostGIS 查询，完成 bbox、坐标转换、简化、裁剪、MVT 编码、存储读取和统计。
4. PostgreSQL 表存储 catalog 元数据、逐 zoom 统计和每个金字塔的 MVT 字节。

核心源码锚点：

- `src/lib.rs` — 模块连接、SQL bootstrap、pg 测试。
- `src/pyramid.rs` — 生命周期 SQL 函数和 catalog 查找。
- `src/build.rs` — 完整构建管线和 zoom 构建编排。
- `src/tile.rs` / `src/dynamic.rs` — 瓦片读取和实时切片路径。
- `src/config.rs` — JSON 配置 schema 与校验。
- `sql/bootstrap.sql` — catalog/stats DDL 和 `ST_Pyramids()`。

## 持久化数据模型

`sql/bootstrap.sql` 创建两个扩展共享表：

- `geometry_pyramid_catalog(pyramid_name, source_table, geom_column, fid_column, config, built_at)` 记录已注册金字塔。
- `geometry_pyramid_stats(pyramid_name, z, tile_count, total_bytes, max_tile_bytes, build_ms, attempts, built_at)` 保存逐 zoom 构建统计。

`src/storage.rs` 创建和删除按金字塔命名的瓦片表 `_gp_tiles_<name>`。表包含 `id bigserial, z smallint, x integer, y integer, format text, srid integer, mvt bytea` 列，主键为 `(z, x, y)`。`ST_Tile` 只返回 `mvt` 列。

完整重建在一个事务中删除并替换正式瓦片表的行，保留表的 OID、授权和依赖视图。并行构建使用 `_gp_shadow_<name>` 作为 worker 可见的暂存表，随后将瓦片行复制进正式表。事务失败时，先前已提交的瓦片行仍对读者可见（`src/build.rs`）。

## 公共 SQL 表面

由 `src/pyramid.rs`、`src/tile.rs`、`src/lib.rs` 和 `sql/bootstrap.sql` 实现：

| 函数 | 实现锚点 | 说明 |
|---|---|---|
| `ST_BuildPyramid(table, geom, fid, config)` | `src/pyramid.rs` | 解析并校验 `PyramidConfig`，upsert catalog 行，调用 `build::build`。 |
| `ST_UpdatePyramid(name, where_clause)` | `src/pyramid.rs`, `src/incremental.rs` | 空条件 = 完整重建；非空条件 = 根据匹配行计算 dirty extent。 |
| `ST_UpdatePyramidByExtent(name, xmin, ymin, xmax, ymax, srid)` | `src/pyramid.rs`, `src/incremental.rs` | 根据 bbox 从当前源数据刷新。 |
| `ST_DropPyramid(name)` | `src/pyramid.rs`, `src/storage.rs`, `src/stats.rs`, `src/shadow.rs` | 删除 catalog/stat/storage/shadow 状态。 |
| `ST_Pyramid(name)` | `src/pyramid.rs` | 返回元数据和逐 zoom 统计 JSON。 |
| `ST_Tile(name, z, x, y)` | `src/tile.rs`, `src/dynamic.rs` | 已存储瓦片查找和动态 fallback。 |
| `ST_Pyramids()` | `sql/bootstrap.sql` | PL/pgSQL catalog 列表，带瓦片计数。 |
| `geometry_pyramid_version()` | `src/lib.rs` | 返回 Cargo package 版本。 |

对于未知金字塔，多个操作会有意返回 `false` 或 `NULL`，而不是向用户抛出存储错误（`README.md`、`src/pyramid.rs`、`src/tile.rs`）。

## 构建管线

`build::build(src)` 是完整构建入口（`src/build.rs`）。主要步骤：

1. 获取每个金字塔的 advisory transaction lock，串行化并发构建者。
2. 拒绝不支持的 `tileSizeBudgetBytes` + `parallel > 1` 组合，因为 budget retry 会擦除/重建 zoom，与 worker 已提交 strip 不安全。
3. 在调用者事务中删除正式瓦片行。并行构建由 autonomous worker 创建 worker 可见的暂存表。
4. 在大量工作前校验 `buildRules` filters。
5. 在 tile SRS 中计算源 bbox。
6. 可选通过 `src/prepared.rs` 准备预转换源表（`preparedBuild`）。
7. 判断本次构建是否可启用 `featureMajorPoints` 或 `featureDrivenEnum`。
8. 对每个 zoom 推导瓦片矩形并调用 zoom 构建路径，可选进行 budget retry。
9. upsert 逐 zoom 统计。并行构建将暂存行复制到正式表，并在提交前清理暂存表。

瓦片 SQL 管线先在源 SRS 中按 bbox 预过滤，再转换到 tile SRS、按 zoom 简化和过滤要素、确定性抽稀/采样，调用 `ST_AsMVTGeom` 与 `ST_AsMVT`，最后根据 `splitSize` 存储非空瓦片（`src/build.rs`）。

## 构建路径变体

构建系统包含若干保持行为一致的路径，由配置和数据形态选择：

- **基线 tile-major**：枚举源 bbox 中的瓦片单元，并逐瓦片探测源 geometry。
- **Prepared tile-major**：`preparedBuild:true` 先把源 geometry 转换/简化到 prepared 表，再对该表运行 tile-major（`src/prepared.rs`、`src/build.rs`）。
- **Feature-major points**：`featureMajorPoints:true` 仅用于纯点源，且要求 `preparedBuild`、`parallel=1`、`tileSRS=3857`、足够大的 `maxZoom`；用算术方式计算 tile id，避免探测空格子。
- **Feature-driven enumeration**：`featureDrivenEnum:true` 把候选瓦片推导推广到所有 geometry envelope，并在收益不足时自动关闭。
- **并行构建**：`parallel > 1` 通过 `$PGDATA/gp_tmp/` 下的文件把 tile strip 分发给 PostgreSQL background worker（`src/worker.rs`）。`parallel:0` 在 `src/config.rs` 中自动解析为受限的主机并行度。

`docs/build-speed-deep-dive.md` 解释了这些路径存在的原因：tile-major 成本随 bbox 瓦片格子 `R(z)` 增长，而 feature-derived 路径在稀疏/高 zoom 范围胜出。

## 瓦片读取管线

`src/tile.rs` 中的 `ST_Tile(name, z, x, y)` 以 catalog 为先：

1. 校验金字塔名称和瓦片坐标边界。
2. 通过 `pyramid::catalog_meta` 一次性加载 catalog 元数据和存储存在性。
3. 低于 `minZoom` 或 x/y 无效时返回 `NULL`。
4. 在 `maxZoom` 范围内，若存在则读取 `_gp_tiles_<name>`。
5. 如果存储读取 miss 且 `splitSize == 0`，返回 `NULL`，因为所有非空瓦片都应已物化。
6. 如果存储读取 miss 且 `splitSize > 0`，调用 `dynamic::generate_tile`，因为稀疏瓦片可能在构建时被延后。
7. 高于 `maxZoom` 时，只有 `dynamicBeyondMaxZoom` 为 true 才调用 `dynamic::generate_tile`。

`src/dynamic.rs` 使用与完整构建相同的 `build::tile_pipeline_from_sql` 机制，包括 zoom-effective `buildRules`，因此物化瓦片和动态瓦片应具有一致的 LOD 语义。动态瓦片只读且不缓存。

## 并发和事务边界

重要设计选择：

- 完整串行构建在一个事务中替换正式表的瓦片行。
- 并行 worker 是独立 backend，因此 job spec 和完成信号通过 `$PGDATA/gp_tmp/` 文件传递，而不是未提交 catalog 行（`src/worker.rs`）。
- 并行暂存表和已提交 prepared 表由 autonomous worker 创建，因为 worker 看不到 coordinator 未提交的对象（`src/build.rs`、`src/prepared.rs`）。
- 已提交 prepared 表由 coordinator 事务删除，而不是 autonomous 删除，以避免 `src/prepared.rs` 中描述的 coordinator/worker 锁等待死锁。
- 增量刷新在调用者事务中运行，替换受影响的瓦片行（`src/incremental.rs`）。

## 架构上的有意限制

- 源表、geometry 列、fid 列和金字塔名称都是经过校验的裸标识符；不支持 schema-qualified 源表。
- 动态瓦片切片不缓存。
- SQL API 受 Ganos 启发，但并不完全兼容 Ganos：当前瓦片签名为 `ST_Tile(name, z, x, y)`，不是文档中的 Ganos `x,y,z` 或 key overload。
- 输出格式仅为 2D MVT。
