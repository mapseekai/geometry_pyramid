# 瓦片表 id/format/srid 列 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给瓦片表 `_gp_tiles_<name>` 加 `id`(bigserial 序号)、`format`(恒 'pbf')、`srid`(= tileSRS)三列。

**Architecture:** schema 单一源 `storage.rs:tile_table_create_sql` 加三列;`build.rs:build_zoom_level` 的 INSERT 列表与 SELECT 同步加 `format`/`srid`(复用已绑定参数 `$8`=tile_srs),`id` 由 bigserial 自动填充。`ST_Tile` 不变。schema 与 INSERT 是 NOT NULL 耦合,同一任务/提交完成。

**Tech Stack:** Rust + pgrx 0.16(`CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`),PostGIS。

**通用约定:**
- 在 `main` 分支(实现时应已切到专用分支由执行流程负责)。Test:`CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`。基线 32 passed。
- Commit `<type>: <description>`,无 attribution。忽略 gitnexus MCP 提示。不要 push。

---

### Task 1: 加三列(schema + INSERT)+ pg_test

**Files:**
- Modify: `src/storage.rs`(`tile_table_create_sql`)
- Modify: `src/build.rs`(`build_zoom_level` 的 INSERT)
- Modify: `src/lib.rs`(新 pg_test)

schema 的 `format`/`srid` 是 NOT NULL,若只改 schema 不改 INSERT,现有 `(z,x,y,mvt)` 插入会因缺列报错——故二者同任务、同提交。

- [ ] **Step 1: 写失败的 pg_test**(`src/lib.rs` tests 模块)

```rust
/// The tile table carries id (serial), format ('pbf'), and srid (= tileSRS).
#[pg_test]
fn tile_table_has_id_format_srid() {
    Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
    Spi::run(
        "CREATE TABLE _gp_cols (gid serial PRIMARY KEY, geom geometry(Point, 4326))",
    )
    .unwrap();
    Spi::run("INSERT INTO _gp_cols (geom) VALUES (ST_SetSRID(ST_MakePoint(0,0),4326))")
        .unwrap();
    // splitSize:0 forces materialization so the storage table has rows.
    Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_cols','geom','gid', \
         '{\"minZoom\":0,\"maxZoom\":2,\"splitSize\":0}')",
    )
    .unwrap();

    let format = Spi::get_one::<String>(
        "SELECT format FROM _gp_tiles__gp_cols LIMIT 1",
    )
    .unwrap()
    .unwrap();
    assert_eq!(format, "pbf", "format column must be 'pbf'");

    let srid = Spi::get_one::<i32>("SELECT srid FROM _gp_tiles__gp_cols LIMIT 1")
        .unwrap()
        .unwrap();
    assert_eq!(srid, 3857, "srid column must equal the default tileSRS (3857)");

    let null_ids = Spi::get_one::<i64>(
        "SELECT count(*)::bigint FROM _gp_tiles__gp_cols WHERE id IS NULL",
    )
    .unwrap()
    .unwrap();
    assert_eq!(null_ids, 0, "id column must be populated by bigserial");

    Spi::run("SELECT ST_DropPyramid('_gp_cols')").unwrap();
    Spi::run("DROP TABLE _gp_cols").unwrap();
}
```

- [ ] **Step 2: 运行确认失败**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: FAIL — `column "format" does not exist`(schema 还没这列)。

- [ ] **Step 3: 改 schema**（`src/storage.rs`，`tile_table_create_sql`）

把
```rust
    format!(
        r#"CREATE TABLE "{table}" (
            z smallint NOT NULL,
            x integer  NOT NULL,
            y integer  NOT NULL,
            mvt bytea  NOT NULL,
            PRIMARY KEY (z, x, y)
        )"#
    )
```
改为
```rust
    format!(
        r#"CREATE TABLE "{table}" (
            id     bigserial,
            z      smallint NOT NULL,
            x      integer  NOT NULL,
            y      integer  NOT NULL,
            format text     NOT NULL,
            srid   integer  NOT NULL,
            mvt    bytea    NOT NULL,
            PRIMARY KEY (z, x, y)
        )"#
    )
```

- [ ] **Step 4: 改 INSERT**（`src/build.rs`,`build_zoom_level`）

找到 INSERT 的 `format!`(列表 `(z, x, y, mvt)` 与 SELECT 投影 `$1::smallint, t.x, t.y, ST_AsMVT(...)`)。把:
```rust
        r#"INSERT INTO "{storage_table}" (z, x, y, mvt)
           SELECT $1::smallint, t.x, t.y, ST_AsMVT(features.*, $2, $3, 'geom', 'fid')
           {pipeline}
           GROUP BY t.x, t.y
           HAVING count(*) >= {split_size} AND ST_AsMVT(features.*, $2, $3, 'geom', 'fid') IS NOT NULL"#
```
改为:
```rust
        r#"INSERT INTO "{storage_table}" (z, x, y, format, srid, mvt)
           SELECT $1::smallint, t.x, t.y, 'pbf'::text, $8::integer,
                  ST_AsMVT(features.*, $2, $3, 'geom', 'fid')
           {pipeline}
           GROUP BY t.x, t.y
           HAVING count(*) >= {split_size} AND ST_AsMVT(features.*, $2, $3, 'geom', 'fid') IS NOT NULL"#
```
说明:`$8` 在该 INSERT 的 14 参数绑定中已是 `tile_srs`(顺序 $1 z,$2 name,$3 tile_size,$4 xmin,$5 xmax,$6 ymin,$7 ymax,$8 tile_srs,$9 tile_extend,$10 source_srs,$11..$14 knobs),直接复用作 `srid`;`'pbf'` 为字面量;`id` 由 bigserial 默认值填充。**绑定数组、GROUP BY、HAVING、{pipeline} 不变。**

> 若该仓库的 build_zoom_level INSERT 文本与上面不完全一致(例如 HAVING 因后续改动略有不同),只需对齐两处:(a) 列表加 `format, srid`,(b) SELECT 投影在 `t.x, t.y,` 之后、`ST_AsMVT` 之前插入 `'pbf'::text, $8::integer,`。其余原样保留。

- [ ] **Step 5: 运行测试确认通过**

Run: `CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx test pg17`
Expected: 33 passed(32 + 新测试)。现有 5 处查瓦片表的测试只用 `mvt`/`count(*)`/`octet_length(mvt)`,不受新列影响。

若 `srid` 断言失败(非 3857):确认 SELECT 用的是 `$8`(tile_srs)而非 `$10`(source_srs)。若 INSERT 报列数不匹配:确认列表与 SELECT 投影列数一致(6 列:z,x,y,format,srid,mvt)。

- [ ] **Step 6: detect_changes + Commit**

Run: `detect_changes()`(MCP),确认仅 build/storage 相关符号受影响。

```bash
git add src/storage.rs src/build.rs src/lib.rs
git commit -m "feat: add id/format/srid columns to tile storage table"
```

---

## 自审记录

- **规格覆盖**:schema 三列(Step 3)、INSERT 填充 format/srid + id 自增(Step 4)、ST_Tile 不变(未触 tile.rs)、新 pg_test(Step 1)——spec 各节均覆盖。id=bigserial 主键仍 (z,x,y)、format='pbf'、srid=$8(tile_srs)、仅加表列均落实。
- **耦合**:schema 与 INSERT 同任务同提交(NOT NULL 列必须同时提供),避免破碎中间态。
- **占位符**:无 TBD;Step 4 给出精确 old→new,并附"若文本略有出入"的对齐指引(两处),非占位。
- **一致性**:列顺序与 INSERT 投影顺序一致(z,x,y,format,srid,mvt);srid 源 = $8 = tile_srs,与 spec 一致;测试表名 `_gp_tiles__gp_cols`(pyramid 名默认 = 源表名 `_gp_cols`)。
- **回归**:现有查表测试不读新列,继续通过;新增列对 `count(*)`/`octet_length(mvt)` 无影响。
