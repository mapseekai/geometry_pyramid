# 源码地图

在打开源码文件前，可用本页快速导航。行为细节优先阅读 `quickstart.md` 中链接的专题页面。

## 根目录元数据和文档

- `README.md` — 主要面向用户的 SQL 表面、配置表、生命周期示例、生成规则、构建命令和 roadmap。
- `README.zh-CN.md` — 与英文 README 并列的中文 README。
- `Cargo.toml` — crate 元数据、pgrx 版本、PostgreSQL feature flags、release profile。
- `geometry_pyramid.control` — PostgreSQL 扩展 control 元数据和 PostGIS 需求。
- `sql/bootstrap.sql` — 扩展自有 catalog/stats DDL 和 `ST_Pyramids()`。
- `docs/build-speed-deep-dive.md` — prepared/feature-driven 路径的性能动机。
- `docs/user-data-build-performance.md` — 供应用户数据集的 benchmark 汇总。
- `docs/ganos-gap-analysis.md` — 历史 Ganos 差距分析；有用的上下文，但其标题提醒部分状态行已过时。

## Rust 扩展模块

- `src/lib.rs` — pgrx 模块连接、`geometry_pyramid_version()` 和多数 integration/pg 测试。
- `src/config.rs` — `PyramidConfig`、默认值、校验、`buildRules` 和 `parallel:0` 自动解析。
- `src/pyramid.rs` — 公共生命周期函数：build、update、bbox update、drop、inspect、catalog lookup、identifier validation。
- `src/source.rs` — 在生命周期/构建/读取路径之间传递的 source metadata。
- `src/build.rs` — 完整构建 coordinator、PostGIS tile SQL pipeline、LOD clauses、zoom stats、budget retry 和构建变体 dispatch。
- `src/prepared.rs` — 用于串行和并行可见 fast path 的 prepared source tables。
- `src/worker.rs` — PostgreSQL background worker job 文件、autonomous SQL 执行和 parallel strip 执行。
- `src/tile.rs` — `ST_Tile` 查找和动态 fallback 决策树。
- `src/dynamic.rs` — 面向 `splitSize` miss 和 beyond-`maxZoom` 读取的非缓存按需单瓦片切片。
- `src/incremental.rs` — 谓词和 bbox dirty-region refresh。
- `src/storage.rs` — 每个金字塔 `_gp_tiles_<name>` 存储表生命周期。
- `src/shadow.rs` — 用于原子完整重建的 shadow table create/swap/drop。
- `src/stats.rs` — 逐 zoom 统计的持久化和读取。
- `src/tile_rect.rs` — tile rectangle 数学和分区辅助函数。

## Demo 和 benchmark

- `demo/server.py` — 围绕 `ST_Tile` 的本地 HTTP wrapper；不是生产服务代码。
- `demo/index.html` — MapLibre viewer，包含本地 vector tile layer 和可选择的在线底图。
- `demo/test_server.py` — parser、handler、static viewer 期望和连接保护行为的 Python 单元测试。
- `demo/seed.sql` 和 `demo/README.md` — 本地 demo 数据库设置。
- `bench/*.sql` — 手工性能脚本。合成基线从 `bench/build_speed.sql` 开始，当前用户数据矩阵从 `bench/user_data_perf.sql` 开始。

## 常见变更从哪里开始

- 新增或修改配置键：`src/config.rs` → `src/build.rs` / `src/prepared.rs` / `src/dynamic.rs` → README 配置表 → `src/lib.rs` 测试。
- 修改瓦片构建输出：先看 `src/build.rs`，然后确认 prepared、feature-major、feature-driven、dynamic 和 parallel 路径仍然一致。
- 修改瓦片读取：`src/tile.rs` 和 `src/dynamic.rs`；检查 split-size 和 beyond-maxZoom 行为。
- 修改更新语义：`src/pyramid.rs` 和 `src/incremental.rs`；检查 stale-delete/move caveats。
- 修改并行行为：`src/build.rs`、`src/prepared.rs`、`src/worker.rs`；注意事务可见性和 worker 已提交行。
- 修改 demo 行为：`demo/server.py`、`demo/index.html` 和 `demo/test_server.py`。
