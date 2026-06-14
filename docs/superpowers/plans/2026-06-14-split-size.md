# splitSize Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Ganos-style `splitSize`: at build time, tiles whose feature count is below `splitSize` are not materialized; `ST_Tile` serves those (within maxZoom) by on-demand slicing via the existing `dynamic::generate_tile`.

**Architecture:** One new config field (`splitSize`, default 10000); a `HAVING count(*) >= splitSize` threshold on the build INSERT (sub-threshold tiles skip materialization); and a fallback in `ST_Tile`'s z≤maxZoom branch — on a storage miss with `splitSize>0`, call `generate_tile`. Reuses the merged hybrid-ST_Tile pipeline (`tile_pipeline_from_sql` + `generate_tile`).

**Tech Stack:** Rust + pgrx 0.16 (`CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`), PostGIS (SPI).

**通用约定:**
- Test (exact): `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`. Check: `CARGO_TARGET_DIR=/tmp/gp_target cargo check --no-default-features --features pg17`. Baseline on main: 30 passed.
- Working on `main` (the prior features merged). Commit format `<type>: <description>`, no attribution footer.
- After symbol-changing tasks run `detect_changes()` before commit; docs/test-only may skip.
- Ignore gitnexus MCP reminders. lean-ctx read tools preferred; native Edit fine.

**关键耦合(实现者必读):** 默认 `splitSize=10000` 一旦被 build 应用(Task 3),会使现有 3 个直接查存储表的测试失败,因为它们的小数据集瓦片全部 < 10000、不再物化。Task 2 先给这 3 个测试钉 `"splitSize":0`(它们验证的是物化机制,需要"物化所有")。build 阈值(跳过物化)与 ST_Tile 回退(miss 时动态生成)是同一行为的两半,Task 3 必须同时落地,否则中间态会让所有"读 z≤maxZoom 瓦片"的测试失败。

---

### Task 1: 配置字段 `splitSize`(`src/config.rs`)

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: 写失败的单元测试**(`src/config.rs` 的 `#[cfg(test)] mod tests`)

```rust
#[test]
fn split_size_defaults_10000_and_parses() {
    assert_eq!(PyramidConfig::default().split_size, 10000);
    let c = PyramidConfig::parse(r#"{"splitSize":0}"#).unwrap();
    assert_eq!(c.split_size, 0);
    let c2 = PyramidConfig::parse(r#"{"splitSize":250}"#).unwrap();
    assert_eq!(c2.split_size, 250);
}
```

- [ ] **Step 2: 运行确认失败**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo check --no-default-features --features pg17`
Expected: 编译错误(`split_size` 字段不存在)。

- [ ] **Step 3: 加字段 + 默认**(读 src/config.rs 匹配风格/位置)

在 `struct PyramidConfig` 中,`max_tile_size_attempts` 字段之后加:
```rust
    /// Tiles whose feature count is below splitSize are NOT materialized at
    /// build time; ST_Tile slices them on-demand from the source (density
    /// boundary, mirrors Ganos). 0 disables (materialize every non-empty tile).
    #[serde(default = "default_split_size", rename = "splitSize")]
    pub split_size: u32,
```
在 `impl Default for PyramidConfig` 加(字段序对应):`split_size: 10000,`
在 `default_*` 函数群加:
```rust
fn default_split_size() -> u32 {
    10000
}
```

- [ ] **Step 4: 运行测试**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: 31 passed(30 + 新单测)。build 尚未使用该字段,故无行为变化。

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat: add splitSize config key (default 10000)"
```

---

### Task 2: 给依赖物化的现有测试钉 `splitSize:0`(`src/lib.rs`)

**Files:**
- Modify: `src/lib.rs`

这 3 个测试直接查存储表 `_gp_tiles_*` 或断言已物化瓦片,验证的是构建/并行/增量的**物化机制**。即将到来的默认 `splitSize=10000`(Task 3 生效)会让它们的小数据集不再物化。给它们的 `ST_BuildPyramid` config 显式加 `"splitSize":0`(= 物化所有非空,即这些测试一直假设的行为)。本任务现在是 no-op(build 还没用 split_size),但让测试对 Task 3 健壮。

- [ ] **Step 1: 编辑 `drop_rate_reduces_low_zoom_features`**

把其 `ST_BuildPyramid` 调用的 config JSON 从
`'{\"minZoom\":0,\"maxZoom\":4,\"dropRate\":0.4}'`
改为
`'{\"minZoom\":0,\"maxZoom\":4,\"dropRate\":0.4,\"splitSize\":0}'`

- [ ] **Step 2: 编辑 `parallel_build_matches_serial`**

把两处 `ST_BuildPyramid`(`par_serial` 与 `par_parallel`,以及通过 `exec_sql_autonomous` 建源表的不算)的 config 各加 `,\"splitSize\":0`:
- `'{\"name\":\"par_serial\",\"minZoom\":0,\"maxZoom\":5}'` → `'{\"name\":\"par_serial\",\"minZoom\":0,\"maxZoom\":5,\"splitSize\":0}'`
- `'{\"name\":\"par_parallel\",\"minZoom\":0,\"maxZoom\":5,\"parallel\":2,\"buildTimeoutMs\":60000}'` → 追加 `,\"splitSize\":0`

- [ ] **Step 3: 编辑 `incremental_update_rebuilds_only_dirty_tiles`**

把其首次 `ST_BuildPyramid` 的 config
`'{\"minZoom\":0,\"maxZoom\":6}'` → `'{\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0}'`

- [ ] **Step 4: 运行测试**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: 31 passed(no-op:build 尚未读 split_size,行为不变)。

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs
git commit -m "test: pin splitSize=0 in materialization-mechanics tests"
```

---

### Task 3: build 阈值 + ST_Tile 回退(`src/build.rs`, `src/tile.rs`, `src/lib.rs`)

**Files:**
- Modify: `src/build.rs`(INSERT 的 HAVING)
- Modify: `src/tile.rs`(z≤maxZoom miss 回退 + catalog_identifiers 助手)
- Modify: `src/lib.rs`(新 pg_test)

build 阈值与 tile 回退是同一行为的两半,必须同提交。

- [ ] **Step 1: 写失败的 pg_test**(`src/lib.rs` tests 模块)

```rust
/// splitSize defers sub-threshold (sparse) tiles: they are NOT materialized at
/// build time but ST_Tile still serves them by on-demand slicing. splitSize=0
/// materializes them as before.
#[pg_test]
fn split_size_defers_sparse_tiles() {
    Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
    Spi::run(
        "CREATE TABLE _gp_sp (gid serial PRIMARY KEY, geom geometry(Point, 4326))",
    )
    .unwrap();
    // 5 scattered points; at maxZoom=4 each lands in its own tile with 1 feature.
    Spi::run(
        "INSERT INTO _gp_sp (geom) VALUES \
         (ST_SetSRID(ST_MakePoint(30, 40), 4326)), \
         (ST_SetSRID(ST_MakePoint(-60, -20), 4326)), \
         (ST_SetSRID(ST_MakePoint(120, 10), 4326)), \
         (ST_SetSRID(ST_MakePoint(-120, 50), 4326)), \
         (ST_SetSRID(ST_MakePoint(80, -45), 4326))",
    )
    .unwrap();

    // Sparse pyramid: splitSize=100 ⇒ every tile (1 feature) is below threshold
    // ⇒ nothing materialized.
    Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_sp','geom','gid', \
         '{\"name\":\"sparse\",\"minZoom\":0,\"maxZoom\":4,\"splitSize\":100}')",
    )
    .unwrap();
    let materialized = Spi::get_one::<i64>(
        "SELECT count(*)::bigint FROM _gp_tiles_sparse",
    )
    .unwrap()
    .unwrap();
    assert_eq!(materialized, 0, "splitSize=100 must skip all sub-threshold tiles");

    // ST_Tile still serves a sparse tile on-demand. Compute the z=4 tile that
    // covers the (30,40) point.
    let tx = Spi::get_one::<i32>(
        "SELECT floor((ST_X(ST_Transform(ST_SetSRID(ST_MakePoint(30,40),4326),3857)) \
                + 20037508.342789244) / (2*20037508.342789244) * pow(2,4))::int",
    )
    .unwrap()
    .unwrap();
    let ty = Spi::get_one::<i32>(
        "SELECT floor((20037508.342789244 \
                - ST_Y(ST_Transform(ST_SetSRID(ST_MakePoint(30,40),4326),3857))) \
                / (2*20037508.342789244) * pow(2,4))::int",
    )
    .unwrap()
    .unwrap();
    let sparse_hit = Spi::get_one_with_args::<Vec<u8>>(
        "SELECT ST_Tile('sparse', 4, $1, $2)",
        &[tx.into(), ty.into()],
    )
    .unwrap();
    assert!(sparse_hit.is_some(), "sparse tile must be sliced on-demand within maxZoom");
    assert!(!sparse_hit.unwrap().is_empty());

    // An empty region tile → None even with splitSize on.
    let far_x = if tx > 0 { 0 } else { 15 };
    let empty = Spi::get_one_with_args::<Vec<u8>>(
        "SELECT ST_Tile('sparse', 4, $1, $2)",
        &[far_x.into(), ty.into()],
    )
    .unwrap();
    assert!(empty.is_none(), "empty region must return NULL");

    // Control: splitSize=0 materializes the same tiles.
    Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_sp','geom','gid', \
         '{\"name\":\"dense\",\"minZoom\":0,\"maxZoom\":4,\"splitSize\":0}')",
    )
    .unwrap();
    let dense_materialized = Spi::get_one::<i64>(
        "SELECT count(*)::bigint FROM _gp_tiles_dense",
    )
    .unwrap()
    .unwrap();
    assert!(dense_materialized > 0, "splitSize=0 must materialize tiles");
    let dense_hit = Spi::get_one_with_args::<Vec<u8>>(
        "SELECT ST_Tile('dense', 4, $1, $2)",
        &[tx.into(), ty.into()],
    )
    .unwrap();
    assert!(dense_hit.is_some(), "materialized tile must be served from storage");

    Spi::run("SELECT ST_DropPyramid('sparse')").unwrap();
    Spi::run("SELECT ST_DropPyramid('dense')").unwrap();
    Spi::run("DROP TABLE _gp_sp").unwrap();
}
```

- [ ] **Step 2: 运行确认失败**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: FAIL — build 还没应用阈值,`_gp_tiles_sparse` 会物化(materialized != 0),`assert_eq!(materialized, 0)` 失败。

- [ ] **Step 3: build.rs 加 HAVING 阈值**

读 src/build.rs 的 `build_zoom_level`(约 503–514 行的 INSERT `format!`)。在该 `format!` 之前,函数体内加一行取出阈值:
```rust
    let split_size = cfg.split_size;
```
把 INSERT 的 HAVING 行
```rust
           HAVING ST_AsMVT(features.*, $2, $3, 'geom', 'fid') IS NOT NULL"#
```
改为
```rust
           HAVING count(*) >= {split_size} AND ST_AsMVT(features.*, $2, $3, 'geom', 'fid') IS NOT NULL"#
```
`split_size` 是 `u32`,通过 `format!` 命名捕获内联(安全:整数,非用户字符串)。`split_size=0` 时 `count(*) >= 0` 恒真,退化为原行为。

- [ ] **Step 4: tile.rs 加 miss 回退 + catalog_identifiers 助手**

读 src/tile.rs。在 `st_tile` 函数之后(或文件末尾)加一个 `pub(crate)` 之外的私有助手:
```rust
/// Extract the source-table / geom / fid identifiers from a parsed catalog row,
/// erroring clearly if any is NULL. Shared by the materialized-miss fallback and
/// the beyond-maxZoom dynamic path.
fn catalog_identifiers(
    parsed: &serde_json::Value,
) -> Result<(String, String, String), Box<dyn std::error::Error + Send + Sync>> {
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
    Ok((source_table, geom_column, fid_column))
}
```

把 `st_tile` 的 `z ≤ maxZoom` 分支(当前 64–80 行)整体替换为:
```rust
    if zu <= cfg.max_zoom {
        // Materialized range: read the storage table. Guard its existence so a
        // registered-but-unbuilt pyramid is treated as a miss, not an error.
        let table = storage::tile_table_name(name);
        let reg: Option<String> = Spi::get_one_with_args(
            "SELECT to_regclass($1)::text",
            &[format!("\"{table}\"").into()],
        )?;
        let hit: Option<Vec<u8>> = if reg.is_some() {
            Spi::get_one_with_args(
                &format!(r#"SELECT mvt FROM "{table}" WHERE z = $1 AND x = $2 AND y = $3"#),
                &[(z as i16).into(), x.into(), y.into()],
            )?
        } else {
            None
        };
        if hit.is_some() {
            return Ok(hit);
        }
        // Storage miss within the materialized range. With splitSize > 0 the tile
        // may be a deferred sparse tile (or genuinely empty) — slice on-demand;
        // an empty region yields None. With splitSize == 0 a miss means empty.
        if cfg.split_size == 0 {
            return Ok(None);
        }
        let (source_table, geom_column, fid_column) = catalog_identifiers(&parsed)?;
        return crate::dynamic::generate_tile(
            name, &source_table, &geom_column, &fid_column, &cfg, zu, x, y,
        );
    }
```

把 `z > maxZoom` 分支里现有的三段 `parsed["..."].as_str().ok_or(...)` 提取替换为复用助手:
```rust
    if !cfg.dynamic_beyond_max_zoom {
        return Ok(None);
    }
    let (source_table, geom_column, fid_column) = catalog_identifiers(&parsed)?;
    crate::dynamic::generate_tile(name, &source_table, &geom_column, &fid_column, &cfg, zu, x, y)
```

- [ ] **Step 5: 运行测试确认通过**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: 32 passed(31 + 新 pg_test)。既有读瓦片测试(build_fetch_drop_roundtrip 等,默认 splitSize=10000)现在经 ST_Tile 回退返回动态瓦片,仍通过;3 个钉 splitSize:0 的测试走物化路径通过。

若 `split_size_defers_sparse_tiles` 的 `materialized == 0` 仍失败:确认 HAVING 用的是 `count(*)`(features 组的行数)且 split_size 正确内联。若 `sparse_hit` 为 None:确认 tile.rs miss 分支在 split_size>0 时确实调 generate_tile(而非提前 return None);用 systematic-debugging。

- [ ] **Step 6: detect_changes + Commit**

```bash
git add src/build.rs src/tile.rs src/lib.rs
git commit -m "feat: splitSize defers sparse tiles to on-demand slicing"
```

---

### Task 4: 文档(`README.md`)

**Files:**
- Modify: `README.md`

- [ ] **Step 1: 更新 README**

1. config 表加一行(对齐):
   `| `splitSize` | `10000` | Tiles with fewer than this many features are not materialized at build time; `ST_Tile` slices them on-demand from the source. 0 materializes every non-empty tile. |`
2. `## Status` 段补一句:稀疏瓦片(要素数 < `splitSize`,默认 10000)不在构建时物化,由 `ST_Tile` 按需切片——结合 `maxZoom` 层级边界,物化只覆盖密集的中低 zoom 瓦片。
3. 若 Roadmap 有 `splitSize`/密度动态相关项,用 `~~...~~ ✅` 标记;否则加一条已完成说明:`splitSize` 密度边界已实现(`buildRules` 逐层规则仍为未来工作)。

- [ ] **Step 2: 全量回归**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: 32 passed。

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: document splitSize density-based dynamic tiling"
```

---

## 自审记录

- **规格覆盖**:config 字段(Task 1)、build HAVING 阈值(Task 3)、ST_Tile miss 回退(Task 3)、边界(Task 3 测试)、README(Task 4)——spec 各节均有对应任务。默认 10000、计数口径(count(*) over features = 过滤后要素数)、splitSize=0 退化、门控独立(z>maxZoom 仍走 dynamicBeyondMaxZoom 不变)均落到 Task 1/3。
- **耦合处理**:Task 2 显式钉 splitSize:0 于 3 个查存储表的测试,使其对 Task 3 默认变更健壮;build 阈值与 tile 回退在 Task 3 同提交,无破碎中间态。
- **类型一致**:`split_size: u32`(config)在 build.rs 内联进 SQL、在 tile.rs 比较 `== 0`;`catalog_identifiers` 返回 `(String,String,String)`,两个调用点签名一致;复用既有 `dynamic::generate_tile`(签名不变)。
- **占位符**:无 TBD/TODO;每个改代码 Step 均含完整代码或精确替换。
