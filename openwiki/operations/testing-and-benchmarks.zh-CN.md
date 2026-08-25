# 运维、测试与 Benchmark

## 本地构建和 pgrx 检查

该 crate 目标是 pgrx `0.18.0`，并通过 Cargo features 默认使用 PostgreSQL 17（`Cargo.toml`）。README 安装说明覆盖 PostgreSQL 13-18 feature flags 和 `cargo-pgrx` 初始化。

常用命令：

```bash
cargo check
cargo pgrx run pg17
cargo pgrx test
cargo pgrx package
```

在实际使用 SQL API 前需要 PostGIS：

```sql
CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;
```

使用 `cargo pgrx test` 运行普通 Rust 测试和 `src/lib.rs` 中的 `#[pg_test]` 覆盖。近期测试覆盖了 buildRules 在动态读取、prepared 多规则输出、feature-major points、feature-driven builds、串行/并行等价性和非法 filter 清理中的行为。

## Demo server 和 viewer

`demo/` 是本地验收工具，不是生产基础设施。`demo/server.py` 提供：

- `/` 和来自 `demo/` 的静态 MapLibre 资源。
- `/tiles/{name}/{z}/{x}/{y}.pbf`，它调用 `SELECT ST_Tile(%s, %s, %s, %s)`，并对 SQL NULL 或空字节返回 HTTP 204。

该 server 是同步的，并维护一个模块级 psycopg 连接。当前代码会在 psycopg 错误后重连一次，在 URL parser 中拒绝非 identifier 瓦片名，捕获常见客户端断开写入错误，并要求 `_fetch_tile` 运行前连接已初始化（`demo/server.py`）。这些行为的测试位于 `demo/test_server.py`。

来自 `demo/README.md` 和 `workflows/pyramid-lifecycle.md` 的典型本地流程：

```bash
CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx install --release \
  --pg-config /opt/homebrew/opt/postgresql@17/bin/pg_config
CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx start pg17

createdb -h localhost -p 28817 geometry_pyramid
psql -h localhost -p 28817 -d geometry_pyramid -f demo/seed.sql

DATABASE_URL="host=localhost port=28817 dbname=geometry_pyramid" python3 demo/server.py
```

静态 viewer 现在在 `demo/index.html` 中包含一个可选择/可切换的在线 OpenStreetMap 底图；测试断言它默认不启用（`demo/test_server.py`）。

## Benchmark 脚本

手工 SQL benchmark 位于 `bench/`。它们用于对 pgrx/PostGIS 数据库运行 psql，并经常创建/删除 benchmark schema 或金字塔。

关键文件：

- `bench/build_speed.sql` — README 中描述的确定性混合点/线/面构建速度基线。
- `bench/user_data_perf.sql` 和 `bench/user_data_perf_followup.sql` — 面向点、建筑面、派生线、LUCC 和混合数据的用户数据导入/性能矩阵。
- `bench/user_data_lucc_optimization.sql`、`bench/user_data_lucc_sampling.sql`、`bench/user_data_mixed_optimization.sql`、`bench/user_data_next_optimization.sql`、`bench/user_data_point_sampling.sql`、`bench/user_data_split_z8.sql`、`bench/user_data_budget_followup.sql` 和 `bench/user_data_tile_profile_parallel.sql` — 后续优化和 profiling 场景。

`docs/user-data-build-performance.md` 汇总了已检查的用户数据运行结果。高信号结论：

- 看起来密集的源行数不是唯一驱动因素；即使输出瓦片很少，复杂 polygon 裁剪/MVT 编码也可能主导成本。
- `preparedBuild` 在测量过的集中数据集上表现为中性到略有帮助。
- `featureMajorPoints` 对浅层/集中 z0-10 点数据没有帮助，因为生成的瓦片集合很小。
- `buildRules.sample` 是测量集中最强的 LUCC 粗 zoom 优化手段。
- `tileSizeBudgetBytes` 更适合作为输出保护，而不是首选构建加速手段。
- `parallel=4` 没有改善已检查的少量重瓦片 workload；mixed LOD 在该 benchmark 中回退。

把这些结论作为起始假设，而不是通用性能保证。在推荐默认值前，应在目标数据和硬件上重新运行相关脚本。

## 运维关注点

- 完整重建通过 shadow table swap 保证原子性，但增量刷新会就地编辑 dirty region（`src/build.rs`、`src/shadow.rs`、`src/incremental.rs`）。
- `ST_UpdatePyramid(where_clause)` 无法清理 delete 或 move-out 的旧位置，除非该 clause 仍覆盖它们。此类场景优先使用 `ST_UpdatePyramidByExtent` 或完整重建。
- 动态切片不缓存；重复的 `splitSize` 或 beyond-`maxZoom` 读取可能变成实时 PostGIS workload（`src/dynamic.rs`）。
- 并行构建使用 background worker 和 `$PGDATA/gp_tmp/` 下的 job/status 文件，因为 worker 看不到 coordinator 未提交状态（`src/worker.rs`）。
- `tileSizeBudgetBytes` 与 `parallel > 1` 的组合会被设计性拒绝（`src/build.rs`）。

## 变更指导

- 对 SQL API 或构建管线变更，尽可能运行 `cargo pgrx test`，并在 `src/lib.rs` 中为用户可见行为添加 `#[pg_test]` 覆盖。
- 对 demo 变更，运行 `python3 demo/test_server.py`；这些测试不需要 live database 即可检查 parser/static/handler 行为。
- 对性能变更，优先新增聚焦的 `bench/*.sql` 脚本或扩展现有脚本，然后把结果汇总到 `docs/user-data-build-performance.md`，而不是写进 wiki。
