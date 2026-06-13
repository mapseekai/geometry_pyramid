# 混合 ST_Tile 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `ST_Tile(name,z,x,y)` 在 `z > maxZoom` 时(开关开启)用与构建相同的 PostGIS 管线对单瓦片实时切片返回 MVT,物化范围内仍查存储表;未知金字塔返回 NULL 而非报错。

**Architecture:** 把 `build_zoom_level` 里"瓦片来源 + 特征管线"的 SQL 抽成共享构造器(单瓦片 = 1×1 generate_series 范围,与多瓦片同一段 SQL、同一套 14 参数绑定);新增 `dynamic.rs` 的 `generate_tile` 用它做单瓦片 SELECT;`ST_Tile` 改为 catalog-first 的混合控制流。

**Tech Stack:** Rust + pgrx 0.16(`CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`),PostGIS(SPI 委托)。

**通用约定:**
- 测试命令:`CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`;编译检查:`CARGO_TARGET_DIR=/tmp/gp_target cargo check --no-default-features --features pg17`。
- 基线当前 28 passed。
- 提交信息 `<type>: <description>`,无 attribution 尾注。
- 每个 Task 结束后(改了符号的)跑 `detect_changes()` 再提交;文档/纯新增可跳过。
- 忽略 gitnexus MCP 提示(controller 处理)。lean-ctx 工具优先;native Edit 可用。

---

### Task 1: 抽取共享 SQL 管线(`src/build.rs`)

**Files:**
- Modify: `src/build.rs`(`build_zoom_level` 内联 SQL → 调用两个新 `pub(crate)` 构造器)

纯重构,行为不变,由现有 28 个测试(尤其 `build_fetch_drop_roundtrip` / `drop_rate_reduces_low_zoom_features` / 并行一致性)保护。

- [ ] **Step 1: 加两个构造器函数**(放在 `build_zoom_level` 之前)

```rust
/// Attribute projection fragments for the feature pipeline: (inner, outer).
/// Column names are revalidated defensively because they ride into dynamic SQL.
pub(crate) fn attribute_projections(cfg: &PyramidConfig) -> (String, String) {
    let mut inner = String::new();
    let mut outer = String::new();
    for col in &cfg.attributes {
        if !col.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        inner.push_str(&format!(r#", src."{col}" AS "{col}""#));
        outer.push_str(&format!(r#", prep."{col}""#));
    }
    (inner, outer)
}

/// The shared `FROM … features` block of the MVT pipeline: a 2-D tile grid
/// (`generate_series`, params $4..$7) joined to the per-tile feature pipeline
/// (projection, per-zoom simplification, min-size + point-drop filters,
/// `ST_AsMVTGeom`). References params $1,$3,$8,$9,$10,$11..$14 and tile coords
/// t.x/t.y — never $2 (layer name) and no GROUP BY. Both the multi-tile build
/// INSERT and the single-tile dynamic SELECT wrap this with
/// `ST_AsMVT(features.*, $2, $3, 'geom','fid')` + `GROUP BY t.x, t.y`.
/// A single tile is just the degenerate range xmin=xmax / ymin=ymax.
pub(crate) fn tile_pipeline_from_sql(
    table: &str,
    geom_column: &str,
    fid_column: &str,
    attrs_inner: &str,
    attrs_outer: &str,
) -> String {
    format!(
        r#"FROM generate_series($4::int, $5::int) AS xs(x)
           CROSS JOIN generate_series($6::int, $7::int) AS ys(y)
           CROSS JOIN LATERAL (SELECT xs.x AS x, ys.y AS y) t
           CROSS JOIN LATERAL (
              SELECT
                 ST_AsMVTGeom(prep.gtile,
                              ST_TileEnvelope($1, t.x, t.y),
                              $3, $9, true) AS geom,
                 prep.fid
                 {attrs_outer}
              FROM (
                 SELECT
                    CASE
                       WHEN $11::float8 > 0
                            AND GeometryType(src."{geom_column}") NOT IN ('POINT','MULTIPOINT')
                          THEN ST_SimplifyPreserveTopology(
                                 ST_Transform(src."{geom_column}", $8),
                                 $11::float8)
                       ELSE ST_Transform(src."{geom_column}", $8)
                    END AS gtile,
                    src."{fid_column}"::bigint AS fid
                    {attrs_inner}
                 FROM "{table}" src
                 WHERE src."{geom_column}" IS NOT NULL
                   AND ST_Intersects(
                          src."{geom_column}",
                          ST_Transform(ST_TileEnvelope($1, t.x, t.y), $10)
                       )
              ) prep
              WHERE
                 ($12::float8 <= 0
                  OR GeometryType(prep.gtile) NOT IN ('POLYGON','MULTIPOLYGON')
                  OR ST_Area(prep.gtile) >= $12::float8)
                 AND ($13::float8 <= 0
                      OR GeometryType(prep.gtile) NOT IN ('LINESTRING','MULTILINESTRING')
                      OR ST_Length(prep.gtile) >= $13::float8)
                 AND ($14::float8 >= 1.0
                      OR GeometryType(prep.gtile) NOT IN ('POINT','MULTIPOINT')
                      OR (abs(hashtextextended(prep.fid::text, 0::bigint)) % 1000000)::float8
                         / 1000000.0 < $14::float8)
           ) features
           WHERE features.geom IS NOT NULL"#
    )
}
```

- [ ] **Step 2: 用构造器重写 `build_zoom_level` 的 SQL 组装**

在 `build_zoom_level` 内,把现有的 `let mut attrs_inner = ...` 循环替换为:

```rust
    let (attrs_inner, attrs_outer) = attribute_projections(cfg);
```

并把 `let sql = format!(r#"INSERT INTO ... HAVING ..."#);` 整段替换为:

```rust
    let pipeline = tile_pipeline_from_sql(table, geom_column, fid_column, &attrs_inner, &attrs_outer);
    let sql = format!(
        r#"INSERT INTO "{storage_table}" (z, x, y, mvt)
           SELECT $1::smallint, t.x, t.y, ST_AsMVT(features.*, $2, $3, 'geom', 'fid')
           {pipeline}
           GROUP BY t.x, t.y
           HAVING ST_AsMVT(features.*, $2, $3, 'geom', 'fid') IS NOT NULL"#
    );
```

`inserted_before` / `Spi::run_with_args(&sql, &[...14 args...])` / `inserted_after` 全部不变(14 参数绑定顺序与原来完全一致)。

- [ ] **Step 3: 编译 + 全量测试**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: 28 passed, 0 failed(行为不变)。

- [ ] **Step 4: detect_changes + Commit**

```bash
git add src/build.rs
git commit -m "refactor: extract shared tile-pipeline SQL builder from build_zoom_level"
```

---

### Task 2: 新增 `dynamicBeyondMaxZoom` 配置键(`src/config.rs`)

**Files:**
- Modify: `src/config.rs`(字段 + 默认 fn + Default + 单元测试)

- [ ] **Step 1: 写失败的单元测试**(`src/config.rs` 的 `#[cfg(test)] mod tests`)

```rust
#[test]
fn dynamic_beyond_max_zoom_defaults_true_and_parses() {
    let def = PyramidConfig::default();
    assert!(def.dynamic_beyond_max_zoom);
    let off = PyramidConfig::parse(r#"{"dynamicBeyondMaxZoom":false}"#).unwrap();
    assert!(!off.dynamic_beyond_max_zoom);
}
```

- [ ] **Step 2: 运行确认失败**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo check --no-default-features --features pg17`
Expected: 编译错误(`dynamic_beyond_max_zoom` 字段不存在)。

- [ ] **Step 3: 加字段 + 默认**

在 `PyramidConfig` 结构体里(放在 `max_tile_size_attempts` 或 `build_timeout_ms` 字段之后)加:

```rust
    /// When a requested tile has z > maxZoom, slice it live from the source
    /// table instead of returning NULL. Default true (mirrors Ganos maxLevel).
    #[serde(default = "default_dynamic_beyond_max_zoom", rename = "dynamicBeyondMaxZoom")]
    pub dynamic_beyond_max_zoom: bool,
```

在 `impl Default for PyramidConfig` 里加一行:`dynamic_beyond_max_zoom: true,`(放在对应位置)。

在文件的 `default_*` 函数群里加:

```rust
fn default_dynamic_beyond_max_zoom() -> bool {
    true
}
```

- [ ] **Step 4: 运行测试**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: 全部 PASS(29:28 + 新单测)。

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat: add dynamicBeyondMaxZoom config key (default true)"
```

---

### Task 3: 单瓦片实时切片(`src/dynamic.rs`)

**Files:**
- Create: `src/dynamic.rs`
- Modify: `src/lib.rs`(`mod dynamic;`)

`generate_tile` 依赖 SPI,只能由 Task 4 的 pg_test 覆盖;本 Task 只需编译通过(接入前是 dead code,预期 dead_code 警告,不加 `#[allow]`)。

- [ ] **Step 1: 实现 `src/dynamic.rs`**

```rust
//! On-demand single-tile slicing. When ST_Tile requests a zoom above the
//! pyramid's materialized ceiling (maxZoom), the tile is sliced live from the
//! source table using the *same* MVT pipeline as the build path
//! (crate::build::tile_pipeline_from_sql), so tiles are visually consistent
//! across the materialized/dynamic boundary. Read-only; nothing is cached.

use crate::config::PyramidConfig;
use pgrx::prelude::*;

/// Slice a single (z, x, y) tile from the source table and return its MVT
/// bytes, or None when the source has no features there (or the source table
/// is missing). `source_table` / `geom_column` / `fid_column` must be
/// validated identifiers (callers pass catalog values re-validated upstream).
#[allow(clippy::too_many_arguments)]
pub(crate) fn generate_tile(
    pyramid_name: &str,
    source_table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
    z: u8,
    x: i32,
    y: i32,
) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>> {
    // Guard against a stale catalog pointing at a dropped source table:
    // to_regclass returns NULL (not an error) for a missing relation.
    let reg: Option<String> = Spi::get_one_with_args(
        "SELECT to_regclass($1)::text",
        &[format!("\"{source_table}\"").into()],
    )?;
    if reg.is_none() {
        return Ok(None);
    }

    let knobs = crate::build::ZoomKnobs::from_config(cfg, z).to_values();
    let (attrs_inner, attrs_outer) = crate::build::attribute_projections(cfg);
    let pipeline = crate::build::tile_pipeline_from_sql(
        source_table,
        geom_column,
        fid_column,
        &attrs_inner,
        &attrs_outer,
    );

    // Single tile = degenerate 1x1 range: xmin=xmax=x ($4,$5), ymin=ymax=y
    // ($6,$7). Same 14-param binding as build_zoom_level.
    let sql = format!(
        r#"SELECT ST_AsMVT(features.*, $2, $3, 'geom', 'fid')
           {pipeline}
           GROUP BY t.x, t.y"#
    );

    let bytes: Option<Vec<u8>> = Spi::get_one_with_args(
        &sql,
        &[
            (z as i32).into(),
            pyramid_name.into(),
            (cfg.tile_size as i32).into(),
            x.into(),
            x.into(),
            y.into(),
            y.into(),
            cfg.tile_srs.into(),
            (cfg.tile_extend as i32).into(),
            cfg.source_srs.into(),
            knobs.simplify_tol_m.into(),
            knobs.min_area_m2.into(),
            knobs.min_len_m.into(),
            knobs.drop_threshold.into(),
        ],
    )?;
    Ok(bytes)
}
```

`src/lib.rs` 加 `mod dynamic;`(在 mod 列表里,如 `mod config;` 之后)。

注意:`crate::build::ZoomKnobs` 与 `from_config` / `to_values` 已是 `pub(crate)`(Task 7 阶段放开);`attribute_projections` / `tile_pipeline_from_sql` 由本计划 Task 1 设为 `pub(crate)`。若 `cargo check` 报可见性问题,确认 Task 1 的两个函数确为 `pub(crate)`。

- [ ] **Step 2: 编译检查**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo check --no-default-features --features pg17`
Expected: 0 errors(允许 `generate_tile` 的 dead_code 警告)。

- [ ] **Step 3: 全量测试(确认未破坏)**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: 29 passed。

- [ ] **Step 4: Commit**

```bash
git add src/dynamic.rs src/lib.rs
git commit -m "feat: on-demand single-tile slicing via shared MVT pipeline"
```

---

### Task 4: `ST_Tile` 改混合控制流(`src/tile.rs`)+ 集成测试

**Files:**
- Modify: `src/tile.rs`(`st_tile` 重写)
- Modify: `src/lib.rs`(新 pg_test)

- [ ] **Step 1: 写失败的 pg_test**(`src/lib.rs` tests 模块)

```rust
/// ST_Tile serves materialized tiles within maxZoom, slices on-demand above it
/// (when enabled), returns NULL for unknown pyramids, and respects the
/// dynamicBeyondMaxZoom switch.
#[pg_test]
fn dynamic_tile_beyond_max_zoom() {
    Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
    Spi::run(
        "CREATE TABLE _gp_dyn (gid serial PRIMARY KEY, geom geometry(Point, 4326))",
    )
    .unwrap();
    // A point well inside one tile (avoids tile-boundary ambiguity).
    Spi::run("INSERT INTO _gp_dyn (geom) VALUES (ST_SetSRID(ST_MakePoint(30, 40), 4326))")
        .unwrap();

    // Pyramid materialized only to z=4; dynamic default ON.
    Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_dyn','geom','gid','{\"name\":\"dyn\",\"minZoom\":0,\"maxZoom\":4}')",
    )
    .unwrap();

    // Unknown pyramid → NULL (regression: used to error on missing table).
    let unknown = Spi::get_one::<Vec<u8>>("SELECT ST_Tile('no_such_pyramid', 0, 0, 0)").unwrap();
    assert!(unknown.is_none(), "unknown pyramid must return NULL, not error");

    // Materialized hit: z=0 is the whole world, the point is in tile (0,0,0).
    let z0 = Spi::get_one::<Vec<u8>>("SELECT ST_Tile('dyn', 0, 0, 0)").unwrap();
    assert!(z0.is_some(), "materialized z=0 tile must exist");

    // Compute the z=6 tile that contains the point (z=6 > maxZoom=4 ⇒ dynamic).
    let tx = Spi::get_one::<i32>(
        "SELECT floor((ST_X(ST_Transform(geom,3857)) + 20037508.342789244) \
                / (2*20037508.342789244) * pow(2,6))::int FROM _gp_dyn LIMIT 1",
    )
    .unwrap()
    .unwrap();
    let ty = Spi::get_one::<i32>(
        "SELECT floor((20037508.342789244 - ST_Y(ST_Transform(geom,3857))) \
                / (2*20037508.342789244) * pow(2,6))::int FROM _gp_dyn LIMIT 1",
    )
    .unwrap()
    .unwrap();

    // Dynamic hit: the tile covering the point at z=6 must be sliced live.
    let dyn_hit = Spi::get_one_with_args::<Vec<u8>>(
        "SELECT ST_Tile('dyn', 6, $1, $2)",
        &[tx.into(), ty.into()],
    )
    .unwrap();
    assert!(dyn_hit.is_some(), "z=6 tile over the point must be sliced on-demand");
    assert!(!dyn_hit.unwrap().is_empty());

    // Dynamic miss: a z=6 tile far from the point has no data → NULL.
    let far_x = if tx > 0 { 0 } else { 63 };
    let dyn_miss = Spi::get_one_with_args::<Vec<u8>>(
        "SELECT ST_Tile('dyn', 6, $1, $2)",
        &[far_x.into(), ty.into()],
    )
    .unwrap();
    assert!(dyn_miss.is_none(), "empty region at z=6 must return NULL");

    // Switch OFF: rebuild with dynamicBeyondMaxZoom=false ⇒ z=6 returns NULL.
    Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_dyn','geom','gid', \
         '{\"name\":\"dynoff\",\"minZoom\":0,\"maxZoom\":4,\"dynamicBeyondMaxZoom\":false}')",
    )
    .unwrap();
    let off = Spi::get_one_with_args::<Vec<u8>>(
        "SELECT ST_Tile('dynoff', 6, $1, $2)",
        &[tx.into(), ty.into()],
    )
    .unwrap();
    assert!(off.is_none(), "dynamicBeyondMaxZoom=false must return NULL beyond maxZoom");

    Spi::run("SELECT ST_DropPyramid('dyn')").unwrap();
    Spi::run("SELECT ST_DropPyramid('dynoff')").unwrap();
    Spi::run("DROP TABLE _gp_dyn").unwrap();
}
```

- [ ] **Step 2: 运行确认失败**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: FAIL — 当前 `st_tile` 对未知/未物化的瓦片直接查 `_gp_tiles_dyn`,`z=6` 不在表中返回 None(可能恰好让部分断言过),但 `unknown_pyramid` 查 `_gp_tiles_no_such_pyramid` 会因表不存在而 ERROR(longjmp),测试在 `unknown` 断言处即失败/报错。记录观察到的失败点。

- [ ] **Step 3: 重写 `src/tile.rs` 的 `st_tile`**

把整个 `st_tile` 函数体替换为:

```rust
#[pg_extern(name = "st_tile")]
fn st_tile(
    name: &str,
    z: i32,
    x: i32,
    y: i32,
) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>> {
    crate::pyramid::validate_identifier(name)?;
    if !(0..=24).contains(&z) {
        return Err(format!("zoom level {z} out of range").into());
    }
    let n = 1i32 << z;
    if !(0..n).contains(&x) || !(0..n).contains(&y) {
        return Ok(None);
    }

    // Catalog-first: an unknown pyramid yields NULL (never an error), and the
    // config tells us where the materialized/dynamic boundary is.
    let row: Option<String> = Spi::get_one_with_args(
        "SELECT json_build_object( \
            'source_table', source_table, \
            'geom_column',  geom_column, \
            'fid_column',   fid_column, \
            'config',       config \
         )::text \
         FROM geometry_pyramid_catalog WHERE pyramid_name = $1",
        &[name.into()],
    )?;
    let Some(json) = row else { return Ok(None) };
    let parsed: serde_json::Value = serde_json::from_str(&json)?;
    let cfg: crate::config::PyramidConfig =
        serde_json::from_value(parsed["config"].clone())?;

    let zu = z as u8;
    if zu < cfg.min_zoom {
        return Ok(None);
    }

    if zu <= cfg.max_zoom {
        // Materialized range: read the storage table. Guard its existence so a
        // registered-but-unbuilt pyramid yields NULL instead of erroring.
        let table = storage::tile_table_name(name);
        let reg: Option<String> = Spi::get_one_with_args(
            "SELECT to_regclass($1)::text",
            &[format!("\"{table}\"").into()],
        )?;
        if reg.is_none() {
            return Ok(None);
        }
        let bytes: Option<Vec<u8>> = Spi::get_one_with_args(
            &format!(r#"SELECT mvt FROM "{table}" WHERE z = $1 AND x = $2 AND y = $3"#),
            &[(z as i16).into(), x.into(), y.into()],
        )?;
        return Ok(bytes);
    }

    // z > maxZoom: on-demand slice from the source, if enabled.
    if !cfg.dynamic_beyond_max_zoom {
        return Ok(None);
    }
    let source_table = parsed["source_table"].as_str().unwrap_or("").to_string();
    let geom_column = parsed["geom_column"].as_str().unwrap_or("").to_string();
    let fid_column = parsed["fid_column"].as_str().unwrap_or("").to_string();
    // Catalog values were validated at build time; re-validate before they
    // ride into dynamic SQL (defense-in-depth, matches ST_UpdatePyramid).
    crate::pyramid::validate_identifier(&source_table)?;
    crate::pyramid::validate_identifier(&geom_column)?;
    crate::pyramid::validate_identifier(&fid_column)?;
    crate::dynamic::generate_tile(name, &source_table, &geom_column, &fid_column, &cfg, zu, x, y)
}
```

确认 `src/tile.rs` 顶部已有 `use crate::storage;` 和 `use pgrx::prelude::*;`(原文件就有)。`validate_identifier` 是 `pub(crate)`。

- [ ] **Step 4: 运行测试确认通过**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: 全部 PASS(30:29 + 新 pg_test)。

- [ ] **Step 5: detect_changes + Commit**

```bash
git add src/tile.rs src/lib.rs
git commit -m "feat: hybrid ST_Tile — materialized lookup with on-demand slicing beyond maxZoom"
```

---

### Task 5: 文档收尾(`README.md`)

**Files:**
- Modify: `README.md`

- [ ] **Step 1: 更新 README**

1. `## Status` 段:补一句 `ST_Tile` 现在是混合行为——`z ≤ maxZoom` 查物化瓦片,`z > maxZoom` 时(`dynamicBeyondMaxZoom` 默认开)从源表实时切片;未知金字塔返回 NULL。
2. config 表加一行:`| `dynamicBeyondMaxZoom` | `true` | 当请求 z > maxZoom 时从源表实时切片(而非返回 NULL),对齐 Ganos maxLevel 语义。设为 false 则超出 maxZoom 返回 NULL。 |`
3. `ST_Tile` 行 Purpose 更新为:`Fetch an MVT tile: materialized within maxZoom, sliced on-demand beyond it (NULL when absent/disabled).`
4. Project layout 树加:`├── dynamic.rs              # 超 maxZoom 的单瓦片实时切片`。

- [ ] **Step 2: 全量回归**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: 30 passed。

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: document hybrid ST_Tile and dynamicBeyondMaxZoom"
```

---

## 自审记录

- **规格覆盖**:SQL 抽取(Task 1)、配置开关(Task 2)、单瓦片切片(Task 3)、混合 ST_Tile + 全部边界 pg_test(Task 4)、README(Task 5)——spec 各节均有对应任务。catalog-first、z<minZoom→None、to_regclass 守卫、未知金字塔→None、开关默认 true 均落到 Task 4/2。
- **类型一致性**:`tile_pipeline_from_sql` / `attribute_projections`(Task 1,pub(crate))↔ `dynamic::generate_tile`(Task 3 调用)签名一致;14 参数绑定顺序在 build 与 dynamic 完全相同(dynamic 仅令 $4=$5=x、$6=$7=y);`ZoomKnobs::from_config/to_values` 复用既有 pub(crate)。
- **占位符**:无 TBD/TODO;每个改代码的 Step 都有完整代码或精确替换指令。
- **边界一致性说明**:spec 第 3 节提到的"z=maxZoom vs maxZoom+1 都能解码"轻量断言,在 Task 4 测试里以"z=0 物化命中 + z=6 动态命中均 is_some()"等价覆盖(两侧都产出有效 MVT)。
