# ST_UpdatePyramidByExtent(按范围更新)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增 `ST_UpdatePyramidByExtent(name, xmin, ymin, xmax, ymax, srid)` —— 把给定 bbox(在 srid)内的瓦片按当前源数据整体重切,正确反映该区域的插入/更新/删除/移动。

**Architecture:** 把 `incremental::update` 里"逐 zoom 脏矩形 DELETE+重切+刷 stats"抽成共享 `rebuild_extent`;`update`(where_clause 路径)与新 `update_extent`(bbox 路径)都调它。`pyramid.rs` 把目录行解析抽成 `catalog_meta` 复用,新增 `st_updatepyramidbyextent`。

**Tech Stack:** Rust + pgrx 0.16(`CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`),PostGIS。

**通用约定:**
- 在 `main`(执行流程切专用分支)。Test:`CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`。基线 36 passed。
- Commit `<type>: <description>`,无 attribution。忽略 gitnexus MCP 提示。不要 push。

---

### Task 1: incremental.rs — 抽 `rebuild_extent` + 新 `update_extent`

**Files:**
- Modify: `src/incremental.rs`

`rebuild_extent` 被 `update()` 复用(非死代码);`update_extent` 在 Task 2 接入前是死代码(预期 dead_code 警告,不加 `#[allow]`)。

- [ ] **Step 1: 抽出 `rebuild_extent`,改写 `update` 调用它**

把 `update()` 中从 `let storage_table = ...` 到函数末尾 `Ok(rebuilt)`(当前第 56–108 行的逐 zoom 循环)整段移出为新函数,`update()` 改为算完 extent 后调用它。

`update()` 末尾(第 53 行 `let arr...` 之后)替换为:
```rust
    let arr: [f64; 4] = serde_json::from_str(&s)?;
    rebuild_extent(
        pyramid_name, source_table, geom_column, fid_column, cfg,
        arr[0], arr[1], arr[2], arr[3],
    )
}
```

在 `update()` 之后新增:
```rust
/// Rebuild every tile intersecting the given tile-SRS bbox across
/// minZoom..=maxZoom: DELETE the dirty rectangle then re-slice it in place,
/// refreshing per-zoom stats. Runs in the caller's transaction (rollback-safe).
/// Re-slices the *current* source within the rectangle, so it reflects inserts,
/// updates, deletes, and moves in that region.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rebuild_extent(
    pyramid_name: &str,
    source_table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
    xmin_m: f64,
    ymin_m: f64,
    xmax_m: f64,
    ymax_m: f64,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    let storage_table = storage::tile_table_name(pyramid_name);
    let mut rebuilt: i64 = 0;
    for z in cfg.min_zoom..=cfg.max_zoom {
        let started = std::time::Instant::now();
        let rect = TileRect::from_bbox_m(z, xmin_m, ymin_m, xmax_m, ymax_m);
        // Wipe the dirty rectangle, then rebuild it in place. Budget retry is
        // intentionally NOT applied here: a partial MAX(octet_length) check
        // would be skewed by untouched tiles; reuse the knobs from config.
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
            pyramid_name,
            &storage_table,
            source_table,
            geom_column,
            fid_column,
            cfg,
            &rect,
            &knobs,
        )?;
        rebuilt += inserted;

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
                (i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)).into(),
                pyramid_name.into(),
            ],
        )?;
    }
    Ok(rebuilt)
}
```

- [ ] **Step 2: 新增 `update_extent`**

```rust
/// Rebuild the tiles intersecting a caller-supplied bbox given in `srid`.
/// The bbox is transformed to the tile SRS, then the region is re-sliced from
/// the current source (handles inserts/updates/deletes/moves in that region).
#[allow(clippy::too_many_arguments)]
pub fn update_extent(
    pyramid_name: &str,
    source_table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
    xmin: f64,
    ymin: f64,
    xmax: f64,
    ymax: f64,
    srid: i32,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    // Transform the caller bbox (in `srid`) to the tile SRS and read its edges.
    // ST_MakeEnvelope normalizes min/max; the CASE returns NULL on a degenerate
    // envelope so get_one_with_args sees one row (NULL), not zero rows.
    let sql = r#"SELECT CASE WHEN g IS NOT NULL
                      THEN json_build_array(ST_XMin(g), ST_YMin(g), ST_XMax(g), ST_YMax(g))::text
                      ELSE NULL END
               FROM (SELECT ST_Transform(ST_MakeEnvelope($1, $2, $3, $4, $5), $6) AS g) s"#;
    let row: Option<String> = Spi::get_one_with_args(
        sql,
        &[
            xmin.into(),
            ymin.into(),
            xmax.into(),
            ymax.into(),
            srid.into(),
            cfg.tile_srs.into(),
        ],
    )?;
    let Some(s) = row else {
        notice!(
            "geometry_pyramid: extent produced no valid envelope; pyramid '{}' unchanged",
            pyramid_name
        );
        return Ok(0);
    };
    let arr: [f64; 4] = serde_json::from_str(&s)?;
    rebuild_extent(
        pyramid_name, source_table, geom_column, fid_column, cfg,
        arr[0], arr[1], arr[2], arr[3],
    )
}
```

- [ ] **Step 3: 编译 + 全量测试**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: 36 passed(`update()` 行为不变,由现有 `incremental_update_rebuilds_only_dirty_tiles` 回归保护;`update_extent` 是 dead_code 警告)。

- [ ] **Step 4: Commit**
```bash
git add src/incremental.rs
git commit -m "refactor: extract rebuild_extent; add update_extent for range update"
```

---

### Task 2: pyramid.rs 新函数 + catalog_meta 复用 + 测试 + README

**Files:**
- Modify: `src/pyramid.rs`(抽 `catalog_meta`,改 `st_update_pyramid` 用它,新增 `st_update_pyramid_by_extent`)
- Modify: `src/lib.rs`(pg_test)
- Modify: `README.md`(SQL surface)

- [ ] **Step 1: 写失败的 pg_test**(`src/lib.rs` tests 模块)

```rust
/// Range update rebuilds the region from the CURRENT source, so it handles a
/// delete (which the where_clause path cannot). Tiles outside the bbox are
/// untouched; unknown pyramid → false.
#[pg_test]
fn update_by_extent_rebuilds_region() {
    Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
    Spi::run(
        "CREATE TABLE _gp_ext (gid serial PRIMARY KEY, geom geometry(Point, 4326))",
    )
    .unwrap();
    // A near (30,40), B near (-100,-30).
    Spi::run(
        "INSERT INTO _gp_ext (geom) VALUES \
         (ST_SetSRID(ST_MakePoint(30, 40), 4326)), \
         (ST_SetSRID(ST_MakePoint(-100, -30), 4326))",
    )
    .unwrap();
    Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_ext','geom','gid', \
         '{\"minZoom\":0,\"maxZoom\":4,\"splitSize\":0}')",
    )
    .unwrap();

    // z=0 is the whole world: it contains both A and B.
    let before = Spi::get_one::<Vec<u8>>("SELECT ST_Tile('_gp_ext', 0, 0, 0)")
        .unwrap()
        .expect("z0 tile exists");

    // Delete A, then range-update the region around A. (where_clause 'gid=1'
    // could not clean A's tile after the row is gone — range update can.)
    Spi::run("DELETE FROM _gp_ext WHERE ST_X(geom) = 30").unwrap();
    let ok = Spi::get_one_with_args::<bool>(
        "SELECT ST_UpdatePyramidByExtent('_gp_ext', $1, $2, $3, $4, 4326)",
        &[29.0_f64.into(), 39.0_f64.into(), 31.0_f64.into(), 41.0_f64.into()],
    )
    .unwrap();
    assert_eq!(ok, Some(true));

    // z=0 was re-sliced from current source (B only) → smaller than before (A+B).
    let after = Spi::get_one::<Vec<u8>>("SELECT ST_Tile('_gp_ext', 0, 0, 0)")
        .unwrap()
        .expect("z0 tile still exists (B remains)");
    assert!(
        after.len() < before.len(),
        "deleting A then range-updating its region must shrink the z0 tile"
    );

    // Unknown pyramid → false, no error.
    let unk = Spi::get_one_with_args::<bool>(
        "SELECT ST_UpdatePyramidByExtent('no_such_pyr', $1, $2, $3, $4, 4326)",
        &[0.0_f64.into(), 0.0_f64.into(), 1.0_f64.into(), 1.0_f64.into()],
    )
    .unwrap();
    assert_eq!(unk, Some(false));

    Spi::run("SELECT ST_DropPyramid('_gp_ext')").unwrap();
    Spi::run("DROP TABLE _gp_ext").unwrap();
}
```

- [ ] **Step 2: 运行确认失败**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: FAIL —— `function st_updatepyramidbyextent(...) does not exist`。

- [ ] **Step 3: 抽 `catalog_meta` 助手,改 `st_update_pyramid` 用它**

在 `src/pyramid.rs` 加一个私有助手(放在 `st_update_pyramid` 之前或 `validate_identifier` 附近):
```rust
/// Fetch + parse + validate a pyramid's catalog row. Returns None for an
/// unknown pyramid (MAX(...) avoids the pgrx 0-row error inside nested SPI).
/// Errors on a NULL catalog field or an identifier failing validation.
fn catalog_meta(
    name: &str,
) -> Result<Option<(String, String, String, PyramidConfig)>, Box<dyn std::error::Error + Send + Sync>>
{
    let row: Option<String> = Spi::get_one_with_args(
        "SELECT MAX(json_build_object( \
            'source_table', source_table, \
            'geom_column',  geom_column, \
            'fid_column',   fid_column, \
            'config',       config \
         )::text) \
         FROM geometry_pyramid_catalog WHERE pyramid_name = $1",
        &[name.into()],
    )?;
    let Some(json) = row else { return Ok(None) };
    let parsed: serde_json::Value = serde_json::from_str(&json)?;
    let source_table = parsed["source_table"]
        .as_str()
        .ok_or("catalog row has NULL source_table")?
        .to_string();
    let geom_column = parsed["geom_column"]
        .as_str()
        .ok_or("catalog row has NULL geom_column")?
        .to_string();
    let fid_column = parsed["fid_column"]
        .as_str()
        .ok_or("catalog row has NULL fid_column")?
        .to_string();
    let cfg: PyramidConfig = serde_json::from_value(parsed["config"].clone())?;
    validate_identifier(&source_table)?;
    validate_identifier(&geom_column)?;
    validate_identifier(&fid_column)?;
    Ok(Some((source_table, geom_column, fid_column, cfg)))
}
```

在 `st_update_pyramid` 中,把第 126–165 行(`validate_identifier(name)?;` 之后的整段 MAX 查询 + 解析 + 三个 `validate_identifier`)替换为:
```rust
    validate_identifier(name)?;
    let Some((source_table, geom_column, fid_column, cfg)) = catalog_meta(name)? else {
        return Ok(false);
    };
```
其余(`let total = if where_clause.trim().is_empty() { ... }`、UPDATE built_at、notice)保持不变。

- [ ] **Step 4: 新增 `st_update_pyramid_by_extent`**(放在 `st_update_pyramid` 之后)

```rust
/// `ST_UpdatePyramidByExtent(name, xmin, ymin, xmax, ymax, srid) -> bool`
///
/// Range update: rebuilds every tile intersecting the given bbox (interpreted
/// in `srid`) from the CURRENT source data, across minZoom..=maxZoom, in place
/// within the caller's transaction. Unlike the where_clause path it re-slices
/// the whole region, so it correctly reflects inserts, updates, deletes, and
/// moves there. Returns false for an unknown pyramid; true otherwise (including
/// a no-op when the bbox covers no tiles).
#[pg_extern(name = "st_updatepyramidbyextent")]
fn st_update_pyramid_by_extent(
    name: &str,
    xmin: f64,
    ymin: f64,
    xmax: f64,
    ymax: f64,
    srid: i32,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    validate_identifier(name)?;
    let Some((source_table, geom_column, fid_column, cfg)) = catalog_meta(name)? else {
        return Ok(false);
    };
    let total = crate::incremental::update_extent(
        name, &source_table, &geom_column, &fid_column, &cfg, xmin, ymin, xmax, ymax, srid,
    )?;
    Spi::run_with_args(
        "UPDATE geometry_pyramid_catalog SET built_at = now() WHERE pyramid_name = $1",
        &[name.into()],
    )?;
    notice!(
        "geometry_pyramid: pyramid '{}' range-refreshed ({} tiles)",
        name,
        total
    );
    Ok(true)
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: 37 passed(36 + 新测试)。`incremental_update_rebuilds_only_dirty_tiles` / `unknown_pyramid_returns_cleanly` 仍绿(`catalog_meta` 抽取不改行为)。
- 若 `after.len() < before.len()` 失败:确认 DELETE 删的是点 A、bbox 覆盖 z0(z0 整张图都覆盖),且 update_extent 用 $5=srid、$6=tile_srs 顺序正确。
- 若 `st_updatepyramidbyextent does not exist`:确认 `#[pg_extern(name="st_updatepyramidbyextent")]` 小写名,重装由测试框架自动完成。

- [ ] **Step 6: 更新 README**(SQL surface 表)

在 `ST_UpdatePyramid` 行之后加一行(对齐):
```
| `ST_UpdatePyramidByExtent(name, xmin, ymin, xmax, ymax, srid) -> bool` | Range update: re-slice every tile intersecting the bbox (in `srid`) from current source — correctly handles inserts/updates/deletes/moves in that region. |
```
并在 `ST_UpdatePyramid` 行的描述里(或附近)点一句:删除/移出场景可用 `ST_UpdatePyramidByExtent` 覆盖旧区域来清理。

- [ ] **Step 7: detect_changes + Commit**

Run: `detect_changes()`(MCP)。
```bash
git add src/pyramid.rs src/lib.rs README.md
git commit -m "feat: ST_UpdatePyramidByExtent — rebuild a pyramid region by bbox"
```

---

## 自审记录

- **规格覆盖**:`rebuild_extent` 抽取(T1S1)、`update_extent` bbox→tileSRS+复用(T1S2)、`catalog_meta` 复用(T2S3)、`st_updatepyramidbyextent`(T2S4)、删除语义测试(T2S1)、README(T2S6)——spec 各节均覆盖。srid 必填、未知金字塔→false、退化 bbox→no-op(`update_extent` 的 CASE NULL → Ok(0))、单事务复用均落实。
- **占位符**:无 TBD;每个改代码 Step 均有完整代码或精确 old→new。
- **一致性**:`rebuild_extent`/`update_extent` 签名在 T1 定义、T2 按签名调用;`catalog_meta` 返回 `(source,geom,fid,cfg)` 两个调用点一致;参数绑定 `$5=srid`/`$6=tile_srs` 与 spec 一致;新 SQL 名小写 `st_updatepyramidbyextent`。
- **耦合**:T1 抽取保持 `update()` 行为(回归测试护);T2 的 catalog_meta 抽取改了 `st_update_pyramid` 但纯机械、由 incremental + unknown_pyramid 测试覆盖。
- **测试诚实性**:删除场景用 z0 瓦片字节变小验证(z0 覆盖全图,删 A 后只剩 B → 更小),是可判定的确定性断言。
