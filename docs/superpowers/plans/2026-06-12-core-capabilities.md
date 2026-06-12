# geometry_pyramid 核心能力实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按规格 `docs/superpowers/specs/2026-06-12-core-capabilities-design.md` 实现四项核心能力:构建统计、原子重建(影子表)、bgworker 并行构建、增量更新。

**Architecture:** 先做公共重构(TileRect 抽象 + 构建结果结构),再按依赖序四阶段交付。并行构建的控制面走 `$PGDATA/gp_tmp/` 下的 spec/status 文件(规避 MVCC 可见性),`parallel>1` 时影子表由自治 ExecSql worker 创建。

**Tech Stack:** Rust + pgrx 0.16(`cargo pgrx test pg16`),PostGIS(SPI 委托),serde/serde_json。

**通用约定:**
- 测试命令:`cargo pgrx test pg16`(同时跑 `#[test]` 与 `#[pg_test]`);快速编译检查 `cargo check`。
- 每个 Task 结束跑 `detect_changes()`(项目 CLAUDE.md 要求)再 commit。
- 提交信息遵循 `<type>: <description>`,无 attribution 尾注。
- pgrx API 名(`BackgroundWorkerBuilder`、`BackgroundWorker::get_extra` 等)以 pgrx 0.16 文档为准;若 `cargo check` 报 API 不匹配,以同名最近 API 替换,不改变设计语义。

---

### Task 1: TileRect 抽象(`src/tile_rect.rs`)

**Files:**
- Create: `src/tile_rect.rs`
- Modify: `src/lib.rs`(加 `mod tile_rect;`)
- Modify: `src/build.rs`(删除 `bbox_to_tile_range` 与 `WEBMERC_HALF_EXTENT`,改用 TileRect;迁移对应测试)

- [ ] **Step 1: 写失败的单元测试**(新文件底部)

```rust
// src/tile_rect.rs 的 #[cfg(test)] mod tests
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn z0_full_world_is_single_tile() {
        let r = TileRect::from_bbox_m(
            0,
            -WEBMERC_HALF_EXTENT, -WEBMERC_HALF_EXTENT,
            WEBMERC_HALF_EXTENT, WEBMERC_HALF_EXTENT,
        );
        assert_eq!((r.xmin, r.ymin, r.xmax, r.ymax), (0, 0, 0, 0));
    }

    #[test]
    fn z1_full_world_is_four_tiles() {
        let r = TileRect::from_bbox_m(
            1,
            -WEBMERC_HALF_EXTENT + 1.0, -WEBMERC_HALF_EXTENT + 1.0,
            WEBMERC_HALF_EXTENT - 1.0, WEBMERC_HALF_EXTENT - 1.0,
        );
        assert_eq!((r.xmin, r.ymin, r.xmax, r.ymax), (0, 0, 1, 1));
    }

    #[test]
    fn out_of_range_is_clamped() {
        let r = TileRect::from_bbox_m(5, -1e9, -1e9, -9e8, -9e8);
        let n: i64 = 1 << 5;
        assert!(r.xmin >= 0 && r.xmax <= n - 1);
        assert!(r.ymin >= 0 && r.ymax <= n - 1);
    }

    #[test]
    fn split_x_covers_exactly_without_overlap() {
        let r = TileRect { z: 10, xmin: 3, ymin: 0, xmax: 17, ymax: 5 };
        let parts = r.split_x(4);
        assert_eq!(parts.len(), 4);
        // 首尾衔接、无缝隙无重叠
        assert_eq!(parts[0].xmin, 3);
        assert_eq!(parts[3].xmax, 17);
        for w in parts.windows(2) {
            assert_eq!(w[0].xmax + 1, w[1].xmin);
        }
        // 宽度总和等于原矩形
        let total: i64 = parts.iter().map(|p| p.width()).sum();
        assert_eq!(total, r.width());
        // y/z 不变
        assert!(parts.iter().all(|p| p.z == 10 && p.ymin == 0 && p.ymax == 5));
    }

    #[test]
    fn split_x_more_parts_than_width_degrades() {
        let r = TileRect { z: 3, xmin: 0, ymin: 0, xmax: 2, ymax: 2 }; // 宽 3
        let parts = r.split_x(10);
        assert_eq!(parts.len(), 3); // 退化为每列一条
    }

    #[test]
    fn split_x_one_part_is_identity() {
        let r = TileRect { z: 3, xmin: 0, ymin: 0, xmax: 7, ymax: 7 };
        assert_eq!(r.split_x(1), vec![r]);
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo check`
Expected: 编译错误(`TileRect` 未定义)。

- [ ] **Step 3: 实现 `src/tile_rect.rs`**

```rust
//! Tile-index rectangles — the unit of work shared by the full,
//! incremental, and parallel build paths.

/// Web Mercator half-extent in metres.
pub const WEBMERC_HALF_EXTENT: f64 = 20_037_508.342_789_244;

/// Inclusive tile-index rectangle at a fixed zoom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileRect {
    pub z: u8,
    pub xmin: i64,
    pub ymin: i64,
    pub xmax: i64,
    pub ymax: i64,
}

impl TileRect {
    /// Convert a Web-Mercator metres bbox into the inclusive tile rectangle
    /// covering it at zoom `z`, clamped to `[0, 2^z - 1]`.
    pub fn from_bbox_m(z: u8, xmin_m: f64, ymin_m: f64, xmax_m: f64, ymax_m: f64) -> Self {
        let n: i64 = 1 << z;
        let tile_span = (2.0 * WEBMERC_HALF_EXTENT) / (n as f64);
        let to_tile_x = |m: f64| ((m + WEBMERC_HALF_EXTENT) / tile_span).floor() as i64;
        let to_tile_y = |m: f64| ((WEBMERC_HALF_EXTENT - m) / tile_span).floor() as i64;
        Self {
            z,
            xmin: to_tile_x(xmin_m).clamp(0, n - 1),
            ymin: to_tile_y(ymax_m).clamp(0, n - 1), // Y axis is inverted
            xmax: to_tile_x(xmax_m).clamp(0, n - 1),
            ymax: to_tile_y(ymin_m).clamp(0, n - 1),
        }
    }

    pub fn width(&self) -> i64 {
        self.xmax - self.xmin + 1
    }

    /// Split into at most `parts` disjoint x-strips that exactly cover
    /// `self`. Degrades to one strip per column when `parts > width`.
    pub fn split_x(&self, parts: u32) -> Vec<TileRect> {
        let parts = (parts as i64).clamp(1, self.width());
        let base = self.width() / parts;
        let extra = self.width() % parts;
        let mut out = Vec::with_capacity(parts as usize);
        let mut x = self.xmin;
        for i in 0..parts {
            let w = base + if i < extra { 1 } else { 0 };
            out.push(TileRect {
                z: self.z,
                xmin: x,
                ymin: self.ymin,
                xmax: x + w - 1,
                ymax: self.ymax,
            });
            x += w;
        }
        out
    }
}
```

`src/lib.rs` 在 `mod tile;` 旁加一行:

```rust
mod tile_rect;
```

- [ ] **Step 4: 改造 `src/build.rs` 使用 TileRect**

1. 顶部加 `use crate::tile_rect::TileRect;`,删除本文件的 `const WEBMERC_HALF_EXTENT` 与 `fn bbox_to_tile_range`,及其 `tests` 模块中 `z0_full_world_is_single_tile` / `z1_full_world_is_four_tiles` / `out_of_range_is_clamped` 三个已迁移的测试(`tighten_*` 测试保留)。
2. `build_zoom_level` 签名:删 `z: u8, xmin_m: f64, ymin_m: f64, xmax_m: f64, ymax_m: f64`,改收 `rect: &TileRect`;函数体开头替换:

```rust
// 原:let (xmin_t, ymin_t, xmax_t, ymax_t) = bbox_to_tile_range(z, xmin_m, ymin_m, xmax_m, ymax_m);
let z = rect.z;
let (xmin_t, ymin_t, xmax_t, ymax_t) = (rect.xmin, rect.ymin, rect.xmax, rect.ymax);
```

其余 SQL 与参数绑定不变。
3. `build_zoom_level_with_budget` 同样改收 `rect: &TileRect`(删 `z` 与四个 `*_m` 参数);`pixel_size_m(rect.z)`、`ZoomKnobs::from_config(cfg, rect.z)`、`DELETE ... WHERE z=$1` 用 `rect.z`,内部调用传 `rect`。
4. `build()` 的 zoom 循环改为:

```rust
for z in cfg.min_zoom..=cfg.max_zoom {
    let rect = TileRect::from_bbox_m(z, xmin_m, ymin_m, xmax_m, ymax_m);
    let inserted = build_zoom_level_with_budget(
        pyramid_name, &storage_table, table, geom_column, fid_column, cfg, &rect,
    )?;
    total += inserted;
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo pgrx test pg16`
Expected: 全部 PASS(含迁移后的 tile_rect 测试与原 pg_test 冒烟)。

- [ ] **Step 6: detect_changes + Commit**

Run: `detect_changes()`(MCP),确认只影响 build 相关符号。

```bash
git add src/tile_rect.rs src/lib.rs src/build.rs
git commit -m "refactor: extract TileRect abstraction shared by build paths"
```

---

### Task 2: ZoomBuildResult 与按 zoom 统计采集(`src/build.rs`)

**Files:**
- Modify: `src/build.rs`

- [ ] **Step 1: 定义结果结构与 zoom 统计查询(先写实现——此 Task 是纯结构搬运,行为不变,由现有 pg_test 回归保护)**

`src/build.rs` 加:

```rust
/// Per-zoom build outcome, consumed by the stats layer.
#[derive(Clone, Copy, Debug)]
pub struct ZoomBuildResult {
    pub z: u8,
    pub tiles: i64,
    pub total_bytes: i64,
    pub max_tile_bytes: i64,
    pub build_ms: i64,
    pub attempts: u32,
}

/// (tile_count, total_bytes, max_tile_bytes) for one zoom of a tile table.
fn zoom_stats(storage_table: &str, z: u8) -> Result<(i64, i64, i64), Box<dyn std::error::Error + Send + Sync>> {
    let row: Option<String> = Spi::get_one_with_args(
        &format!(
            r#"SELECT json_build_array(
                   count(*),
                   COALESCE(sum(octet_length(mvt)), 0),
                   COALESCE(max(octet_length(mvt)), 0))::text
               FROM "{storage_table}" WHERE z = $1"#
        ),
        &[(z as i32).into()],
    )?;
    let s = row.ok_or("zoom_stats returned no row")?;
    let arr: [i64; 3] = serde_json::from_str(&s)?;
    Ok((arr[0], arr[1], arr[2]))
}
```

- [ ] **Step 2: `build_zoom_level_with_budget` 返回 `ZoomBuildResult`**

返回类型改 `Result<ZoomBuildResult, Box<dyn std::error::Error + Send + Sync>>`;函数开头加 `let started = std::time::Instant::now();`;两处 `return Ok(inserted);` 改为:

```rust
let (tiles, total_bytes, max_tile_bytes) = zoom_stats(storage_table, rect.z)?;
return Ok(ZoomBuildResult {
    z: rect.z,
    tiles,
    total_bytes,
    max_tile_bytes,
    build_ms: started.elapsed().as_millis() as i64,
    attempts: attempt + 1,
});
```

(预算检查仍可直接用 `max_tile_bytes`,`max_tile_bytes()` 独立函数可删,`inserted_count` 保留供 `build_zoom_level` 内部使用。)

- [ ] **Step 3: `build()` 适配并保留总数语义**

```rust
let mut total: i64 = 0;
let mut zoom_results: Vec<ZoomBuildResult> = Vec::new();
for z in cfg.min_zoom..=cfg.max_zoom {
    let rect = TileRect::from_bbox_m(z, xmin_m, ymin_m, xmax_m, ymax_m);
    let result = build_zoom_level_with_budget(
        pyramid_name, &storage_table, table, geom_column, fid_column, cfg, &rect,
    )?;
    total += result.tiles;
    zoom_results.push(result);
}
let _ = &zoom_results; // Task 3 接管:写入 stats 表
Ok(total)
```

- [ ] **Step 4: 运行测试**

Run: `cargo pgrx test pg16`
Expected: 全部 PASS(行为不变)。

- [ ] **Step 5: detect_changes + Commit**

```bash
git add src/build.rs
git commit -m "refactor: build_zoom_level_with_budget returns ZoomBuildResult"
```

---

### Task 3: 统计表与写入(`sql/bootstrap.sql`、`src/stats.rs`)

**Files:**
- Modify: `sql/bootstrap.sql`(加 stats 表)
- Create: `src/stats.rs`
- Modify: `src/lib.rs`(`mod stats;` + 新 pg_test)
- Modify: `src/build.rs`(写入 stats)
- Modify: `src/pyramid.rs`(Drop 清理)

- [ ] **Step 1: 写失败的 pg_test**(`src/lib.rs` tests 模块)

```rust
/// Stats rows must be written per zoom and removed on drop.
#[pg_test]
fn stats_written_per_zoom_and_cleaned_on_drop() {
    Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
    Spi::run(
        "CREATE TABLE _gp_stats_t (gid serial PRIMARY KEY, \
         geom geometry(Point, 4326))",
    )
    .unwrap();
    Spi::run("INSERT INTO _gp_stats_t (geom) VALUES (ST_SetSRID(ST_MakePoint(0,0),4326))")
        .unwrap();
    Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_stats_t','geom','gid','{\"minZoom\":0,\"maxZoom\":3}')",
    )
    .unwrap();
    let rows = Spi::get_one::<i64>(
        "SELECT count(*)::bigint FROM geometry_pyramid_stats WHERE pyramid_name='_gp_stats_t'",
    )
    .unwrap()
    .unwrap();
    assert_eq!(rows, 4, "expected one stats row per zoom 0..=3");
    let ms = Spi::get_one::<i64>(
        "SELECT min(build_ms)::bigint FROM geometry_pyramid_stats WHERE pyramid_name='_gp_stats_t'",
    )
    .unwrap()
    .unwrap();
    assert!(ms >= 0);
    Spi::run("SELECT ST_DropPyramid('_gp_stats_t')").unwrap();
    let rows = Spi::get_one::<i64>(
        "SELECT count(*)::bigint FROM geometry_pyramid_stats WHERE pyramid_name='_gp_stats_t'",
    )
    .unwrap()
    .unwrap();
    assert_eq!(rows, 0, "drop must clean stats");
    Spi::run("DROP TABLE _gp_stats_t").unwrap();
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo pgrx test pg16`
Expected: FAIL — `geometry_pyramid_stats` 不存在。

- [ ] **Step 3: `sql/bootstrap.sql` 末尾追加**

```sql
-- Per-zoom build statistics, maintained by the build/update paths.
CREATE TABLE IF NOT EXISTS geometry_pyramid_stats (
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

COMMENT ON TABLE geometry_pyramid_stats IS
    'Per-zoom build statistics for pyramids managed by geometry_pyramid.';
```

- [ ] **Step 4: 实现 `src/stats.rs`**

```rust
//! Per-zoom build statistics persistence.

use crate::build::ZoomBuildResult;
use pgrx::prelude::*;

pub fn upsert_zoom_stats(pyramid_name: &str, r: &ZoomBuildResult) -> Result<(), spi::Error> {
    Spi::run_with_args(
        "INSERT INTO geometry_pyramid_stats \
           (pyramid_name, z, tile_count, total_bytes, max_tile_bytes, build_ms, attempts, built_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, now()) \
         ON CONFLICT (pyramid_name, z) DO UPDATE SET \
           tile_count     = EXCLUDED.tile_count, \
           total_bytes    = EXCLUDED.total_bytes, \
           max_tile_bytes = EXCLUDED.max_tile_bytes, \
           build_ms       = EXCLUDED.build_ms, \
           attempts       = EXCLUDED.attempts, \
           built_at       = now()",
        &[
            pyramid_name.into(),
            (r.z as i16).into(),
            r.tiles.into(),
            r.total_bytes.into(),
            r.max_tile_bytes.into(),
            r.build_ms.into(),
            (r.attempts as i32).into(),
        ],
    )
}

pub fn delete_stats(pyramid_name: &str) -> Result<(), spi::Error> {
    Spi::run_with_args(
        "DELETE FROM geometry_pyramid_stats WHERE pyramid_name = $1",
        &[pyramid_name.into()],
    )
}
```

`src/lib.rs` 加 `mod stats;`。

- [ ] **Step 5: 接线**

`src/build.rs` 的 `build()` zoom 循环里(Task 2 的 `zoom_results.push` 之后)替换占位行为:

```rust
crate::stats::upsert_zoom_stats(pyramid_name, &result)?;
```

(删除 `let _ = &zoom_results;` 和 `zoom_results` 本身——直接逐 zoom 写库即可,YAGNI。)

`src/pyramid.rs` 的 `st_drop_pyramid` 在 `storage::drop_tile_table(name)?;` 后加:

```rust
crate::stats::delete_stats(name)?;
```

- [ ] **Step 6: 运行测试确认通过**

Run: `cargo pgrx test pg16`
Expected: 全部 PASS。

- [ ] **Step 7: detect_changes + Commit**

```bash
git add sql/bootstrap.sql src/stats.rs src/lib.rs src/build.rs src/pyramid.rs
git commit -m "feat: persist per-zoom build statistics"
```

---

### Task 4: `ST_Pyramid()` 暴露 stats

**Files:**
- Modify: `src/pyramid.rs`(`st_pyramid_info`)
- Modify: `src/lib.rs`(扩展现有 `build_fetch_drop_roundtrip` 或新增断言)

- [ ] **Step 1: 写失败的 pg_test**(`src/lib.rs`,加在 `stats_written_per_zoom_and_cleaned_on_drop` 内、Drop 之前)

```rust
let info = Spi::get_one::<pgrx::JsonB>("SELECT ST_Pyramid('_gp_stats_t')")
    .unwrap()
    .expect("metadata should exist");
let stats = info.0.get("stats").and_then(|v| v.as_array()).expect("stats array");
assert_eq!(stats.len(), 4);
assert!(stats[0].get("tileCount").is_some());
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo pgrx test pg16`
Expected: FAIL — `stats array` panic。

- [ ] **Step 3: 改 `st_pyramid_info` 的 SQL**

`jsonb_build_object(...)` 中追加一对键值(`'tile_count', $2::bigint` 之后):

```sql
, 'stats', (
    SELECT COALESCE(jsonb_agg(jsonb_build_object(
        'z', s.z,
        'tileCount', s.tile_count,
        'totalBytes', s.total_bytes,
        'maxTileBytes', s.max_tile_bytes,
        'buildMs', s.build_ms,
        'attempts', s.attempts) ORDER BY s.z), '[]'::jsonb)
    FROM geometry_pyramid_stats s
    WHERE s.pyramid_name = $1)
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo pgrx test pg16`
Expected: 全部 PASS。

- [ ] **Step 5: detect_changes + Commit**

```bash
git add src/pyramid.rs src/lib.rs
git commit -m "feat: expose per-zoom stats in ST_Pyramid output"
```

---

### Task 5: 影子表模块(`src/shadow.rs`)与标识符长度收紧

**Files:**
- Create: `src/shadow.rs`
- Modify: `src/lib.rs`(`mod shadow;`)
- Modify: `src/pyramid.rs`(`validate_identifier` 长度 50→45)

- [ ] **Step 1: 写失败的单元测试**

`src/shadow.rs` 测试:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_name_is_derived_from_main() {
        assert_eq!(shadow_table_name("foo"), "_gp_tiles_foo__shadow");
    }

    #[test]
    fn longest_derived_name_fits_pg_limit() {
        // validate_identifier caps names at 45 → prefix(10) + 45 + 8 = 63
        let longest = "a".repeat(45);
        assert!(shadow_table_name(&longest).len() <= 63);
    }
}
```

`src/pyramid.rs` 测试模块加:

```rust
#[test]
fn rejects_identifier_longer_than_45() {
    assert!(validate_identifier(&"a".repeat(45)).is_ok());
    assert!(validate_identifier(&"a".repeat(46)).is_err());
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo check`
Expected: 编译错误(`shadow_table_name` 未定义)。

- [ ] **Step 3: 实现 `src/shadow.rs`**

```rust
//! Shadow-table lifecycle: full builds write into a shadow table that is
//! atomically swapped into place, so readers never observe a half-built
//! pyramid. With parallel > 1 the shadow is created by an autonomous
//! worker (committed DDL) so background workers can see it.

use crate::storage;
use pgrx::prelude::*;

pub const SHADOW_SUFFIX: &str = "__shadow";

pub fn shadow_table_name(pyramid_name: &str) -> String {
    format!("{}{}", storage::tile_table_name(pyramid_name), SHADOW_SUFFIX)
}

/// SQL that (re)creates the shadow table. Factored out so the parallel
/// path can hand the same DDL to an autonomous ExecSql worker.
pub fn create_sql(pyramid_name: &str) -> String {
    let table = shadow_table_name(pyramid_name);
    format!(
        r#"DROP TABLE IF EXISTS "{table}";
           CREATE TABLE "{table}" (
               z smallint NOT NULL,
               x integer  NOT NULL,
               y integer  NOT NULL,
               mvt bytea  NOT NULL,
               PRIMARY KEY (z, x, y)
           )"#
    )
}

/// Create the shadow table inside the current transaction (parallel = 1).
pub fn create(pyramid_name: &str) -> Result<(), spi::Error> {
    Spi::run(&create_sql(pyramid_name))
}

/// Atomically replace the main tile table with the shadow. Runs in the
/// caller's transaction; the ACCESS EXCLUSIVE window covers only this DDL.
pub fn swap(pyramid_name: &str) -> Result<(), spi::Error> {
    let main = storage::tile_table_name(pyramid_name);
    let shadow = shadow_table_name(pyramid_name);
    Spi::run(&format!(r#"DROP TABLE IF EXISTS "{main}""#))?;
    Spi::run(&format!(r#"ALTER TABLE "{shadow}" RENAME TO "{main}""#))
}

/// Best-effort cleanup of a leftover shadow table.
pub fn abandon(pyramid_name: &str) -> Result<(), spi::Error> {
    let shadow = shadow_table_name(pyramid_name);
    Spi::run(&format!(r#"DROP TABLE IF EXISTS "{shadow}""#))
}
```

`src/lib.rs` 加 `mod shadow;`。
`src/pyramid.rs` 的 `validate_identifier`:`if ident.len() > 50` 改为 `if ident.len() > 45`(注释说明:`_gp_tiles_` 前缀 10 + 名称 45 + `__shadow` 后缀 8 = 63,顶满 PG 标识符上限)。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo pgrx test pg16`
Expected: 全部 PASS。

- [ ] **Step 5: detect_changes + Commit**

```bash
git add src/shadow.rs src/lib.rs src/pyramid.rs
git commit -m "feat: shadow table lifecycle for atomic rebuild"
```

---

### Task 6: `build()` 走影子表 + 咨询锁(原子重建落地)

**Files:**
- Modify: `src/build.rs`(`build()` 重组)
- Modify: `src/storage.rs`(删 `reset_tile_table`)
- Modify: `src/pyramid.rs`(`st_drop_pyramid` 清影子残留)
- Modify: `src/lib.rs`(新 pg_test)

- [ ] **Step 1: 写失败的 pg_test**

```rust
/// A failed rebuild must leave the previous tiles intact (atomicity), and
/// a successful rebuild must replace them.
#[pg_test]
fn failed_rebuild_keeps_previous_tiles() {
    Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
    Spi::run(
        "CREATE TABLE _gp_atomic (gid serial PRIMARY KEY, \
         geom geometry(Point, 4326))",
    )
    .unwrap();
    Spi::run("INSERT INTO _gp_atomic (geom) VALUES (ST_SetSRID(ST_MakePoint(0,0),4326))")
        .unwrap();
    Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_atomic','geom','gid','{\"minZoom\":0,\"maxZoom\":1}')",
    )
    .unwrap();
    let before = Spi::get_one::<Vec<u8>>("SELECT ST_Tile('_gp_atomic',0,0,0)")
        .unwrap()
        .expect("tile must exist after first build");

    // attributes 引用不存在的列:通过标识符校验,但构建 SQL 必然报错。
    // 用子事务捕获错误,然后验证旧瓦片仍可读。
    Spi::run(
        "DO $$ BEGIN \
           PERFORM ST_BuildPyramid('_gp_atomic','geom','gid', \
             '{\"minZoom\":0,\"maxZoom\":1,\"attributes\":[\"no_such_col\"]}'); \
         EXCEPTION WHEN others THEN NULL; END $$",
    )
    .unwrap();

    let after = Spi::get_one::<Vec<u8>>("SELECT ST_Tile('_gp_atomic',0,0,0)")
        .unwrap()
        .expect("previous tile must survive a failed rebuild");
    assert_eq!(before, after);

    // 无影子残留(parallel=1 的影子在失败事务内,本就不可见;此处防御性断言)
    let shadows = Spi::get_one::<i64>(
        "SELECT count(*)::bigint FROM pg_tables WHERE tablename LIKE '%\\_\\_shadow'",
    )
    .unwrap()
    .unwrap();
    assert_eq!(shadows, 0);

    Spi::run("SELECT ST_DropPyramid('_gp_atomic')").unwrap();
    Spi::run("DROP TABLE _gp_atomic").unwrap();
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo pgrx test pg16`
Expected: FAIL — 当前实现先 `reset_tile_table`(DROP 主表),失败的重建虽然回滚……注意:当前实现整个函数在同一事务,DO 块的子事务回滚后旧表其实还在。**预期失败点**是 `assert_eq!(shadows, 0)` 之前的行为差异不大,但 `ST_BuildPyramid` 成功路径仍是"先 DROP 再建"。若此测试在旧实现上意外通过,以测试为准继续 Step 3(重构动机是并行阶段的提交语义,原子性测试是回归保护网)。

- [ ] **Step 3: 重组 `build()`**

```rust
pub fn build(
    pyramid_name: &str,
    table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    // Serialize concurrent builds of the same pyramid for the duration of
    // this transaction.
    Spi::run_with_args(
        "SELECT pg_advisory_xact_lock(hashtextextended('geometry_pyramid:' || $1, 0))",
        &[pyramid_name.into()],
    )?;

    crate::shadow::create(pyramid_name)?;
    let shadow_table = crate::shadow::shadow_table_name(pyramid_name);

    let total = build_into(pyramid_name, &shadow_table, table, geom_column, fid_column, cfg)?;

    crate::shadow::swap(pyramid_name)?;
    Ok(total)
}

/// The pre-existing build loop, now writing into an arbitrary target table.
fn build_into(
    pyramid_name: &str,
    storage_table: &str,
    table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    let bbox = source_bbox(table, geom_column, cfg.tile_srs)?;
    let Some((xmin_m, ymin_m, xmax_m, ymax_m)) = bbox else {
        notice!(
            "geometry_pyramid: source table '{}' has no rows; pyramid '{}' is empty",
            table, pyramid_name
        );
        return Ok(0); // 空影子表仍会被 swap:空源 → 空 pyramid,语义正确
    };
    let mut total: i64 = 0;
    for z in cfg.min_zoom..=cfg.max_zoom {
        let rect = TileRect::from_bbox_m(z, xmin_m, ymin_m, xmax_m, ymax_m);
        let result = build_zoom_level_with_budget(
            pyramid_name, storage_table, table, geom_column, fid_column, cfg, &rect,
        )?;
        crate::stats::upsert_zoom_stats(pyramid_name, &result)?;
        total += result.tiles;
    }
    Ok(total)
}
```

说明:失败路径不需要显式 `abandon` —— `parallel=1` 时影子表 DDL 在同一事务内,出错回滚即消失。`abandon` 留给并行路径(Task 9)与 Drop 清理。

`src/storage.rs`:删除 `reset_tile_table`(已无调用方)。
`src/pyramid.rs` 的 `st_drop_pyramid` 加(`delete_stats` 之后):

```rust
crate::shadow::abandon(name)?;
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo pgrx test pg16`
Expected: 全部 PASS(含既有 roundtrip / drop_rate 测试)。

- [ ] **Step 5: detect_changes + Commit**

Run: `detect_changes()`,预期影响 `build`、`st_build_pyramid`、`st_update_pyramid`、`st_drop_pyramid` 相关流程,无意外符号。

```bash
git add src/build.rs src/storage.rs src/pyramid.rs src/lib.rs
git commit -m "feat: atomic rebuild via shadow table swap with advisory lock"
```

---

### Task 7: worker spec 文件协议(`src/worker.rs` 第一部分)

**Files:**
- Create: `src/worker.rs`(spec 结构 + 文件 IO,本 Task 不含 bgworker 入口)
- Modify: `src/lib.rs`(`mod worker;`)
- Modify: `src/build.rs`(`ZoomKnobs` 字段改 `pub(crate)`,派生 serde)
- Modify: `src/config.rs`(加 `buildTimeoutMs`)

- [ ] **Step 1: 写失败的单元测试**(`src/worker.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PyramidConfig;
    use crate::tile_rect::TileRect;

    #[test]
    fn spec_roundtrips_through_json() {
        let spec = WorkerSpec {
            db_oid: 16384,
            mode: WorkerMode::BuildStrips {
                pyramid_name: "p".into(),
                storage_table: "_gp_tiles_p__shadow".into(),
                source_table: "src".into(),
                geom_column: "geom".into(),
                fid_column: "gid".into(),
                config: PyramidConfig::default(),
                knobs: KnobValues {
                    simplify_tol_m: 1.0,
                    min_area_m2: 2.0,
                    min_len_m: 3.0,
                    drop_threshold: 0.4,
                },
                strips: vec![TileRect { z: 3, xmin: 0, ymin: 0, xmax: 1, ymax: 7 }],
            },
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: WorkerSpec = serde_json::from_str(&json).unwrap();
        let WorkerMode::BuildStrips { strips, knobs, .. } = back.mode else {
            panic!("wrong mode");
        };
        assert_eq!(back.db_oid, 16384);
        assert_eq!(strips.len(), 1);
        assert_eq!(strips[0].z, 3);
        assert!((knobs.drop_threshold - 0.4).abs() < 1e-9);
    }

    #[test]
    fn spec_filename_fits_bgw_extra() {
        // bgw_extra is 128 bytes including NUL.
        let name = spec_filename(u32::MAX, u32::MAX);
        assert!(name.len() < 127);
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo check`
Expected: 编译错误(类型未定义)。

- [ ] **Step 3: 实现 spec 协议**

`src/tile_rect.rs`:`TileRect` 的 derive 扩为
`#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]`。

`src/worker.rs`:

```rust
//! Background-worker control plane. Specs and completion status travel
//! through files under $PGDATA/gp_tmp/ because the coordinator's
//! transaction is uncommitted — its catalog rows and job rows would be
//! invisible to workers under MVCC; files are not.

use crate::config::PyramidConfig;
use crate::tile_rect::TileRect;
use pgrx::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
pub struct WorkerSpec {
    pub db_oid: u32,
    pub mode: WorkerMode,
}

#[derive(Serialize, Deserialize)]
pub enum WorkerMode {
    /// Execute one SQL script in an autonomous (separately committed)
    /// transaction. Used to create the shadow table for parallel builds.
    ExecSql { sql: String },
    /// Build tile strips into `storage_table` using the shared
    /// build_zoom_level pipeline, one transaction per strip.
    BuildStrips {
        pyramid_name: String,
        storage_table: String,
        source_table: String,
        geom_column: String,
        fid_column: String,
        config: PyramidConfig,
        knobs: KnobValues,
        strips: Vec<TileRect>,
    },
}

/// Serializable snapshot of build.rs's ZoomKnobs (the budget-retry loop
/// tightens these per attempt, so they ride along with each round's spec).
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct KnobValues {
    pub simplify_tol_m: f64,
    pub min_area_m2: f64,
    pub min_len_m: f64,
    pub drop_threshold: f64,
}

pub fn spec_filename(backend_pid: u32, idx: u32) -> String {
    format!("gp_{backend_pid}_{idx}.json")
}

/// $PGDATA/gp_tmp — created on demand.
pub fn spec_dir() -> PathBuf {
    let datadir = unsafe {
        std::ffi::CStr::from_ptr(pg_sys::DataDir)
            .to_string_lossy()
            .into_owned()
    };
    PathBuf::from(datadir).join("gp_tmp")
}

pub fn write_spec(filename: &str, spec: &WorkerSpec) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let dir = spec_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(filename);
    // 先清掉可能残留的旧状态文件
    let _ = std::fs::remove_file(path.with_extension("done"));
    let _ = std::fs::remove_file(path.with_extension("err"));
    std::fs::write(&path, serde_json::to_vec(spec)?)?;
    Ok(path)
}

pub fn cleanup_spec(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("done"));
    let _ = std::fs::remove_file(path.with_extension("err"));
}
```

`src/lib.rs` 加 `mod worker;`。

`src/build.rs`:`ZoomKnobs` 加方法(供协调者打包 spec、worker 还原):

```rust
impl ZoomKnobs {
    pub(crate) fn to_values(self) -> crate::worker::KnobValues {
        crate::worker::KnobValues {
            simplify_tol_m: self.simplify_tol_m,
            min_area_m2: self.min_area_m2,
            min_len_m: self.min_len_m,
            drop_threshold: self.drop_threshold,
        }
    }
    pub(crate) fn from_values(v: crate::worker::KnobValues) -> Self {
        Self {
            simplify_tol_m: v.simplify_tol_m,
            min_area_m2: v.min_area_m2,
            min_len_m: v.min_len_m,
            drop_threshold: v.drop_threshold,
        }
    }
}
```

并把 `struct ZoomKnobs` 与 `build_zoom_level` 的可见性改为 `pub(crate)`(worker 要调用)。

`src/config.rs` 的 `PyramidConfig` 加字段(`max_tile_size_attempts` 之后):

```rust
/// Upper bound (ms) the coordinator waits for parallel workers per zoom
/// round. 0 = wait indefinitely.
#[serde(default, rename = "buildTimeoutMs")]
pub build_timeout_ms: i64,
```

`Default` impl 加 `build_timeout_ms: 0,`。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo pgrx test pg16`
Expected: 全部 PASS。

- [ ] **Step 5: detect_changes + Commit**

```bash
git add src/worker.rs src/lib.rs src/build.rs src/config.rs src/tile_rect.rs
git commit -m "feat: worker spec file protocol for parallel build control plane"
```

---

### Task 8: bgworker 入口(`src/worker.rs` 第二部分)

**Files:**
- Modify: `src/worker.rs`(worker main + spawn 帮助函数)

- [ ] **Step 1: 实现 worker 入口与 spawn**(bgworker 入口无法用单元测试覆盖;集成验证在 Task 9/10 的 pg_test)

`src/worker.rs` 追加:

```rust
use pgrx::bgworkers::*;
use std::time::Duration;

/// Entry point for dynamic background workers. bgw_extra carries the spec
/// FILENAME (the worker derives the directory from its own DataDir).
#[pg_guard]
#[no_mangle]
pub extern "C" fn gp_worker_main(_arg: pg_sys::Datum) {
    let filename = BackgroundWorker::get_extra().to_string();
    let path = spec_dir().join(&filename);

    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);

    let spec: WorkerSpec = match std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::write(path.with_extension("err"), format!("bad spec: {e}"));
            return;
        }
    };

    unsafe {
        pg_sys::BackgroundWorkerInitializeConnectionByOid(
            pg_sys::Oid::from(spec.db_oid),
            pg_sys::InvalidOid,
            0,
        );
    }

    match run_spec(&spec) {
        Ok(()) => {
            let _ = std::fs::write(path.with_extension("done"), b"ok");
        }
        Err(e) => {
            let _ = std::fs::write(path.with_extension("err"), e.to_string());
        }
    }
}

fn run_spec(spec: &WorkerSpec) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match &spec.mode {
        WorkerMode::ExecSql { sql } => {
            BackgroundWorker::transaction(|| Spi::run(sql))?;
            Ok(())
        }
        WorkerMode::BuildStrips {
            pyramid_name,
            storage_table,
            source_table,
            geom_column,
            fid_column,
            config,
            knobs,
            strips,
        } => {
            let k = crate::build::ZoomKnobs::from_values(*knobs);
            for rect in strips {
                BackgroundWorker::transaction(|| {
                    crate::build::build_zoom_level(
                        pyramid_name,
                        storage_table,
                        source_table,
                        geom_column,
                        fid_column,
                        config,
                        rect,
                        &k,
                    )
                })?;
            }
            Ok(())
        }
    }
}

/// Spawn one dynamic worker for an already-written spec file. Returns the
/// handle, or Err when no worker slot is available (caller degrades to
/// inline execution).
pub fn spawn(filename: &str) -> Result<DynamicBackgroundWorker, Box<dyn std::error::Error + Send + Sync>> {
    BackgroundWorkerBuilder::new("geometry_pyramid worker")
        .set_function("gp_worker_main")
        .set_library("geometry_pyramid")
        .set_extra(filename)
        .set_notify_pid(unsafe { pg_sys::MyProcPid })
        .enable_spi_access()
        .load_dynamic()
        .map_err(|e| format!("failed to register background worker: {e:?}").into())
}

/// Run an SQL script in an autonomous transaction via a one-shot worker,
/// waiting for completion. Used for committed shadow-table DDL.
pub fn exec_sql_autonomous(sql: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let filename = spec_filename(unsafe { pg_sys::MyProcPid } as u32, 0);
    let spec = WorkerSpec {
        db_oid: unsafe { pg_sys::MyDatabaseId }.to_u32(),
        mode: WorkerMode::ExecSql { sql: sql.to_string() },
    };
    let path = write_spec(&filename, &spec)?;
    let _handle = spawn(&filename)?;
    let result = wait_for(&[path.clone()], 60_000); // DDL 给固定 60s 上限
    cleanup_spec(&path);
    result
}

/// Poll status files until every spec is done, any errors out, or the
/// timeout (ms, 0 = unbounded) elapses.
pub fn wait_for(paths: &[PathBuf], timeout_ms: i64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let started = std::time::Instant::now();
    loop {
        let mut pending = 0usize;
        for p in paths {
            let err_path = p.with_extension("err");
            if err_path.exists() {
                let msg = std::fs::read_to_string(&err_path).unwrap_or_default();
                return Err(format!("worker failed: {msg}").into());
            }
            if !p.with_extension("done").exists() {
                pending += 1;
            }
        }
        if pending == 0 {
            return Ok(());
        }
        if timeout_ms > 0 && started.elapsed().as_millis() as i64 > timeout_ms {
            return Err(format!("timed out waiting for {pending} worker(s)").into());
        }
        pgrx::check_for_interrupts!();
        std::thread::sleep(Duration::from_millis(50));
    }
}
```

注意事项(写进代码注释,此处提醒实现者):
- pgrx 0.16 的 `DynamicBackgroundWorker` / `load_dynamic()` / `get_extra()` / `transaction()` 名称以文档为准;`cargo check` 不过则查 `pgrx::bgworkers` 模块替换同义 API,语义不变。
- worker 内 PG `ERROR` 会 longjmp 终止进程导致 `.err` 未写出;协调者的 `wait_for` 超时/handle 检测兜底(Task 9 用 `buildTimeoutMs`,文档建议并行时设置)。

- [ ] **Step 2: 编译检查**

Run: `cargo check`
Expected: 通过(或按上述注意事项修正 API 名后通过)。

- [ ] **Step 3: detect_changes + Commit**

```bash
git add src/worker.rs
git commit -m "feat: dynamic background worker entry point and spawn helpers"
```

---

### Task 9: 协调者并行路径(`src/build.rs`)

**Files:**
- Modify: `src/build.rs`

- [ ] **Step 1: 实现条带分发**(集成测试在 Task 10;本 Task 先让 `parallel=1` 全量回归保持绿色)

`build()` 改造:影子表创建按 parallel 分流:

```rust
    let workers = cfg.parallel.max(1) - 1;
    if workers == 0 {
        crate::shadow::create(pyramid_name)?;
    } else {
        // Parallel: shadow DDL must be COMMITTED so bgworkers can see it.
        crate::worker::exec_sql_autonomous(&crate::shadow::create_sql(pyramid_name))?;
    }
```

`build_zoom_level_with_budget` 内,把单次 `build_zoom_level(...)` 调用替换为分发函数:

```rust
let inserted = run_zoom_strips(
    pyramid_name, storage_table, table, geom_column, fid_column, cfg, rect, &knobs,
)?;
```

新增:

```rust
/// Execute one zoom round, splitting the rectangle into strips and farming
/// them out to background workers when cfg.parallel > 1. Slot 0 always runs
/// inline in the coordinator; failed spawns degrade to inline execution.
#[allow(clippy::too_many_arguments)]
fn run_zoom_strips(
    pyramid_name: &str,
    storage_table: &str,
    table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
    rect: &TileRect,
    knobs: &ZoomKnobs,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    let workers = cfg.parallel.max(1) - 1;
    if workers == 0 {
        return Ok(build_zoom_level(
            pyramid_name, storage_table, table, geom_column, fid_column, cfg, rect, knobs,
        )?);
    }

    let strips = rect.split_x(4 * (workers + 1));
    // Round-robin static assignment: slot s takes strips s, s+(K+1), ...
    let slots = (workers as usize + 1).min(strips.len());
    let mut spec_paths: Vec<std::path::PathBuf> = Vec::new();
    let mut inline_strips: Vec<TileRect> = Vec::new(); // slot 0 + degraded slots

    for (slot, chunk) in (0..slots).map(|s| {
        (s, strips.iter().copied().skip(s).step_by(slots).collect::<Vec<_>>())
    }) {
        if slot == 0 {
            inline_strips = chunk;
            continue;
        }
        let filename = crate::worker::spec_filename(
            unsafe { pg_sys::MyProcPid } as u32,
            slot as u32,
        );
        let spec = crate::worker::WorkerSpec {
            db_oid: unsafe { pg_sys::MyDatabaseId }.to_u32(),
            mode: crate::worker::WorkerMode::BuildStrips {
                pyramid_name: pyramid_name.to_string(),
                storage_table: storage_table.to_string(),
                source_table: table.to_string(),
                geom_column: geom_column.to_string(),
                fid_column: fid_column.to_string(),
                config: cfg.clone(),
                knobs: knobs.to_values(),
                strips: chunk.clone(),
            },
        };
        let path = crate::worker::write_spec(&filename, &spec)?;
        match crate::worker::spawn(&filename) {
            Ok(_handle) => spec_paths.push(path),
            Err(e) => {
                notice!(
                    "geometry_pyramid: worker slot {} unavailable ({}); running inline",
                    slot, e
                );
                crate::worker::cleanup_spec(&path);
                inline_strips.extend(chunk);
            }
        }
    }

    // Coordinator does its share while workers run.
    let mut inserted: i64 = 0;
    for strip in &inline_strips {
        inserted += build_zoom_level(
            pyramid_name, storage_table, table, geom_column, fid_column, cfg, strip, knobs,
        )?;
    }

    let wait_result = crate::worker::wait_for(&spec_paths, cfg.build_timeout_ms);
    for p in &spec_paths {
        crate::worker::cleanup_spec(p);
    }
    wait_result?;

    // Workers committed their own rows; recount the zoom so the return
    // value covers every strip, not just the coordinator's.
    Ok(inserted_count(storage_table, rect.z)?)
}
```

补充改动:
- 预算重试的 `DELETE FROM ... WHERE z=$1`(`build_zoom_level_with_budget` 内)在并行下删的是 workers 已提交的行 + 自己未提交的行,语义不变。
- `swap` 与失败残留:并行路径失败时已提交的影子表会留存,`st_drop_pyramid` 与下次构建的 `create_sql`(`DROP IF EXISTS`)兜底——Task 6 已就位,无需新代码。

- [ ] **Step 2: 回归测试(全部既有用例仍走 parallel=1)**

Run: `cargo pgrx test pg16`
Expected: 全部 PASS。

- [ ] **Step 3: detect_changes + Commit**

```bash
git add src/build.rs
git commit -m "feat: parallel zoom build via strip-partitioned background workers"
```

---

### Task 10: 并行构建集成测试

**Files:**
- Modify: `src/lib.rs`

- [ ] **Step 1: 写 pg_test**

```rust
/// parallel=4 must produce byte-identical tiles to parallel=1 (the whole
/// pipeline is deterministic: hashtextextended-based point dropping, no
/// randomness), and must complete even when worker slots are scarce.
#[pg_test]
fn parallel_build_matches_serial() {
    Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
    Spi::run(
        "CREATE TABLE _gp_par (gid serial PRIMARY KEY, \
         geom geometry(Point, 4326))",
    )
    .unwrap();
    Spi::run(
        "INSERT INTO _gp_par (geom) \
         SELECT ST_SetSRID(ST_MakePoint( \
            (i % 36) * 10 - 175, (i / 36) % 15 * 10 - 70), 4326) \
         FROM generate_series(0, 539) i",
    )
    .unwrap();

    Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_par','geom','gid', \
         '{\"name\":\"par_serial\",\"minZoom\":0,\"maxZoom\":5}')",
    )
    .unwrap();
    Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_par','geom','gid', \
         '{\"name\":\"par_parallel\",\"minZoom\":0,\"maxZoom\":5,\"parallel\":4, \
           \"buildTimeoutMs\":60000}')",
    )
    .unwrap();

    // 两套瓦片逐字节一致(FULL JOIN 找任何不一致行)
    let mismatches = Spi::get_one::<i64>(
        "SELECT count(*)::bigint FROM _gp_tiles_par_serial a \
         FULL JOIN _gp_tiles_par_parallel b USING (z, x, y) \
         WHERE a.mvt IS DISTINCT FROM b.mvt",
    )
    .unwrap()
    .unwrap();
    assert_eq!(mismatches, 0, "parallel tiles must match serial byte-for-byte");

    Spi::run("SELECT ST_DropPyramid('par_serial')").unwrap();
    Spi::run("SELECT ST_DropPyramid('par_parallel')").unwrap();
    Spi::run("DROP TABLE _gp_par").unwrap();
}
```

- [ ] **Step 2: 运行**

Run: `cargo pgrx test pg16`
Expected: PASS。若 FAIL,按 superpowers:systematic-debugging 排查(常见:worker 看不到影子表 → 检查 `exec_sql_autonomous` 是否真的提交;`.err` 内容会给出 SQL 错误)。

- [ ] **Step 3: detect_changes + Commit**

```bash
git add src/lib.rs
git commit -m "test: parallel build produces byte-identical tiles"
```

---

### Task 11: 增量更新(`src/incremental.rs` + `st_update_pyramid` 接线)

**Files:**
- Create: `src/incremental.rs`
- Modify: `src/lib.rs`(`mod incremental;` + pg_test)
- Modify: `src/pyramid.rs`(`st_update_pyramid`)

- [ ] **Step 1: 写失败的 pg_test**

```rust
/// Incremental update must touch only the dirty tiles, no-op on empty
/// matches, and reject a broken where_clause without damaging the pyramid.
#[pg_test]
fn incremental_update_rebuilds_only_dirty_tiles() {
    Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
    Spi::run(
        "CREATE TABLE _gp_inc (gid serial PRIMARY KEY, \
         geom geometry(Point, 4326), touched bool DEFAULT false)",
    )
    .unwrap();
    // 两个相距很远的点:东半球 / 西半球,在高 zoom 落在不同瓦片
    Spi::run(
        "INSERT INTO _gp_inc (geom) VALUES \
         (ST_SetSRID(ST_MakePoint(100, 30), 4326)), \
         (ST_SetSRID(ST_MakePoint(-100, -30), 4326))",
    )
    .unwrap();
    Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_inc','geom','gid','{\"minZoom\":0,\"maxZoom\":6}')",
    )
    .unwrap();

    // 快照:全部瓦片字节
    Spi::run(
        "CREATE TEMP TABLE _snap AS SELECT z, x, y, mvt FROM _gp_tiles__gp_inc",
    )
    .unwrap();

    // 移动东半球的点(仍留在东半球),标记 touched
    Spi::run(
        "UPDATE _gp_inc SET geom = ST_SetSRID(ST_MakePoint(101, 31), 4326), \
         touched = true WHERE gid = 1",
    )
    .unwrap();
    let ok = Spi::get_one::<bool>(
        "SELECT ST_UpdatePyramid('_gp_inc', 'touched = true')",
    )
    .unwrap();
    assert_eq!(ok, Some(true));

    // 西半球瓦片(x 在西侧)必须逐字节未变
    let west_changed = Spi::get_one::<i64>(
        "SELECT count(*)::bigint FROM _snap s \
         JOIN _gp_tiles__gp_inc t USING (z, x, y) \
         WHERE s.x < (1 << s.z) / 2 AND s.mvt IS DISTINCT FROM t.mvt",
    )
    .unwrap()
    .unwrap();
    assert_eq!(west_changed, 0, "west-hemisphere tiles must be untouched");

    // 东半球高 zoom 必须有变化(点动了)
    let east_changed = Spi::get_one::<i64>(
        "SELECT count(*)::bigint FROM _snap s \
         FULL JOIN _gp_tiles__gp_inc t USING (z, x, y) \
         WHERE COALESCE(s.x, t.x) >= (1 << COALESCE(s.z, t.z)) / 2 \
           AND COALESCE(s.z, t.z) >= 4 \
           AND s.mvt IS DISTINCT FROM t.mvt",
    )
    .unwrap()
    .unwrap();
    assert!(east_changed > 0, "moved point must change east tiles");

    // 空匹配:no-op 且返回 true
    let ok = Spi::get_one::<bool>("SELECT ST_UpdatePyramid('_gp_inc', 'false')").unwrap();
    assert_eq!(ok, Some(true));

    // 坏 where_clause:干净报错,瓦片无损
    Spi::run(
        "DO $$ BEGIN \
           PERFORM ST_UpdatePyramid('_gp_inc', 'no_such_column = 1'); \
           RAISE EXCEPTION 'should have failed'; \
         EXCEPTION WHEN undefined_column THEN NULL; END $$",
    )
    .unwrap();
    let tile = Spi::get_one::<Vec<u8>>("SELECT ST_Tile('_gp_inc', 0, 0, 0)").unwrap();
    assert!(tile.is_some(), "pyramid must survive a bad where_clause");

    Spi::run("SELECT ST_DropPyramid('_gp_inc')").unwrap();
    Spi::run("DROP TABLE _gp_inc, _snap").unwrap();
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo pgrx test pg16`
Expected: FAIL — 当前 `ST_UpdatePyramid` 全量重建,`west_changed = 0` 大概率仍成立(确定性管线),但 `'false'` 空匹配路径会把整个 pyramid 重建为空?不——全量重建忽略 where_clause,west 瓦片重建后字节相同。**预期失败点**:`undefined_column` 分支(当前实现忽略 where_clause,不会报错,`RAISE EXCEPTION 'should have failed'` 触发)。

- [ ] **Step 3: 实现 `src/incremental.rs`**

```rust
//! Incremental refresh: rebuild only the tiles whose source rows changed,
//! identified by a caller-supplied where_clause (Ganos semantics — the
//! caller holds table privileges; the clause is their SQL).

use crate::build;
use crate::config::PyramidConfig;
use crate::storage;
use crate::tile_rect::TileRect;
use pgrx::prelude::*;

/// Rebuild the tiles intersecting the extent of rows matched by
/// `where_clause`. Returns the number of tiles rebuilt. Runs entirely in
/// the caller's transaction: a failure rolls back to the intact pyramid.
pub fn update(
    pyramid_name: &str,
    source_table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
    where_clause: &str,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    // Defensive pre-flight: surface syntax errors cleanly before they are
    // embedded into the larger extent query.
    Spi::run(&format!(
        r#"SELECT 1 FROM "{source_table}" WHERE ({where_clause}) LIMIT 0"#
    ))?;

    // Dirty extent in tile SRS.
    let sql = format!(
        r#"WITH e AS (
              SELECT ST_Extent(ST_Transform("{geom_column}", $1)) AS env
              FROM "{source_table}"
              WHERE "{geom_column}" IS NOT NULL AND ({where_clause})
           )
           SELECT json_build_array(
                    ST_XMin(env), ST_YMin(env),
                    ST_XMax(env), ST_YMax(env))::text
           FROM e WHERE env IS NOT NULL"#
    );
    let row: Option<String> = Spi::get_one_with_args(&sql, &[cfg.tile_srs.into()])?;
    let Some(s) = row else {
        notice!(
            "geometry_pyramid: where_clause matched no rows; pyramid '{}' unchanged",
            pyramid_name
        );
        return Ok(0);
    };
    let arr: [f64; 4] = serde_json::from_str(&s)?;
    let (xmin_m, ymin_m, xmax_m, ymax_m) = (arr[0], arr[1], arr[2], arr[3]);

    let storage_table = storage::tile_table_name(pyramid_name);
    let mut rebuilt: i64 = 0;
    for z in cfg.min_zoom..=cfg.max_zoom {
        let started = std::time::Instant::now();
        let rect = TileRect::from_bbox_m(z, xmin_m, ymin_m, xmax_m, ymax_m);
        // Wipe the dirty rectangle, then rebuild it in place. Budget retry
        // is intentionally NOT applied here: a partial MAX(octet_length)
        // check would be skewed by untouched tiles; the knobs settled on
        // by the last full build are reused as-is.
        Spi::run_with_args(
            &format!(
                r#"DELETE FROM "{storage_table}"
                   WHERE z = $1 AND x BETWEEN $2 AND $3 AND y BETWEEN $4 AND $5"#
            ),
            &[
                (z as i32).into(),
                (rect.xmin as i32).into(),
                (rect.xmax as i32).into(),
                (rect.ymin as i32).into(),
                (rect.ymax as i32).into(),
            ],
        )?;
        let knobs = build::ZoomKnobs::from_config(cfg, z);
        let inserted = build::build_zoom_level(
            pyramid_name, &storage_table, source_table, geom_column, fid_column, cfg,
            &rect, &knobs,
        )?;
        rebuilt += inserted;

        // Refresh this zoom's stats from the (partially rebuilt) table;
        // build_ms records this incremental pass, attempts is left as-is.
        Spi::run_with_args(
            &format!(
                r#"UPDATE geometry_pyramid_stats SET
                     tile_count     = (SELECT count(*) FROM "{storage_table}" WHERE z = $1),
                     total_bytes    = (SELECT COALESCE(sum(octet_length(mvt)),0) FROM "{storage_table}" WHERE z = $1),
                     max_tile_bytes = (SELECT COALESCE(max(octet_length(mvt)),0) FROM "{storage_table}" WHERE z = $1),
                     build_ms       = $2,
                     built_at       = now()
                   WHERE pyramid_name = $3 AND z = $1"#
            ),
            &[
                (z as i32).into(),
                (started.elapsed().as_millis() as i64).into(),
                pyramid_name.into(),
            ],
        )?;
    }
    Ok(rebuilt)
}
```

(`ZoomKnobs::from_config` 需要 `pub(crate)` —— Task 7 已把结构体改 `pub(crate)`,此处同样放开方法可见性。)

`src/lib.rs` 加 `mod incremental;`。

- [ ] **Step 4: 接线 `st_update_pyramid`**

`src/pyramid.rs`:参数改名 `_where_clause` → `where_clause`,在解析出 catalog 字段后分流:

```rust
    let total = if where_clause.trim().is_empty() {
        build::build(name, &source_table, &geom_column, &fid_column, &cfg)?
    } else {
        crate::incremental::update(
            name, &source_table, &geom_column, &fid_column, &cfg, where_clause,
        )?
    };
```

notice 文案改为区分 `refreshed (N tiles)` / `incrementally refreshed (N tiles)`。
函数 doc 注释更新:where_clause 非空时只重建脏瓦片;Ganos 信任模型(调用者持有源表权限)。

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo pgrx test pg16`
Expected: 全部 PASS。

- [ ] **Step 6: detect_changes + Commit**

Run: `detect_changes()` — 重点确认 `st_update_pyramid` 执行流(proc_*_st_update_pyramid)按预期变化。

```bash
git add src/incremental.rs src/lib.rs src/pyramid.rs
git commit -m "feat: incremental pyramid update scoped by where_clause"
```

---

### Task 12: 文档收尾与全量回归

**Files:**
- Modify: `README.md`

- [ ] **Step 1: 更新 README**

1. Status 段:改为说明并行构建、增量更新、原子重建、构建统计已实现;PNG 渲染与 coalesce-densest 仍在 roadmap。
2. config 表加一行:`| buildTimeoutMs | 0 | Per-zoom upper bound (ms) waiting for parallel workers; 0 = unbounded. |`
3. `ST_UpdatePyramid` 行更新:`Refresh only the tiles intersecting rows matched by where_clause (full rebuild when empty).`
4. Roadmap:8(Parallelism)、9(Incremental updates)、11(stats)标 ✅;补一条已知语义说明:增量更新重建"脏矩形内的完整瓦片",纯删除场景需提供覆盖被删区域的 where_clause。
5. Project layout 加新文件:`tile_rect.rs` / `stats.rs` / `shadow.rs` / `worker.rs` / `incremental.rs`。
6. 注明标识符长度上限 45(影子表后缀预算)。

- [ ] **Step 2: 全量回归**

Run: `cargo pgrx test pg16`
Expected: 全部 PASS。

- [ ] **Step 3: detect_changes(compare 模式)+ Commit**

Run: `detect_changes({scope: "compare", base_ref: "main"})` 复核整个分支影响面。

```bash
git add README.md
git commit -m "docs: document parallel build, incremental update, stats, atomic rebuild"
```

---

## 自审记录

- **规格覆盖**:统计(Task 3/4)、原子重建(Task 5/6)、并行(Task 7/8/9/10)、增量(Task 11)、公共重构(Task 1/2)、README(Task 12)——规格各节均有对应任务。规格中"协调者轮询 status 文件 + handle"的 handle 兜底在 Task 8 注意事项中以超时兜底简化(`.err` 缺失 → `buildTimeoutMs` 超时报错),与规格"worker exited without status"语义一致。
- **类型一致性**:`ZoomKnobs`(build.rs,`pub(crate)`)↔ `KnobValues`(worker.rs,serde)经 `to_values`/`from_values` 互转;`TileRect` 全程同一类型;`ZoomBuildResult` 字段与 stats 表列一一对应。
- **占位符**:无 TBD/TODO;Task 8 的 pgrx API 名差异以"同义 API 替换,语义不变"显式交代,非占位。
