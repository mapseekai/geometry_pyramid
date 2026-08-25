# geometry_pyramid OpenWiki 快速开始（中文）

## 这个仓库是什么

`geometry_pyramid` 是一个 Rust/pgrx PostgreSQL 扩展，用于在 PostGIS geometry 表上构建并读取矢量瓦片金字塔。它的 SQL 使用界面参考阿里云 GanosBase `ganos_geometry_pyramid`：用 `ST_BuildPyramid` 注册源表并物化 MVT 瓦片，用 `ST_Tile` 读取已存储或动态切片的瓦片。几何裁剪和 MVT 编码由 PostGIS 的 `ST_TileEnvelope`、`ST_AsMVTGeom`、`ST_AsMVT` 完成，而不是在 Rust 中实现瓦片编码器。

主要证据：`README.md`、`README.zh-CN.md`、`Cargo.toml`、`src/lib.rs`、`src/pyramid.rs`、`src/build.rs`、`src/tile.rs`、`sql/bootstrap.sql`。

## 当前实现概览

- SQL 生命周期：`ST_BuildPyramid`、`ST_UpdatePyramid`、`ST_UpdatePyramidByExtent`、`ST_DropPyramid`、`ST_Pyramid`、`ST_Tile`、`ST_Pyramids`、`geometry_pyramid_version`（`README.md`、`README.zh-CN.md`、`src/pyramid.rs`、`src/tile.rs`、`sql/bootstrap.sql`）。
- 存储模型：每个金字塔写入 `_gp_tiles_<name>` 瓦片表，同时维护 `geometry_pyramid_catalog` 和逐 zoom 的 `geometry_pyramid_stats`（`sql/bootstrap.sql`、`src/storage.rs`、`src/stats.rs`）。
- 原子完整重建：完整构建先写入 shadow 表，再切换到正式表；读者不应看到半成品（`src/build.rs`、`src/shadow.rs`）。
- 混合读路径：`ST_Tile` 在 `maxZoom` 内优先读物化瓦片；`splitSize` 导致的稀疏 miss 和超过 `maxZoom` 且 `dynamicBeyondMaxZoom=true` 的请求会动态切片（`src/tile.rs`、`src/dynamic.rs`）。
- 性能路径：基线 tile-major、`preparedBuild`、纯点 `featureMajorPoints`、全几何 `featureDrivenEnum`、以及 PostgreSQL background worker 并行构建（`src/build.rs`、`src/prepared.rs`、`src/worker.rs`、`docs/build-speed-deep-dive.md`）。
- LOD/build rules：项目原生 `buildRules` 支持按 zoom 的 `filter`、`attrFields` 和确定性 `sample`；重叠规则按 AND 组合（`src/config.rs`、`src/build.rs`、`README.md`、`README.zh-CN.md`）。

## 工程师从这里开始

### 本地构建与测试

```bash
cargo check
cargo pgrx run pg17
cargo pgrx test
cargo pgrx package
```

`Cargo.toml` 默认使用 PostgreSQL 17 feature，并依赖 pgrx `0.18.0`。扩展运行需要 PostGIS；README 中的安装说明还覆盖 PostgreSQL 13-18 feature 与 `cargo-pgrx` 初始化。

### 最小 SQL 生命周期

```sql
CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;

SELECT ST_BuildPyramid(
  'buildings', 'geom', 'gid',
  '{"name":"buildings","minZoom":0,"maxZoom":14,"attributes":["name"]}'
);

SELECT ST_Tile('buildings', 14, 13488, 6210);
SELECT ST_Pyramid('buildings');
SELECT * FROM ST_Pyramids();
SELECT ST_UpdatePyramidByExtent('buildings', 116.30, 39.90, 116.42, 39.99, 4326);
SELECT ST_DropPyramid('buildings');
```

标识符输入被限制为短的裸标识符，因为多条路径会把验证后的标识符插入动态 SQL（`src/pyramid.rs`、`src/config.rs`）。当前不支持 schema-qualified 源表名。

## 文档地图

- [English quickstart](quickstart.md) — OpenWiki 英文入口和完整导航。
- [标准使用文档](usage.zh-CN.md) / [English](usage.md) — 启用扩展、准备数据、构建、读取、刷新、配置和排障的面向用户指南。
- [架构概览](architecture/overview.zh-CN.md) / [English](architecture/overview.md) — 模块职责、SQL 入口、构建/读取管线、存储和并发模型。
- [金字塔生命周期工作流](workflows/pyramid-lifecycle.zh-CN.md) / [English](workflows/pyramid-lifecycle.md) — 构建、取瓦片、更新、删除和 demo 流程及注意事项。
- [配置与 LOD 领域概念](domain/config-and-lod.zh-CN.md) / [English](domain/config-and-lod.md) — `PyramidConfig`、`buildRules`、采样、动态瓦片和性能旋钮。
- [运维、测试与 Benchmark](operations/testing-and-benchmarks.zh-CN.md) / [English](operations/testing-and-benchmarks.md) — pgrx 命令、demo server、benchmark SQL、用户数据结果和变更检查。
- [源码地图](source-map.zh-CN.md) / [English](source-map.md) — 给未来 agent/工程师使用的文件导航。

## 最近仓库方向

当前记录的上次成功更新 git head 与 `HEAD` 相同；本次检查到的是未提交工作树内容。重点变化包括：新增 `README.zh-CN.md`，demo viewer 增加默认关闭的在线 OSM 底图开关，demo server 增加连接初始化与客户端断开保护，`docs/user-data-build-performance.md` 汇总用户数据 benchmark，以及 worker 代码把 background-worker 文件从“spec”命名整理为“job”命名。现有英文 wiki 已覆盖这些行为；本页补齐中文入口。

## 安全与运维要点

- `buildRules.filter` 和 `ST_UpdatePyramid(where_clause)` 都接受原始 SQL 谓词。它们按受信任管理员输入处理，并以调用者权限执行；不要通过 `SECURITY DEFINER` 包装暴露给低权限角色（`README.md`、`README.zh-CN.md`、`src/pyramid.rs`、`src/config.rs`）。
- `buildRules.filter` 的分号检查只是防御性约束，不是完整 SQL 沙箱。构建前会探测非法谓词，避免留下部分构建结果（`src/config.rs`、`src/build.rs`）。
- 动态 SQL 标识符必须继续经过校验（`src/pyramid.rs`、`src/config.rs`、`src/dynamic.rs`）。
- `splitSize > 0` 和超过 `maxZoom` 的动态切片不做缓存；重复读取会变成实时 PostGIS 负载，应确保源表有 GiST 索引（`src/tile.rs`、`src/dynamic.rs`）。
- 并行构建使用 `$PGDATA/gp_tmp/` 下的 job/status 文件，因为 background worker 看不到 coordinator 未提交的状态（`src/worker.rs`）。

## Backlog

详见英文入口 [quickstart.md](quickstart.md) 的 `## Backlog`。当前未实现或兼容性缺口包括 coalesce/merge LOD、Ganos 兼容别名/签名、PNG 与 3D 输出、生产级瓦片服务/缓存、schema-qualified 源表名。
