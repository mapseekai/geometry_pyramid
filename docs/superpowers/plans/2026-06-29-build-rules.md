# buildRules (per-level feature filtering / LOD) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `buildRules` — per-zoom-band raw-SQL feature filters — so coarse zooms carry only "important" features (Ganos-aligned level-of-detail).

**Architecture:** Each rule's filter is evaluated once into a boolean column `_gp_rule_k` at prepared-table CTAS time; each per-zoom query ANDs the precomputed booleans of the rules whose band covers that zoom. The baseline (non-prepared) path inlines the raw filter against the source instead. Empty `buildRules` = current behavior, byte-for-byte.

**Tech Stack:** Rust, pgrx 0.18, PostgreSQL 17, PostGIS 3.6; `cargo pgrx test pg17`.

## Global Constraints

- pgrx 0.18 / PostgreSQL 17 / PostGIS 3.6; default cargo feature `pg17`.
- Run the suite with `cargo pgrx test pg17`. Unit-only (non-`#[pg_test]`) tests still compile under the same command.
- `BuildRule` must derive `Serialize` too — `PyramidConfig` derives `Serialize` and now contains `Vec<BuildRule>`.
- Empty `buildRules` MUST produce byte-identical output to today (regression gate in Task 4 + equivalence pg_test in Task 6).
- Raw filter is trusted-admin input (runs with the `ST_BuildPyramid` caller's privileges). Guards: non-empty, single-statement (no `;`), validated against the source before any prep/shadow work.
- Spec: `docs/superpowers/specs/2026-06-29-build-rules-design.md`. Rule columns named `_gp_rule_{k}` where `k` is the rule's 0-based index in the `buildRules` array.

---

### Task 1: Config — `BuildRule` type, `buildRules` field, validation

**Files:**
- Modify: `src/config.rs` (struct + `Default` + `validate()` + unit tests)

**Interfaces:**
- Produces: `pub struct BuildRule { pub min_zoom: u8, pub max_zoom: u8, pub filter: String }` (serde rename `minZoom`/`maxZoom`); `PyramidConfig::build_rules: Vec<BuildRule>` (serde `buildRules`, `default`).

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` in `src/config.rs`:

```rust
    #[test]
    fn build_rules_default_empty_and_parses() {
        assert!(PyramidConfig::default().build_rules.is_empty());
        let c = PyramidConfig::parse(
            r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"filter":"scalerank < 5"}]}"#,
        )
        .unwrap();
        assert_eq!(c.build_rules.len(), 1);
        assert_eq!(c.build_rules[0].min_zoom, 0);
        assert_eq!(c.build_rules[0].max_zoom, 4);
        assert_eq!(c.build_rules[0].filter, "scalerank < 5");
        assert!(c.validate().is_ok());
    }

    #[test]
    fn build_rules_reject_inverted_zoom() {
        let c = PyramidConfig::parse(
            r#"{"buildRules":[{"minZoom":5,"maxZoom":2,"filter":"true"}]}"#,
        )
        .unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn build_rules_reject_empty_filter() {
        let c = PyramidConfig::parse(
            r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"filter":"   "}]}"#,
        )
        .unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn build_rules_reject_semicolon() {
        let c = PyramidConfig::parse(
            r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"filter":"true; DROP TABLE x"}]}"#,
        )
        .unwrap();
        assert!(c.validate().is_err());
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo pgrx test pg17 2>&1 | grep -E "build_rules_|error\[" | head`
Expected: compile error — `build_rules` / `BuildRule` not defined.

- [ ] **Step 3: Add the struct, field, default, and validation**

In `src/config.rs`, add the struct just above `pub struct PyramidConfig` (line ~66):

```rust
/// One level-of-detail rule: within `[minZoom, maxZoom]`, only source features
/// satisfying `filter` (a raw SQL boolean expression over source columns)
/// participate in tiles. See docs/superpowers/specs/2026-06-29-build-rules-design.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRule {
    #[serde(rename = "minZoom")]
    pub min_zoom: u8,
    #[serde(rename = "maxZoom")]
    pub max_zoom: u8,
    pub filter: String,
}
```

Add the field inside `PyramidConfig` (next to `split_size`):

```rust
    /// Per-level feature filters (level-of-detail). Empty = no filtering (default).
    #[serde(default, rename = "buildRules")]
    pub build_rules: Vec<BuildRule>,
```

Add to the `impl Default for PyramidConfig` block (next to `split_size: 0,`):

```rust
            build_rules: Vec::new(),
```

In `validate()`, just before the final `Ok(())`:

```rust
        for (k, rule) in self.build_rules.iter().enumerate() {
            if rule.min_zoom > rule.max_zoom {
                return Err(format!(
                    "buildRules[{k}]: minZoom ({}) must be <= maxZoom ({})",
                    rule.min_zoom, rule.max_zoom
                ));
            }
            let filter = rule.filter.trim();
            if filter.is_empty() {
                return Err(format!("buildRules[{k}]: filter must not be empty"));
            }
            if filter.contains(';') {
                return Err(format!(
                    "buildRules[{k}]: filter must be a single expression (no ';')"
                ));
            }
        }
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo pgrx test pg17 2>&1 | grep -E "build_rules_|test result" | head`
Expected: the four `build_rules_*` tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): add buildRules per-level filter config + validation"
```

---

### Task 2: build.rs SQL-fragment helpers (`rule_projections_for`, `rules_clause_for_zoom`)

**Files:**
- Modify: `src/build.rs` (two pure helpers + `RuleTarget` enum + unit tests)

**Interfaces:**
- Consumes: `PyramidConfig::build_rules` (Task 1).
- Produces:
  - `pub(crate) fn rule_projections_for(cfg: &PyramidConfig) -> String` → `", (<filter>) AS _gp_rule_0, (<filter>) AS _gp_rule_1…"` (empty when no rules).
  - `pub(crate) enum RuleTarget { Prepared, Source }` (derives `Clone, Copy`).
  - `pub(crate) fn rules_clause_for_zoom(cfg: &PyramidConfig, z: u8, target: RuleTarget) -> String` → `" AND prep_src._gp_rule_k…"` (Prepared) / `" AND (<filter>)…"` (Source); empty when no rule covers `z`.

- [ ] **Step 1: Write the failing tests**

Add to `#[cfg(test)] mod tests` in `src/build.rs`:

```rust
    #[test]
    fn rule_projections_number_by_index() {
        let cfg = PyramidConfig::parse(
            r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"filter":"a < 5"},
                              {"minZoom":5,"maxZoom":8,"filter":"b > 1"}]}"#,
        )
        .unwrap();
        let p = rule_projections_for(&cfg);
        assert!(p.contains("(a < 5) AS _gp_rule_0"), "got: {p}");
        assert!(p.contains("(b > 1) AS _gp_rule_1"), "got: {p}");
    }

    #[test]
    fn rule_projections_empty_when_no_rules() {
        assert_eq!(rule_projections_for(&PyramidConfig::default()), "");
    }

    #[test]
    fn rules_clause_prepared_ands_covering_booleans() {
        let cfg = PyramidConfig::parse(
            r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"filter":"a < 5"},
                              {"minZoom":3,"maxZoom":8,"filter":"b > 1"}]}"#,
        )
        .unwrap();
        assert_eq!(
            rules_clause_for_zoom(&cfg, 3, RuleTarget::Prepared),
            " AND prep_src._gp_rule_0 AND prep_src._gp_rule_1"
        );
        assert_eq!(
            rules_clause_for_zoom(&cfg, 6, RuleTarget::Prepared),
            " AND prep_src._gp_rule_1"
        );
        assert_eq!(rules_clause_for_zoom(&cfg, 10, RuleTarget::Prepared), "");
    }

    #[test]
    fn rules_clause_source_inlines_filter() {
        let cfg = PyramidConfig::parse(
            r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"filter":"a < 5"}]}"#,
        )
        .unwrap();
        assert_eq!(
            rules_clause_for_zoom(&cfg, 2, RuleTarget::Source),
            " AND (a < 5)"
        );
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo pgrx test pg17 2>&1 | grep -E "rule_projections|rules_clause|error\[" | head`
Expected: compile error — helpers not defined.

- [ ] **Step 3: Implement the helpers**

Add to `src/build.rs` near `attribute_projections_for` (after line ~726):

```rust
/// LOD rule predicate columns for the prepared-table CTAS: one boolean column
/// `_gp_rule_k` per rule, evaluated once from the source row. Empty when there
/// are no rules. Mirrors the inner fragment shape of `attribute_projections_for`.
pub(crate) fn rule_projections_for(cfg: &PyramidConfig) -> String {
    let mut s = String::new();
    for (k, rule) in cfg.build_rules.iter().enumerate() {
        s.push_str(&format!(r#", ({}) AS _gp_rule_{k}"#, rule.filter));
    }
    s
}

/// Which side a per-zoom rule clause targets: the prepared table (precomputed
/// `_gp_rule_k` booleans) or the raw source table (the inline filter expression).
#[derive(Clone, Copy)]
pub(crate) enum RuleTarget {
    Prepared,
    Source,
}

/// The ` AND …` clause restricting zoom `z` to features passing every rule whose
/// band covers `z`. Empty when no rule covers `z`. `Prepared` references the
/// precomputed `prep_src._gp_rule_k`; `Source` inlines the raw filter (unqualified
/// columns resolve against the single source table in scope).
pub(crate) fn rules_clause_for_zoom(cfg: &PyramidConfig, z: u8, target: RuleTarget) -> String {
    let mut s = String::new();
    for (k, rule) in cfg.build_rules.iter().enumerate() {
        if z >= rule.min_zoom && z <= rule.max_zoom {
            match target {
                RuleTarget::Prepared => s.push_str(&format!(" AND prep_src._gp_rule_{k}")),
                RuleTarget::Source => s.push_str(&format!(" AND ({})", rule.filter)),
            }
        }
    }
    s
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo pgrx test pg17 2>&1 | grep -E "rule_projections|rules_clause|test result" | head`
Expected: the four helper tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/build.rs
git commit -m "feat(build): add buildRules SQL-fragment helpers"
```

---

### Task 3: Carry rule boolean columns into the prepared table

**Files:**
- Modify: `src/prepared.rs` (`create_table_sql`, `create_table_committed_sql`, both `prepare_*` call sites, existing + new unit tests)

**Interfaces:**
- Consumes: `crate::build::rule_projections_for` (Task 2).
- Produces: prepared table (both temp and committed) carries `_gp_rule_k boolean` columns.

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `src/prepared.rs`:

```rust
    #[test]
    fn create_table_sql_projects_rule_columns() {
        let sql = create_table_sql(
            "_gp_prep_7",
            "roads",
            "geom",
            "gid",
            "",
            r#", (scalerank < 5) AS _gp_rule_0"#,
        );
        assert!(sql.contains(r#"(scalerank < 5) AS _gp_rule_0"#));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo pgrx test pg17 2>&1 | grep -E "create_table_sql_projects_rule|error\[" | head`
Expected: compile error — `create_table_sql` takes 5 args, called with 6.

- [ ] **Step 3: Thread `rules_inner` through both CTAS builders and call sites**

In `src/prepared.rs`, add a `rules_inner: &str` parameter to **both** `create_table_committed_sql` and `create_table_sql`, inserting `{rules_inner}` right after `{attrs_inner}` in each `format!`. For `create_table_committed_sql`:

```rust
fn create_table_committed_sql(
    prepared_table: &str,
    source_table: &str,
    geom_column: &str,
    fid_column: &str,
    attrs_inner: &str,
    rules_inner: &str,
    tile_srs: i32,
) -> String {
    format!(
        r#"CREATE TABLE "{prepared_table}" AS
           SELECT
              src."{fid_column}"::bigint AS fid,
              ST_Transform(src."{geom_column}", {tile_srs}) AS geom_tile,
              GeometryType(src."{geom_column}") AS geom_type
              {attrs_inner}
              {rules_inner}
           FROM "{source_table}" src
           WHERE src."{geom_column}" IS NOT NULL"#
    )
}
```

For `create_table_sql` (keep `$1` for tile_srs, add `rules_inner` before the implicit srs):

```rust
fn create_table_sql(
    prepared_table: &str,
    source_table: &str,
    geom_column: &str,
    fid_column: &str,
    attrs_inner: &str,
    rules_inner: &str,
) -> String {
    format!(
        r#"CREATE TEMP TABLE "{prepared_table}" ON COMMIT DROP AS
           SELECT
              src."{fid_column}"::bigint AS fid,
              ST_Transform(src."{geom_column}", $1) AS geom_tile,
              GeometryType(src."{geom_column}") AS geom_type
              {attrs_inner}
              {rules_inner}
           FROM "{source_table}" src
           WHERE src."{geom_column}" IS NOT NULL"#
    )
}
```

In `prepare_source_for_build`, after the `attrs_inner` line:

```rust
    let (attrs_inner, _) = crate::build::attribute_projections_for(src.cfg, "src", "prep");
    let rules_inner = crate::build::rule_projections_for(src.cfg);
```

and pass `&rules_inner` into the `create_table_sql(...)` call (after `&attrs_inner`).

In `prepare_source_committed`, likewise add `let rules_inner = crate::build::rule_projections_for(src.cfg);` after the `attrs_inner` line, and pass `&rules_inner` into `create_table_committed_sql(...)` (after `&attrs_inner`, before `src.cfg.tile_srs`).

- [ ] **Step 4: Fix the two existing CTAS unit tests**

In `create_table_sql_projects_geometry_and_attributes`, change the call to pass `""` for `rules_inner`:

```rust
        let sql = create_table_sql(
            "_gp_prep_7",
            "roads",
            "geom",
            "gid",
            r#", src."kind" AS "kind""#,
            "",
        );
```

In `committed_create_sql_is_persistent_and_inlines_srs`, insert `""` for `rules_inner` before the `3857` argument:

```rust
        let sql = create_table_committed_sql(
            "_gp_prep_roads",
            "roads",
            "geom",
            "gid",
            r#", src."kind" AS "kind""#,
            "",
            3857,
        );
```

- [ ] **Step 5: Run to verify all prepared tests pass**

Run: `cargo pgrx test pg17 2>&1 | grep -E "create_table|committed_create|test result" | head`
Expected: all `prepared::tests` PASS, including `create_table_sql_projects_rule_columns`.

- [ ] **Step 6: Commit**

```bash
git add src/prepared.rs
git commit -m "feat(prepared): carry buildRules boolean columns in prepared table"
```

---

### Task 4: Apply the per-zoom rule clause in all four build paths

**Files:**
- Modify: `src/build.rs` (4 pipeline builders gain a `rule_where: &str` param; 4 dispatchers compute + pass the clause)

**Interfaces:**
- Consumes: `rules_clause_for_zoom`, `RuleTarget` (Task 2); `_gp_rule_k` columns (Task 3).
- Produces: each per-zoom query restricted by `R(z)`.

- [ ] **Step 1: Add `rule_where` to the four pipeline builders**

In `src/build.rs`:

(a) `tile_pipeline_from_sql` — add `rule_where: &str` as the last parameter, and append `{rule_where}` to the inner source `WHERE` (after the `&& ST_Transform(...$10)` filter, before `) prep`):

```rust
              FROM (
                 ...same...
                 FROM "{table}" src
                 WHERE src."{geom_column}" IS NOT NULL
                   AND src."{geom_column}" && ST_Transform(
                          ST_TileEnvelope($1, t.x, t.y, margin => ($9::float8 / $3::float8)),
                          $10
                       ){rule_where}
              ) prep
```

(b) `tile_pipeline_from_prepared_sql` — add `rule_where: &str` last param; append after the `ST_TileEnvelope(...)` filter, before `) prep`:

```rust
                 FROM "{prepared_table}" prep_src
                 WHERE prep_src.geom_tile && ST_TileEnvelope(
                    $1, t.x, t.y, margin => ($9::float8 / $3::float8)
                 ){rule_where}
              ) prep
```

(c) `point_feature_major_pipeline_sql` — add `rule_where: &str` last param; append at the end of the `raw` CTE `WHERE` (after the `< $14::float8)` drop-rate close, before `),`):

```rust
             FROM "{prepared_table}" prep_src
             WHERE prep_src.geom_type = 'POINT'
               AND ($14::float8 >= 1.0
                    OR (abs(hashtextextended(prep_src.fid::text, 0::bigint)) % 1000000)::float8
                       / 1000000.0 < $14::float8){rule_where}
           ),
```

(d) `feature_driven_pipeline_sql` — add `rule_where: &str` last param; append at the end of the `prepped` CTE `WHERE` (after the `< $14::float8)` drop-rate close, before `),`):

```rust
               AND ($14::float8 >= 1.0
                    OR prep_src.geom_type NOT IN ('POINT','MULTIPOINT')
                    OR (abs(hashtextextended(prep_src.fid::text, 0::bigint)) % 1000000)::float8
                       / 1000000.0 < $14::float8){rule_where}
           ),
```

- [ ] **Step 2: Compute and pass the clause from the four dispatchers**

`build_zoom_level` (source/baseline) — after `let (attrs_inner, attrs_outer) = attribute_projections(cfg);`:

```rust
    let rule_where = rules_clause_for_zoom(cfg, z, RuleTarget::Source);
    let pipeline = tile_pipeline_from_sql(
        src.source_table,
        src.geom_column,
        src.fid_column,
        &attrs_inner,
        &attrs_outer,
        &rule_where,
    );
```

`build_zoom_level_prepared` — replace the `tile_pipeline_from_prepared_sql` call:

```rust
    let rule_where = rules_clause_for_zoom(cfg, z, RuleTarget::Prepared);
    let pipeline =
        tile_pipeline_from_prepared_sql(&prepared.table, &attrs_inner, &attrs_outer, &rule_where);
```

`build_zoom_level_prepared_points_feature_major` — replace the `point_feature_major_pipeline_sql` call:

```rust
    let rule_where = rules_clause_for_zoom(cfg, z, RuleTarget::Prepared);
    let pipeline = point_feature_major_pipeline_sql(
        &prepared.table,
        &attrs_inner,
        &attrs_outer,
        &attrs_mvt,
        &rule_where,
    );
```

`build_zoom_level_prepared_feature_driven` — replace the `feature_driven_pipeline_sql` call:

```rust
    let rule_where = rules_clause_for_zoom(cfg, z, RuleTarget::Prepared);
    let pipeline = feature_driven_pipeline_sql(
        &prepared.table,
        &attrs_inner,
        &attrs_from_prepped,
        &attrs_from_kept,
        &attrs_mvt,
        &rule_where,
    );
```

- [ ] **Step 3: Run the FULL suite — empty rules must not change anything**

Run: `cargo pgrx test pg17 2>&1 | tail -5`
Expected: all existing tests PASS (no `buildRules` set anywhere yet → every `rule_where`/`rules_inner` is `""` → byte-identical output). This is the regression gate for the "empty = unchanged" constraint.

- [ ] **Step 4: Commit**

```bash
git add src/build.rs
git commit -m "feat(build): apply per-zoom buildRules clause in all four build paths"
```

---

### Task 5: Validate each rule's filter against the source before building

**Files:**
- Modify: `src/build.rs` (`build_into` — probe loop at the top)

**Interfaces:**
- Consumes: `PyramidConfig::build_rules`, `PyramidSource::source_table`.
- Produces: a clean, rule-numbered error for a bad filter; no partial pyramid (probe runs before any prep/shadow work; `build_into` returning `Err` aborts the atomic swap).

- [ ] **Step 1: Add the probe at the top of `build_into`**

In `src/build.rs`, as the first statements of `build_into` (before `let bbox = source_bbox(...)`):

```rust
    // Validate each LOD rule's filter against the source up front, so a bad
    // expression fails cleanly (naming the rule) before any prep/shadow work and
    // never leaves a partial pyramid. `AND false` makes it a pure parse/column
    // check that scans nothing.
    for (k, rule) in src.cfg.build_rules.iter().enumerate() {
        let probe = format!(
            r#"SELECT 1 FROM "{}" WHERE ({}) AND false LIMIT 1"#,
            src.source_table, rule.filter
        );
        Spi::run(&probe).map_err(|e| {
            Box::<dyn std::error::Error + Send + Sync>::from(format!(
                "buildRules[{k}] filter is invalid: {e}"
            ))
        })?;
    }
```

- [ ] **Step 2: Verify it compiles and the suite still passes**

Run: `cargo pgrx test pg17 2>&1 | tail -5`
Expected: all tests PASS (no rules in existing tests → probe loop is a no-op).

- [ ] **Step 3: Commit**

```bash
git add src/build.rs
git commit -m "feat(build): pre-validate buildRules filters against the source"
```

---

### Task 6: End-to-end pg_tests for buildRules

**Files:**
- Modify: `src/lib.rs` (add `#[pg_test]` functions in the `tests` module)

**Interfaces:**
- Consumes: the full feature (Tasks 1–5).

Coordinate facts used below (EPSG:4326 → Web-Mercator tiling): a point at `lon=-100, lat=0` falls in tile `(z=2,x=0,y=2)` and `(z=3,x=1,y=4)`; `lon=+100, lat=0` falls in `(z=2,x=3,y=2)` and `(z=3,x=6,y=4)`; `lon=0,lat=0` falls in `(z=2,x=2,y=2)`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/lib.rs`:

```rust
    #[pg_test]
    fn build_rules_filter_covered_zoom_only() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_br (gid serial PRIMARY KEY, rank int, \
             geom geometry(Point, 4326))",
        )
        .unwrap();
        // A: lon -100 rank 1 (kept by rank<5); B: lon +100 rank 9 (dropped).
        Spi::run(
            "INSERT INTO _gp_br (rank, geom) VALUES \
             (1, ST_SetSRID(ST_MakePoint(-100, 0), 4326)), \
             (9, ST_SetSRID(ST_MakePoint( 100, 0), 4326))",
        )
        .unwrap();
        Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_br','geom','gid', \
             '{\"name\":\"br\",\"minZoom\":0,\"maxZoom\":3,\"splitSize\":0, \
               \"buildRules\":[{\"minZoom\":2,\"maxZoom\":2,\"filter\":\"rank < 5\"}]}')",
        )
        .unwrap();

        // z2 is covered by the rule: A's tile survives, B's tile is filtered out.
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('br', 2, 0, 2)").unwrap().is_some(),
            "z2 tile over A (rank 1) must survive rank<5"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('br', 2, 3, 2)").unwrap().is_none(),
            "z2 tile over B (rank 9) must be filtered out"
        );
        // z3 is NOT covered: B reappears.
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('br', 3, 6, 4)").unwrap().is_some(),
            "z3 (uncovered) tile over B must be present"
        );

        Spi::run("SELECT ST_DropPyramid('br')").unwrap();
        Spi::run("DROP TABLE _gp_br").unwrap();
    }

    #[pg_test]
    fn build_rules_true_filter_matches_no_rules() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_bre (gid serial PRIMARY KEY, geom geometry(Point, 4326))",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_bre (geom) SELECT ST_SetSRID(ST_MakePoint(\
                random()*340 - 170, random()*140 - 70), 4326) \
             FROM generate_series(1, 300)",
        )
        .unwrap();
        for (name, cfg) in [
            ("bre_none", r#"{"name":"bre_none","minZoom":0,"maxZoom":4,"splitSize":0}"#),
            (
                "bre_true",
                r#"{"name":"bre_true","minZoom":0,"maxZoom":4,"splitSize":0,"buildRules":[{"minZoom":0,"maxZoom":22,"filter":"true"}]}"#,
            ),
        ] {
            let sql = format!("SELECT ST_BuildPyramid('_gp_bre','geom','gid','{cfg}')");
            Spi::get_one::<bool>(&sql).unwrap();
            let _ = name;
        }
        // Every materialized tile must be byte-identical between the two.
        let mismatches = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM \
               (SELECT z,x,y,mvt FROM geometry_pyramid_bre_none) a \
             FULL JOIN \
               (SELECT z,x,y,mvt FROM geometry_pyramid_bre_true) b \
             USING (z,x,y) \
             WHERE a.mvt IS DISTINCT FROM b.mvt",
        )
        .unwrap()
        .unwrap();
        assert_eq!(mismatches, 0, "true-filter pyramid must equal the no-rules pyramid");

        Spi::run("SELECT ST_DropPyramid('bre_none')").unwrap();
        Spi::run("SELECT ST_DropPyramid('bre_true')").unwrap();
        Spi::run("DROP TABLE _gp_bre").unwrap();
    }

    #[pg_test]
    fn build_rules_parallel_matches_serial() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_brp (gid serial PRIMARY KEY, rank int, geom geometry(Point, 4326))",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_brp (rank, geom) SELECT (g % 10), \
               ST_SetSRID(ST_MakePoint(random()*340 - 170, random()*140 - 70), 4326) \
             FROM generate_series(1, 400) g",
        )
        .unwrap();
        for (name, par) in [("brp_ser", 1), ("brp_par", 2)] {
            let cfg = format!(
                r#"{{"name":"{name}","minZoom":0,"maxZoom":5,"splitSize":0,"preparedBuild":true,"parallel":{par},"buildRules":[{{"minZoom":0,"maxZoom":3,"filter":"rank < 5"}}]}}"#
            );
            Spi::get_one::<bool>(&format!("SELECT ST_BuildPyramid('_gp_brp','geom','gid','{cfg}')"))
                .unwrap();
        }
        let mismatches = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM geometry_pyramid_brp_ser a \
             FULL JOIN geometry_pyramid_brp_par b USING (z,x,y) \
             WHERE octet_length(a.mvt) IS DISTINCT FROM octet_length(b.mvt)",
        )
        .unwrap()
        .unwrap();
        assert_eq!(mismatches, 0, "parallel buildRules must equal serial");

        Spi::run("SELECT ST_DropPyramid('brp_ser')").unwrap();
        Spi::run("SELECT ST_DropPyramid('brp_par')").unwrap();
        Spi::run("DROP TABLE _gp_brp").unwrap();
    }

    #[pg_test]
    fn build_rules_multiple_rules_and_together() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_brm (gid serial PRIMARY KEY, rank int, geom geometry(Point, 4326))",
        )
        .unwrap();
        // A lon-100 rank5 (passes both >=5 and <=5); B lon+100 rank9 (fails <=5);
        // C lon0 rank1 (fails >=5).
        Spi::run(
            "INSERT INTO _gp_brm (rank, geom) VALUES \
             (5, ST_SetSRID(ST_MakePoint(-100, 0), 4326)), \
             (9, ST_SetSRID(ST_MakePoint( 100, 0), 4326)), \
             (1, ST_SetSRID(ST_MakePoint(   0, 0), 4326))",
        )
        .unwrap();
        Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_brm','geom','gid', \
             '{\"name\":\"brm\",\"minZoom\":0,\"maxZoom\":2,\"splitSize\":0, \
               \"buildRules\":[{\"minZoom\":2,\"maxZoom\":2,\"filter\":\"rank >= 5\"}, \
                               {\"minZoom\":2,\"maxZoom\":2,\"filter\":\"rank <= 5\"}]}')",
        )
        .unwrap();
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brm', 2, 0, 2)").unwrap().is_some(),
            "A (rank 5) passes both rules"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brm', 2, 3, 2)").unwrap().is_none(),
            "B (rank 9) fails rank<=5"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brm', 2, 2, 2)").unwrap().is_none(),
            "C (rank 1) fails rank>=5"
        );

        Spi::run("SELECT ST_DropPyramid('brm')").unwrap();
        Spi::run("DROP TABLE _gp_brm").unwrap();
    }

    #[pg_test]
    fn build_rules_invalid_filter_errors_cleanly() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_bri (gid serial PRIMARY KEY, geom geometry(Point, 4326))",
        )
        .unwrap();
        Spi::run("INSERT INTO _gp_bri (geom) VALUES (ST_SetSRID(ST_MakePoint(1,1),4326))")
            .unwrap();
        // Filter references a non-existent column: build must error in a
        // subtransaction, leaving NO pyramid behind.
        Spi::run(
            "DO $$ BEGIN \
               PERFORM ST_BuildPyramid('_gp_bri','geom','gid', \
                 '{\"name\":\"bri\",\"minZoom\":0,\"maxZoom\":2, \
                   \"buildRules\":[{\"minZoom\":0,\"maxZoom\":2,\"filter\":\"no_such_col < 5\"}]}'); \
             EXCEPTION WHEN others THEN NULL; END $$",
        )
        .unwrap();
        let exists = Spi::get_one::<bool>(
            "SELECT to_regclass('geometry_pyramid_bri') IS NOT NULL",
        )
        .unwrap()
        .unwrap();
        assert!(!exists, "invalid buildRules filter must leave no pyramid");

        Spi::run("DROP TABLE _gp_bri").unwrap();
    }
```

- [ ] **Step 2: Run to verify they fail (before re-confirming the impl), then pass**

Run: `cargo pgrx test pg17 2>&1 | grep -E "build_rules_(filter|true|parallel|multiple|invalid)|test result"`
Expected: all five PASS (Tasks 1–5 implement the behavior). If `build_rules_filter_covered_zoom_only` fails on a tile coordinate, re-derive the (z,x,y) for the test longitudes with `SELECT ST_TileEnvelope(...)` and adjust — the assertion logic (covered filtered, uncovered present) stays the same.

- [ ] **Step 3: Confirm the storage table name**

The equivalence/parallel tests query `geometry_pyramid_<name>`. If the suite errors with `relation "geometry_pyramid_bre_none" does not exist`, find the real prefix:

Run: `grep -n "fn tile_table_name" src/storage.rs`
Then read that function and adjust the table names in the two FULL JOIN queries accordingly.

- [ ] **Step 4: Run the full suite**

Run: `cargo pgrx test pg17 2>&1 | tail -5`
Expected: every test passes (existing + 5 new).

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs
git commit -m "test: end-to-end pg_tests for buildRules per-level filtering"
```

---

### Task 7: Document `buildRules` in the README

**Files:**
- Modify: `README.md` (config table row + a short usage note)

- [ ] **Step 1: Add a config-table row**

In the config table in `README.md` (next to the `splitSize` row), add:

```markdown
| `buildRules` | `[]` | Per-level feature filters (LOD). Each rule `{minZoom, maxZoom, filter}` restricts that zoom band to source features satisfying `filter` (a raw SQL boolean expression over source columns). Zooms not covered by any rule include all features; multiple rules covering a zoom are ANDed. Empty (default) = no filtering. |
```

- [ ] **Step 2: Add a usage + safety note**

After the `splitSize` note in `README.md`, add:

```markdown
> **Note on `buildRules`:** rules implement level-of-detail — e.g. `[{"minZoom":0,"maxZoom":4,"filter":"scalerank < 5"}]` keeps only major features at coarse zooms. `filter` is a raw SQL boolean expression evaluated with the privileges of the `ST_BuildPyramid` caller (trusted-admin input, like a view definition); it is checked against the source before any build work and must be a single expression (no `;`). This is a deliberate, documented exception to parameterized-query guidance — an arbitrary predicate cannot be parameterized.
```

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: document buildRules per-level feature filtering"
```

---

## Self-Review

**Spec coverage:**
- §2 config schema → Task 1. §3 semantics (uncovered=all, multi-rule AND) → Task 2 (`rules_clause_for_zoom`) + Task 6 (`multiple_rules_and_together`, `filter_covered_zoom_only`). §4 precomputed booleans → Task 3 (prep columns) + Task 4 (prep paths) + baseline source path (Task 4a). §5 validation/safety → Task 1 (structural guards) + Task 5 (source probe). §6 parallel → Task 3 (committed prep carries columns) + Task 6 (`parallel_matches_serial`). §7 affected code → Tasks 1–4 cover every listed file/function. §8 testing → Task 6 maps 1:1 to the five listed pg_tests + Task 1/2/3 unit tests. §9 backward compat → Task 4 Step 3 regression gate + Task 6 `true_filter_matches_no_rules`. README (§7) → Task 7.
- No spec requirement left without a task.

**Placeholder scan:** No TBD/TODO/"handle errors"/"similar to". Every code step shows complete code; every test step shows full test bodies.

**Type consistency:** `BuildRule{min_zoom,max_zoom,filter}` defined in Task 1, used identically in Tasks 2/5. `rule_projections_for(&PyramidConfig)->String`, `rules_clause_for_zoom(&PyramidConfig,u8,RuleTarget)->String`, `RuleTarget{Prepared,Source}` defined in Task 2, consumed with matching signatures in Tasks 3/4. `_gp_rule_{k}` column naming consistent between Task 2 (`rule_projections_for` emits, `rules_clause_for_zoom` references) and Task 3 (CTAS carries). The four pipeline builders each gain exactly one trailing `rule_where: &str`, matched by the four dispatcher call sites in Task 4.
