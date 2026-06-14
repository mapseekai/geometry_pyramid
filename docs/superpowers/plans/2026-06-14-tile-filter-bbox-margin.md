# 候选过滤优化(&& + envelope margin)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把共享管线 `tile_pipeline_from_sql` 的源表候选过滤从 `ST_Intersects(geom, envelope)` 改成 `geom && ST_Transform(ST_TileEnvelope(z,x,y, margin => tileExtend/tileSize), source_srs)` —— 去掉冗余精确复检(走索引-only)并纳入缓冲区边缘几何。构建与动态切片同时受益。

**Architecture:** 仅改 `build.rs:tile_pipeline_from_sql` 的一段 WHERE;`ST_AsMVTGeom` 调用不动。margin 与 SRID 全部复用已绑定参数($1 z、$3 tile_size、$9 tile_extend、$10 source_srs),不新增参数/配置。

**Tech Stack:** Rust + pgrx 0.16(`CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`),PostGIS 3.6.4(`ST_TileEnvelope` 的 margin 参数需 3.1+,已满足)。

**通用约定:**
- 在 `main` 分支(执行流程会切到专用分支)。Test:`CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`。基线 33 passed。
- Commit `<type>: <description>`,无 attribution。忽略 gitnexus MCP 提示。不要 push。

---

### Task 1: WHERE 改 `&&` + margin,加回归 pg_test

**Files:**
- Modify: `src/build.rs`(`tile_pipeline_from_sql` 的 WHERE)
- Modify: `src/lib.rs`(新 pg_test)

- [ ] **Step 1: 写 pg_test**(`src/lib.rs` tests 模块)

精确单测"缓冲带被填充"需要解码 MVT,成本过高;本测试做**回归**:验证 `&&`+margin 改写后管线仍产出正确瓦片(有数据→非空,空区→NULL)。无缝边缘的视觉效果由浏览器 demo 验收,管线确定性由现有逐字节对比测试保障。

```rust
/// The && + tile-envelope-margin candidate filter still produces correct
/// tiles: non-empty where the source has geometry, NULL for empty regions.
/// (The seamless-edge buffer benefit is validated visually in demo/.)
#[pg_test]
fn bbox_filter_and_margin_render() {
    Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
    Spi::run(
        "CREATE TABLE _gp_bbox (gid serial PRIMARY KEY, geom geometry(Polygon, 4326))",
    )
    .unwrap();
    // A polygon near (0,0) spanning a small box.
    Spi::run(
        "INSERT INTO _gp_bbox (geom) VALUES \
         (ST_SetSRID(ST_MakeEnvelope(-1, -1, 1, 1), 4326))",
    )
    .unwrap();
    Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_bbox','geom','gid', \
         '{\"minZoom\":0,\"maxZoom\":3,\"splitSize\":0}')",
    )
    .unwrap();

    // z=0 covers the world; the polygon makes it non-empty.
    let z0 = Spi::get_one::<Vec<u8>>("SELECT ST_Tile('_gp_bbox', 0, 0, 0)").unwrap();
    assert!(z0.is_some() && !z0.unwrap().is_empty(), "z0 tile must contain the polygon");

    // The z=0/0/0 envelope is the whole world; pick an empty high tile far from
    // the polygon: at z=3 the polygon sits near tile (4,4); tile (0,0) (far NW)
    // has no geometry → NULL.
    let empty = Spi::get_one::<Vec<u8>>("SELECT ST_Tile('_gp_bbox', 3, 0, 0)").unwrap();
    assert!(empty.is_none(), "empty region tile must be NULL");

    Spi::run("SELECT ST_DropPyramid('_gp_bbox')").unwrap();
    Spi::run("DROP TABLE _gp_bbox").unwrap();
}
```

- [ ] **Step 2: 运行**(基线确认:此测试在改写前应已通过,因为 `ST_Intersects` 也产出这些瓦片)

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: 34 passed(33 + 新测试)。本测试不是 RED→GREEN 式(它是改写的回归网);先确认它在旧实现下也绿,再做 Step 3 改写,确保改写不破坏它。

- [ ] **Step 3: 改 WHERE**(`src/build.rs`,`tile_pipeline_from_sql`)

把:
```rust
                 FROM "{table}" src
                 WHERE src."{geom_column}" IS NOT NULL
                   AND ST_Intersects(
                          src."{geom_column}",
                          ST_Transform(ST_TileEnvelope($1, t.x, t.y), $10)
                       )
```
改为:
```rust
                 FROM "{table}" src
                 WHERE src."{geom_column}" IS NOT NULL
                   AND src."{geom_column}" && ST_Transform(
                          ST_TileEnvelope($1, t.x, t.y, margin => ($9::float8 / $3::float8)),
                          $10
                       )
```
说明:`&&` 是 bbox 重叠判定(走 GiST 索引);`margin => $9/$3` = tileExtend/tileSize 放大过滤包络以纳入缓冲带几何;`$1`/`$3`/`$9`/`$10` 均为该 INSERT 已绑定参数,绑定数组与其余 SQL 不变。`&&` 两侧 SRID 一致(`src.geom` 为 source_srs,`ST_Transform(...,$10=source_srs)`)。

> `tile_pipeline_from_sql` 被构建 INSERT 与 `dynamic::generate_tile` 的单瓦片 SELECT 共用,改这一处即两路同时生效。

- [ ] **Step 4: 运行测试确认通过**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: 34 passed。重点确认这些仍绿:
- `bbox_filter_and_margin_render`(本任务新增)
- `parallel_build_matches_serial`(串/并行同管线,逐字节仍相等)
- `build_fetch_drop_roundtrip` / `drop_rate_reduces_low_zoom_features` / `incremental_update_rebuilds_only_dirty_tiles` / `split_size_defers_sparse_tiles`

若报 `function st_tileenvelope(..., margin => ...) does not exist`:PostGIS 版本 < 3.1;本环境为 3.6.4,不应发生——若发生,确认连接的是 pgrx pg17 实例(已装 3.6.4)。
若报 `&&` SRID 不匹配:确认 `ST_Transform(..., $10)` 的 `$10` 是 source_srs(与 src.geom 同 SRID)。

- [ ] **Step 5: detect_changes + Commit**

Run: `detect_changes()`(MCP),确认仅 `tile_pipeline_from_sql` / build 相关符号受影响。

```bash
git add src/build.rs src/lib.rs
git commit -m "perf: && + tile-envelope margin for candidate filter (index-only, buffer-aware)"
```

---

## 自审记录

- **规格覆盖**:WHERE 改 `&&`(Step 3)、margin = $9/$3(Step 3)、共享管线两路生效(Step 3 说明)、pg_test(Step 1)、现有测试保持绿(Step 4 清单)——spec 各节均覆盖。
- **占位符**:无 TBD;Step 3 给出精确 old→new。
- **测试诚实性**:明确说明本 pg_test 是改写回归(非缓冲带精确断言,因需解码 MVT);无缝边缘视觉效果由 demo 验收、管线确定性由 parallel 逐字节测试保障——已在测试注释与计划中如实写明,不夸大。
- **一致性**:margin 形参 `$9/$3`、SRID `$10` 与 spec 一致;`ST_AsMVTGeom` 不动;参数绑定数组不变。
- **风险**:`ST_TileEnvelope` margin 命名参数依赖 PostGIS 3.1+(环境 3.6.4 满足),Step 4 给出排错指引。
