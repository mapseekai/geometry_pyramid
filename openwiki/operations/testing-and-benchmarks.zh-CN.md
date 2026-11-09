# 开发、测试与基准

[English](testing-and-benchmarks.md) · [使用说明](../usage.zh-CN.md)

在仓库根目录执行以下命令。Cargo 默认启用 PostgreSQL 17 feature，pgrx 版本为
0.18.0。请指定 PostgreSQL 17 的 `pg_config`；`PATH` 上其他 PostgreSQL
版本的 `pg_config` 不能替代。

## 检查源码变更

```bash
cargo install --locked cargo-pgrx --version 0.18.0
cargo pgrx init --pg17 /path/to/postgresql-17/bin/pg_config
cargo check
cargo pgrx test pg17
python3 demo/test_server.py
```

`cargo pgrx test pg17` 运行 Rust 和 PostgreSQL 集成测试，包括使用 PostGIS 的
`#[pg_test]`。Python 测试不需要运行中的数据库，覆盖演示服务的路径解析和
HTTP 行为；运行前需安装 `psycopg`。

交互式 SQL 检查可运行 `cargo pgrx run pg17`：它会编译、安装扩展，启动受 pgrx
管理的 PostgreSQL，并打开 `psql`。目标数据库应先启用 PostGIS，再启用
`geometry_pyramid`。[README 示例](../../README.zh-CN.md#生成第一张瓦片)
可构建并检查一张瓦片。

## 构建安装包

```bash
cargo pgrx package --pg-config /path/to/postgresql-17/bin/pg_config --out-dir package
```

仓库的 [Verify and release 工作流](../../.github/workflows/release.yml) 在
Ubuntu 24.04 上测试 PostgreSQL 17、构建安装包，并验证全新安装和从 0.1.0
升级。Cargo 提供 PostgreSQL 13 至 18 的 feature；该工作流验证的是 PG17 安装包。

## 运行查看器

按照[演示操作说明](../../demo/README.zh-CN.md)在专用数据库中导入样例，启动 HTTP
服务，检查瓦片响应，再打开 MapLibre 查看器。本地服务使用单个同步数据库连接。
默认端口为 8989，可通过 `PORT` 修改。OSM 底图可选，默认关闭。

## 测量构建速度

`bench/build_speed.sql` 会创建和删除 `_gp_bench_mixed` 以及指定的金字塔，
因此只应在**可丢弃的基准数据库**中运行。把 `DATABASE_URL` 指向该数据库。
先用小规模检查环境，再在目标硬件上选择有代表性的工作量并重复测量：

```bash
psql "$DATABASE_URL" \
  -v pyramid_name=gp_bench_baseline \
  -v max_zoom=8 \
  -v scale=1000 \
  -v prepared_build=false \
  -f bench/build_speed.sql
```

脚本输出源几何分布、逐 zoom 瓦片格数量、总耗时和已保存的逐层统计。比较
prepared 路径时，换一个金字塔名并设置 `-v prepared_build=true`，其余输入
和环境保持一致。`bench/user_data_*.sql` 和[实测报告](../../docs/user-data-build-performance.md)
涵盖更多真实数据形态。某种数据的加速结果不能未经复测就推广到其他数据。

## 运维检查

- 全量重建在事务内替换正式瓦片表的行。读者不会看到部分结果；表的授权和依赖
  视图保持不变。
- 谓词刷新只看到当前匹配行。删除或移动时，应刷新覆盖旧、新位置的区域，或
  执行全量重建。
- 延迟生成或超过 `maxZoom` 的瓦片会实时切片，扩展内部没有缓存。按需监控
  源表查询负载并在服务层加入缓存。
- 并行构建使用 PostgreSQL background worker 和 `$PGDATA/gp_tmp/` 下的文件。
  `tileSizeBudgetBytes` 与 `parallel > 1` 的组合会被拒绝。

SQL 行为、配置、升级及按症状排障，请参阅[使用说明](../usage.zh-CN.md)。
