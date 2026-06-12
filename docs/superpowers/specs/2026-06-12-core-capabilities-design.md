# geometry_pyramid 核心能力设计:统计、原子重建、并行构建、增量更新

日期:2026-06-12
状态:已与用户逐节确认

## 背景与目标

当前扩展已有可用的单线程全量构建路径(`ST_BuildPyramid` → PostGIS MVT 编码 → `_gp_tiles_<name>` 存储,`ST_Tile` 读取)。本设计补齐四项核心能力,按依赖序分四个阶段交付(方案 A):

1. **构建统计** — 每 zoom 行数/字节/耗时落库,`ST_Pyramid()` 暴露。
2. **原子重建** — 影子表 + 事务内 swap,消除重建期间 `ST_Tile` 读失败窗口。
3. **并行构建** — 动态 bgworker 池,`parallel` 配置生效。
4. **增量更新** — `ST_UpdatePyramid(name, where_clause)` 只重建脏瓦片。

约束:

- SQL 表面保持 Ganos 兼容的**超集**:已有函数签名不变,允许新增配置键与 `ST_Pyramid()` 输出字段。
- 排序依据:bgworker 各自独立提交事务,破坏现有"单事务全量构建"的隐式原子性,因此**影子表(阶段 2)是并行(阶段 3)的前置依赖**。
- `parallel=1` 时必须走与现状等价的路径,零行为变化。

## 公共重构

**瓦片矩形抽象**:新增 `TileRect { z, xmin, ymin, xmax, ymax }`(`src/tile_rect.rs`,纯函数模块:bbox→矩形换算、矩形条带切分)。`build_zoom_level` 显式接受 `TileRect`;全量、增量、并行三个消费方共用同一段 INSERT SQL。

**写入目标与统计解耦**:`build_zoom_level` 接受目标表名参数(主表或影子表),返回 `ZoomBuildResult { tiles, bytes, duration }`。

模块布局:

```
src/
├── build.rs       # 改造:单矩形构建 + 预算重试循环
├── tile_rect.rs   # 新增:TileRect、bbox→矩形、条带切分
├── stats.rs       # 新增:统计表写入/读取
├── shadow.rs      # 新增:影子表 create/swap/abandon
├── worker.rs      # 新增:bgworker 入口 + 任务认领循环
├── incremental.rs # 新增:where_clause → 脏矩形 → 原地重建
```

## 阶段 1:构建统计

`sql/bootstrap.sql` 新增:

```sql
CREATE TABLE geometry_pyramid_stats (
    pyramid_name   text     NOT NULL,
    z              smallint NOT NULL,
    tile_count     bigint   NOT NULL,
    total_bytes    bigint   NOT NULL,
    max_tile_bytes bigint   NOT NULL,
    build_ms       bigint   NOT NULL,
    attempts       integer  NOT NULL DEFAULT 1,
    built_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (pyramid_name, z)
);
```

- 每 zoom 构建完成后 upsert;`attempts` 记录预算重试轮数(现状只在 notice 日志中)。
- `ST_Pyramid()` 输出新增 `"stats": [{z, tileCount, totalBytes, maxTileBytes, buildMs, attempts}, ...]`。
- `ST_DropPyramid` 同步删除统计行。

## 阶段 2:原子重建

`shadow.rs` 三个操作:

- `create(name)` — 建 `_gp_tiles_<name>__shadow`,结构同主表。`validate_identifier` 的长度校验按"最长派生名"(主表前缀 + name + `__shadow` 后缀)收紧。
- `swap(name)` — 当前事务内 `DROP TABLE IF EXISTS 主表; ALTER TABLE 影子表 RENAME TO 主表`。ACCESS EXCLUSIVE 锁窗口仅覆盖两条 DDL(毫秒级)。
- `abandon(name)` — `DROP TABLE IF EXISTS 影子表`,失败清理。

`build()` 全量路径(含首次构建)统一写影子表、结尾 swap;`ST_Tile` 不变,始终读主表。

错误处理:构建任何 `Err` 传播前调用 `abandon`;残留影子表由下次 `create` 的 `DROP IF EXISTS` 兜底。并发 `ST_BuildPyramid` 同名冲突用 `pg_advisory_xact_lock(hashtext('gp:' || name))` 串行化。

## 阶段 3:并行构建

### 任务表

```sql
CREATE UNLOGGED TABLE geometry_pyramid_jobs (
    job_id       bigserial PRIMARY KEY,
    pyramid_name text     NOT NULL,
    z            smallint NOT NULL,
    xmin_t integer NOT NULL, ymin_t integer NOT NULL,
    xmax_t integer NOT NULL, ymax_t integer NOT NULL,
    -- 当轮 knobs 快照(预算重试每轮可能收紧)
    simplify_tol_m float8 NOT NULL,
    min_area_m2    float8 NOT NULL,
    min_len_m      float8 NOT NULL,
    drop_threshold float8 NOT NULL,
    status       text NOT NULL DEFAULT 'pending',  -- pending|running|done|error
    error        text,
    claimed_by   integer
);
```

unlogged:任务是临时数据,崩溃后由协调者重发,省 WAL。

### 协调流程(按 zoom 串行、zoom 内并行)

预算重试以 zoom 为单位(所有切片完成后才能检查 `MAX(octet_length)`),故 zoom 间保持串行:

1. 该 zoom 的瓦片矩形按 x 切成 `4×K` 个条带(4 倍粒度做负载均衡;条带数不足则退化)。
2. `K = min(cfg.parallel, 可用 worker 余量) - 1`;`K≤0` 或 `parallel=1` 时**完全跳过任务表**,协调者自己跑(与现状等价)。
3. 写任务行 → `RegisterDynamicBackgroundWorker` 拉起 K 个 worker;协调者自身也消费任务(共 K+1 个消费者)。
4. 协调者消费完后轮询任务表至全部 `done`/`error`;新配置键 `buildTimeoutMs`(默认 0 = 不限)控制等待上限。
5. 任一 `error` → 等其余任务结束 → `abandon` 影子表 → 错误上抛。
6. zoom 完成后协调者做预算检查与统计写入;重试时清空该 zoom 影子表数据并以收紧后的 knobs 重发任务。

### worker(`worker.rs`)

- `main_arg` 仅携带 database OID(bgworker 只能传一个 Datum);worker 连库后自主认领任务。
- 认领:`UPDATE ... SET status='running', claimed_by=pg_backend_pid() WHERE job_id = (SELECT ... WHERE status='pending' LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING ...`;无任务即退出。
- 每任务一个事务:认领 + `build_zoom_level(影子表, TileRect, knobs)` + 标记 `done` 同事务提交;worker 崩溃则回滚回 `pending`,可被重新认领。

### 退化语义

- worker 注册失败(`max_worker_processes` 耗尽)→ notice 提示并降级为协调者全跑,**构建照常完成**。并行是尽力而为,不是正确性依赖。
- 影子表保证:无论 worker 提交了多少切片,主表在 swap 前不受影响。

## 阶段 4:增量更新

`ST_UpdatePyramid(name, where_clause)`(`incremental.rs`):

1. `where_clause` 为空 → 全量重建(影子表路径),维持现状语义。
2. 非空:
   - `SELECT ST_Extent(ST_Transform(geom, tile_srs)) FROM <source_table> WHERE (<where_clause>)`;
   - 结果 NULL → no-op 返回 true,notice 提示;
   - 否则对每个 `z ∈ [minZoom, maxZoom]`:`bbox_to_tile_range` 算脏矩形 → `DELETE` 主表中该矩形瓦片 → `build_zoom_level(主表, 脏矩形, knobs)` 原地重建 → 更新该 zoom 统计(`build_ms` 记本次增量耗时,`attempts` 不变)。
3. 全程单事务,失败回滚,主表无损,无需影子表。

语义说明(写入 README):重建的是"脏矩形内的完整瓦片";纯删除场景调用者需提供能覆盖被删区域的 where_clause(与 Ganos 信任模型一致)。

安全性:`where_clause` 是用户 SQL,信任模型同 Ganos(调用者持有源表权限);防御措施:以 `(...)` 包裹拼接,且先用 `SELECT 1 FROM <table> WHERE (<clause>) LIMIT 0` 试执行,语法错误得到干净报错。

预算重试在增量路径**不启用**(局部 MAX 字节检查会被未触及旧瓦片干扰),沿用全量构建 knobs。

## 测试策略

| 阶段 | 单元测试(`#[test]`) | 集成测试(`#[pg_test]`,需 PostGIS) |
|---|---|---|
| 统计 | — | 构建后 stats 行数 = zoom 数;`ST_Pyramid` 含 stats;Drop 清理 |
| 原子重建 | 派生表名长度校验 | 重建后旧数据被替换;构建失败后主表完好、无影子残留 |
| 并行 | TileRect 条带切分:数量、覆盖完整、互不重叠 | `parallel=4` 与 `parallel=1` 瓦片集逐字节一致;worker 不足时降级完成 |
| 增量 | — | 改 1 行→仅脏瓦片 mvt 变化;空匹配 no-op;坏 where_clause 报错且主表无损 |

逐字节一致可行:点丢弃用 `hashtextextended(fid)` 确定性哈希,管线无随机性。

## 错误处理总原则

- 任何失败路径下主表与 catalog 保持一致可用。
- 错误消息携带 pyramid 名与 zoom 上下文。
- 沿用现有 `Result<_, Box<dyn Error>>` + `notice!` 风格。

## 新增 SQL 可见面(Ganos 超集)

| 项 | 类型 | 说明 |
|---|---|---|
| `geometry_pyramid_stats` 表 | 新增表 | 每 zoom 构建统计 |
| `geometry_pyramid_jobs` 表 | 新增表(unlogged) | 并行任务队列 |
| `ST_Pyramid().stats` | 新增输出字段 | 统计数组 |
| `buildTimeoutMs` | 新增配置键 | 并行等待上限,默认 0(不限) |
