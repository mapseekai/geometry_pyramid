# buildRules End-to-End Coverage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close MAP-16, MAP-17, MAP-18, and MAP-38 by adding focused end-to-end `buildRules` regression tests for dynamic reads, prepared multi-rule AND, feature-major-points, and feature-driven paths.

**Architecture:** Add `#[pg_test]` coverage in the existing `src/lib.rs` test module, following the current PostGIS fixture style and using deterministic point/geometry placement. The default path is tests-only; runtime code changes are allowed only if a new test exposes a real path-specific defect after the fixture is verified. Assertions use `ST_Tile(...)` existence on carefully separated tiles so failures identify a missing rule clause rather than MVT byte-order noise.

**Tech Stack:** Rust, pgrx 0.18, PostgreSQL 17, PostGIS 3.6, `cargo pgrx test pg17`, Multica CLI.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-06-buildrules-test-coverage-design.md`.
- Scope is limited to MAP-16, MAP-17, MAP-18, and MAP-38; MAP-21, MAP-22, and MAP-39 are future LOD design work.
- Prefer tests-only changes in `src/lib.rs`; do not refactor runtime code unless a new test proves a real defect.
- Do not add dependencies or a new test framework.
- Use small deterministic fixtures; do not use random data or performance benchmarks.
- Keep critical features in different tiles so `ST_Tile(...) IS [NOT] NULL` expresses the feature set.
- For serial prepared tests, normal `Spi::run` fixture setup is enough. If a later runtime-fix branch adds parallel worker coverage, create source data with `crate::worker::exec_sql_autonomous`.
- Before editing any existing runtime symbol, run GitNexus impact analysis for that symbol and stop for user review if the risk is HIGH or CRITICAL.
- Before committing implementation changes, run GitNexus `detect_changes` and confirm only expected test symbols or intentionally fixed runtime symbols are affected.

## File Structure

- Modify: `src/lib.rs`
  - Responsibility: existing pgrx integration tests for SQL-visible extension behavior.
  - Add four `#[pg_test]` functions inside `#[cfg(any(test, feature = "pg_test"))] #[pg_schema] mod tests`.
  - Reuse the existing `prepared_geom_index_count()` helper for the feature-major path-trigger guard.
- No runtime file should change in the tests-only path.
- No new helper module is planned; the repository currently keeps these pgrx integration fixtures in one file, and this plan follows that pattern.

---

### Task 1: MAP-16 Dynamic Read Path Applies `buildRules`

**Files:**
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: SQL functions `ST_BuildPyramid`, `ST_Tile`, `ST_DropPyramid`.
- Produces: `#[pg_test] fn build_rules_dynamic_split_and_beyond_max_zoom()`.

- [ ] **Step 1: Add the dynamic read regression test**

Add this function inside the existing `mod tests` in `src/lib.rs`, near the current `dynamic_tile_beyond_max_zoom`, `split_size_defers_sparse_tiles`, or `build_rules_filter_covered_zoom_only` tests:

```rust
    #[pg_test]
    fn build_rules_dynamic_split_and_beyond_max_zoom() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_brdyn (gid serial PRIMARY KEY, rank int, \
             geom geometry(Point, 4326))",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_brdyn (rank, geom) VALUES \
             (1, ST_SetSRID(ST_MakePoint(-100, 0), 4326)), \
             (9, ST_SetSRID(ST_MakePoint( 100, 0), 4326))",
        )
        .unwrap();

        let mat_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_brdyn','geom','gid', \
             '{\"name\":\"brdyn_mat\",\"minZoom\":0,\"maxZoom\":5,\"splitSize\":0, \
                \"buildRules\":[{\"minZoom\":2,\"maxZoom\":5,\"filter\":\"rank < 5\"}]}')",
        )
        .unwrap();
        assert_eq!(mat_ok, Some(true));

        let split_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_brdyn','geom','gid', \
             '{\"name\":\"brdyn_split\",\"minZoom\":0,\"maxZoom\":5,\"splitSize\":100, \
                \"buildRules\":[{\"minZoom\":2,\"maxZoom\":5,\"filter\":\"rank < 5\"}]}')",
        )
        .unwrap();
        assert_eq!(split_ok, Some(true));

        let beyond_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_brdyn','geom','gid', \
             '{\"name\":\"brdyn_beyond\",\"minZoom\":0,\"maxZoom\":2,\"splitSize\":0, \
                \"buildRules\":[{\"minZoom\":5,\"maxZoom\":5,\"filter\":\"rank < 5\"}]}')",
        )
        .unwrap();
        assert_eq!(beyond_ok, Some(true));

        let split_materialized =
            Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_brdyn_split")
                .unwrap()
                .unwrap();
        assert_eq!(
            split_materialized, 0,
            "splitSize=100 must force storage misses so ST_Tile uses dynamic slicing"
        );

        // z5 tile coords for (-100,0) and (100,0): x=7/y=16 and x=24/y=16.
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brdyn_mat', 5, 7, 16)")
                .unwrap()
                .is_some(),
            "materialized covered zoom must keep rank 1"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brdyn_mat', 5, 24, 16)")
                .unwrap()
                .is_none(),
            "materialized covered zoom must filter rank 9"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brdyn_split', 5, 7, 16)")
                .unwrap()
                .is_some(),
            "splitSize dynamic covered zoom must keep rank 1"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brdyn_split', 5, 24, 16)")
                .unwrap()
                .is_none(),
            "splitSize dynamic covered zoom must filter rank 9"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brdyn_beyond', 5, 7, 16)")
                .unwrap()
                .is_some(),
            "z>maxZoom dynamic covered zoom must keep rank 1"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brdyn_beyond', 5, 24, 16)")
                .unwrap()
                .is_none(),
            "z>maxZoom dynamic covered zoom must filter rank 9"
        );

        // z1 is not covered by either rule set; the rank 9 feature reappears.
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brdyn_mat', 1, 1, 1)")
                .unwrap()
                .is_some(),
            "uncovered materialized zoom must not filter rank 9"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brdyn_split', 1, 1, 1)")
                .unwrap()
                .is_some(),
            "uncovered splitSize dynamic zoom must not filter rank 9"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brdyn_beyond', 1, 1, 1)")
                .unwrap()
                .is_some(),
            "uncovered stored zoom in beyond-max pyramid must not filter rank 9"
        );

        Spi::run("SELECT ST_DropPyramid('brdyn_mat')").unwrap();
        Spi::run("SELECT ST_DropPyramid('brdyn_split')").unwrap();
        Spi::run("SELECT ST_DropPyramid('brdyn_beyond')").unwrap();
        Spi::run("DROP TABLE _gp_brdyn").unwrap();
    }
```

- [ ] **Step 2: Run the focused test**

Run:

```bash
cargo pgrx test pg17 build_rules_dynamic_split_and_beyond_max_zoom
```

Expected: command exits 0 and output includes `build_rules_dynamic_split_and_beyond_max_zoom ... ok`. If it fails, first verify the hard-coded tile coordinates match the fixture points; only then inspect `src/dynamic.rs` rule-clause plumbing.

- [ ] **Step 3: Commit Task 1**

```bash
git add src/lib.rs
git commit -m "Cover buildRules on dynamic tile reads" \
  -m "MAP-16 needs end-to-end evidence that splitSize-deferred tiles and z>maxZoom tiles apply the same buildRules filter as materialized tiles. The new pg_test builds materialized, splitSize dynamic, and beyond-max dynamic pyramids over one deterministic point fixture." \
  -m "Constraint: tests-only unless the new regression exposes a runtime defect
Confidence: high
Scope-risk: narrow
Directive: Keep dynamic buildRules checks based on feature-set presence, not raw MVT bytes
Tested: cargo pgrx test pg17 build_rules_dynamic_split_and_beyond_max_zoom
Not-tested: Full pgrx suite in this task"
```

---

### Task 2: MAP-17 Prepared Multi-Rule AND Is End-to-End

**Files:**
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `preparedBuild=true`, `rule_projections_for`, `rules_clause_for_zoom(..., RuleTarget::Prepared)`.
- Produces: `#[pg_test] fn build_rules_prepared_multi_rule_and_by_zoom()`.

- [ ] **Step 1: Add the prepared AND regression test**

Add this function inside the existing `mod tests` in `src/lib.rs`, near the current `build_rules_multiple_rules_and_together` test:

```rust
    #[pg_test]
    fn build_rules_prepared_multi_rule_and_by_zoom() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_brprep (gid serial PRIMARY KEY, rank int, kind text, \
             geom geometry(Point, 4326))",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_brprep (rank, kind, geom) VALUES \
             (5, 'keep', ST_SetSRID(ST_MakePoint(-150, 0), 4326)), \
             (1, 'drop', ST_SetSRID(ST_MakePoint( -50, 0), 4326)), \
             (9, 'keep', ST_SetSRID(ST_MakePoint(  50, 0), 4326)), \
             (9, 'drop', ST_SetSRID(ST_MakePoint( 150, 0), 4326))",
        )
        .unwrap();

        let ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_brprep','geom','gid', \
             '{\"name\":\"brprep\",\"minZoom\":0,\"maxZoom\":4,\"splitSize\":0, \
                \"preparedBuild\":true, \
                \"buildRules\":[{\"minZoom\":1,\"maxZoom\":2,\"filter\":\"rank <= 5\"}, \
                                {\"minZoom\":2,\"maxZoom\":3,\"filter\":\"kind = ''keep''\"}]}')",
        )
        .unwrap();
        assert_eq!(ok, Some(true));

        // z1: only rank <= 5 applies. The eastern tile contains only rank 9 rows.
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brprep', 1, 0, 1)")
                .unwrap()
                .is_some(),
            "z1 must keep rows passing the rank rule"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brprep', 1, 1, 1)")
                .unwrap()
                .is_none(),
            "z1 must filter rows failing the rank rule"
        );

        // z2: both rules overlap, so the result is rank <= 5 AND kind = 'keep'.
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brprep', 2, 0, 2)")
                .unwrap()
                .is_some(),
            "z2 must keep the feature satisfying both prepared rule columns"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brprep', 2, 1, 2)")
                .unwrap()
                .is_none(),
            "z2 must filter rank-pass/kind-fail rows"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brprep', 2, 2, 2)")
                .unwrap()
                .is_none(),
            "z2 must filter rank-fail/kind-pass rows"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brprep', 2, 3, 2)")
                .unwrap()
                .is_none(),
            "z2 must filter rows failing both rules"
        );

        // z3: only kind = 'keep' applies; the high-rank keep row reappears.
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brprep', 3, 5, 4)")
                .unwrap()
                .is_some(),
            "z3 must keep high-rank rows when only the kind rule covers the zoom"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brprep', 3, 2, 4)")
                .unwrap()
                .is_none(),
            "z3 must filter kind=drop rows"
        );

        // z4: no rules cover this zoom; the row failing both rules reappears.
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brprep', 4, 14, 8)")
                .unwrap()
                .is_some(),
            "uncovered z4 must not filter the rank 9/kind drop row"
        );

        Spi::run("SELECT ST_DropPyramid('brprep')").unwrap();
        Spi::run("DROP TABLE _gp_brprep").unwrap();
    }
```

- [ ] **Step 2: Run the focused test**

Run:

```bash
cargo pgrx test pg17 build_rules_prepared_multi_rule_and_by_zoom
```

Expected: command exits 0 and output includes `build_rules_prepared_multi_rule_and_by_zoom ... ok`. If it fails at the overlap zoom while non-prepared buildRules tests pass, inspect `src/prepared.rs` CTAS rule projection and `src/build.rs` prepared aliases.

- [ ] **Step 3: Commit Task 2**

```bash
git add src/lib.rs
git commit -m "Cover prepared buildRules multi-rule AND output" \
  -m "MAP-17 needs proof that prepared-table _gp_rule_k columns are consumed by the actual MVT build, not only emitted in helper strings. The new pg_test exercises single-rule, overlapping-rule, and uncovered zooms through preparedBuild=true." \
  -m "Constraint: prepared test omits raw filter columns from attributes so the rule booleans must come from CTAS
Confidence: high
Scope-risk: narrow
Directive: Keep this test focused on prepared tile-major; do not fold feature-driven behavior into it
Tested: cargo pgrx test pg17 build_rules_prepared_multi_rule_and_by_zoom
Not-tested: Full pgrx suite in this task"
```

---

### Task 3: MAP-18 Feature-Major Points Applies `buildRules`

**Files:**
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: existing helper `fn prepared_geom_index_count() -> i64`.
- Produces: `#[pg_test] fn build_rules_feature_major_points_filters_output()`.

- [ ] **Step 1: Add the feature-major-points regression test**

Add this function inside the existing `mod tests` in `src/lib.rs`, near the current `feature_major_points_matches_default_for_point_sources` or buildRules tests:

```rust
    #[pg_test]
    fn build_rules_feature_major_points_filters_output() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_brfm (gid serial PRIMARY KEY, rank int, \
             geom geometry(Point, 4326))",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_brfm (rank, geom) VALUES \
             (1, ST_SetSRID(ST_MakePoint(-100, 0), 4326)), \
             (9, ST_SetSRID(ST_MakePoint( 100, 0), 4326))",
        )
        .unwrap();

        let before = prepared_geom_index_count();
        let ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_brfm','geom','gid', \
             '{\"name\":\"brfm\",\"minZoom\":0,\"maxZoom\":5,\"splitSize\":0, \
                \"preparedBuild\":true,\"featureMajorPoints\":true, \
                \"featureMajorMinZoom\":0, \
                \"buildRules\":[{\"minZoom\":5,\"maxZoom\":5,\"filter\":\"rank < 5\"}]}')",
        )
        .unwrap();
        assert_eq!(ok, Some(true));
        let after = prepared_geom_index_count();
        assert_eq!(
            after, before,
            "pure POINT feature-major path should not create the prepared geom_tile index"
        );

        let tiles = Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_brfm")
            .unwrap()
            .unwrap();
        assert!(tiles > 0, "feature-major buildRules test produced no tiles");

        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brfm', 5, 7, 16)")
                .unwrap()
                .is_some(),
            "feature-major covered zoom must keep rank 1"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brfm', 5, 24, 16)")
                .unwrap()
                .is_none(),
            "feature-major covered zoom must filter rank 9"
        );

        Spi::run("SELECT ST_DropPyramid('brfm')").unwrap();
        Spi::run("DROP TABLE _gp_brfm").unwrap();
    }
```

- [ ] **Step 2: Run the focused test**

Run:

```bash
cargo pgrx test pg17 build_rules_feature_major_points_filters_output
```

Expected: command exits 0 and output includes `build_rules_feature_major_points_filters_output ... ok`. If it fails because `after > before`, confirm the source column type is `geometry(Point, 4326)` and `featureMajorMinZoom` is `0` before inspecting feature-major gating.

- [ ] **Step 3: Commit Task 3**

```bash
git add src/lib.rs
git commit -m "Cover buildRules on feature-major point builds" \
  -m "MAP-18 needs end-to-end evidence that the point feature-major path applies prepared rule columns when writing MVT tiles. The new pg_test forces featureMajorPoints on a pure POINT fixture and checks both path engagement and filtered output." \
  -m "Constraint: featureMajorMinZoom=0 is used only to make the shallow regression fixture deterministic
Confidence: high
Scope-risk: narrow
Directive: Keep this test pure POINT; mixed geometry belongs to prepared fallback or feature-driven coverage
Tested: cargo pgrx test pg17 build_rules_feature_major_points_filters_output
Not-tested: Full pgrx suite in this task"
```

---

### Task 4: MAP-18 Feature-Driven Applies `buildRules`

**Files:**
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `preparedBuild=true`, `featureDrivenEnum=true`, and sparse/outlier fixture shape.
- Produces: `#[pg_test] fn build_rules_feature_driven_filters_output()`.

- [ ] **Step 1: Add the feature-driven regression test**

Add this function inside the existing `mod tests` in `src/lib.rs`, near the current `feature_driven_enum_matches_default_for_mixed_geometries` or buildRules tests:

```rust
    #[pg_test]
    fn build_rules_feature_driven_filters_output() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_brfd (gid serial PRIMARY KEY, kind text, rank int, \
             geom geometry(Geometry, 4326))",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_brfd (kind, rank, geom) VALUES \
             ('keep_point', 1, ST_SetSRID(ST_MakePoint(-100, 0), 4326)), \
             ('drop_point', 900, ST_SetSRID(ST_MakePoint(100, 0), 4326)), \
             ('keep_line', 2, ST_SetSRID(ST_MakeLine(ST_MakePoint(-130, 20), ST_MakePoint(-120, 25)), 4326)), \
             ('keep_poly', 3, ST_SetSRID(ST_MakeEnvelope(-140, 30, -135, 35, 4326), 4326)), \
             ('outlier', 9001, ST_SetSRID(ST_MakePoint(-170, -75), 4326)), \
             ('outlier', 9002, ST_SetSRID(ST_MakePoint(170, 78), 4326))",
        )
        .unwrap();

        let ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_brfd','geom','gid', \
             '{\"name\":\"brfd\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"preparedBuild\":true,\"featureDrivenEnum\":true, \
                \"buildRules\":[{\"minZoom\":6,\"maxZoom\":6,\"filter\":\"rank < 100\"}]}')",
        )
        .unwrap();
        assert_eq!(ok, Some(true));

        let z6_tiles = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _gp_tiles_brfd WHERE z = 6",
        )
        .unwrap()
        .unwrap();
        assert!(
            z6_tiles > 0,
            "feature-driven buildRules test must produce non-empty z6 output"
        );

        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brfd', 6, 14, 32)")
                .unwrap()
                .is_some(),
            "feature-driven covered zoom must keep the rank 1 point"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brfd', 6, 49, 32)")
                .unwrap()
                .is_none(),
            "feature-driven covered zoom must filter the rank 900 point"
        );

        Spi::run("SELECT ST_DropPyramid('brfd')").unwrap();
        Spi::run("DROP TABLE _gp_brfd").unwrap();
    }
```

- [ ] **Step 2: Run the focused test**

Run:

```bash
cargo pgrx test pg17 build_rules_feature_driven_filters_output
```

Expected: command exits 0 and output includes `build_rules_feature_driven_filters_output ... ok`. If it fails because the drop tile exists, inspect `feature_driven_pipeline_sql` rule clause placement after confirming the fixture still has outliers and `featureDrivenEnum=true`.

- [ ] **Step 3: Commit Task 4**

```bash
git add src/lib.rs
git commit -m "Cover buildRules on feature-driven builds" \
  -m "MAP-18 also needs coverage for feature-driven enumeration, whose SQL shape differs from prepared tile-major. The new pg_test uses a sparse mixed fixture with outliers and checks that covered zoom output respects the rank rule." \
  -m "Constraint: sparse fixture must keep critical keep/drop points in distinct z6 tiles
Confidence: high
Scope-risk: narrow
Directive: Do not replace this with byte-length equivalence; feature-set presence is the intended assertion
Tested: cargo pgrx test pg17 build_rules_feature_driven_filters_output
Not-tested: Full pgrx suite in this task"
```

---

### Task 5: Full Verification and Multica Closure

**Files:**
- Modify: none expected.

**Interfaces:**
- Consumes: tests from Tasks 1-4.
- Produces: verified implementation state and closed Multica issues MAP-16, MAP-17, MAP-18, MAP-38.

- [ ] **Step 1: Format check**

Run:

```bash
cargo fmt --check
```

Expected: command exits 0. If it fails, run `cargo fmt`, inspect `git diff`, and rerun `cargo fmt --check`.

- [ ] **Step 2: Run all new focused tests**

Run:

```bash
cargo pgrx test pg17 build_rules_dynamic_split_and_beyond_max_zoom
cargo pgrx test pg17 build_rules_prepared_multi_rule_and_by_zoom
cargo pgrx test pg17 build_rules_feature_major_points_filters_output
cargo pgrx test pg17 build_rules_feature_driven_filters_output
```

Expected: each command exits 0 and each output includes its test name followed by `ok`.

- [ ] **Step 3: Run full pgrx suite**

Run:

```bash
cargo pgrx test pg17
```

Expected: command exits 0. If the command cannot run because PostgreSQL/PostGIS/pgrx is unavailable, capture the exact failure and do not mark Multica issues done.

- [ ] **Step 4: Run GitNexus changed-scope review**

Run GitNexus MCP:

```text
detect_changes({ "repo": "geometry_pyramid", "scope": "all" })
```

Expected: changed symbols are the four new pg_test functions in `src/lib.rs`, with no unexpected runtime process changes. If runtime symbols appear because a test exposed a real defect, confirm that each changed runtime symbol had pre-edit impact analysis and is covered by one of the new tests.

- [ ] **Step 5: Commit final verification-only adjustments if `cargo fmt` changed files**

If Step 1 required `cargo fmt`, commit that formatting change:

```bash
git add src/lib.rs
git commit -m "Format buildRules coverage tests" \
  -m "Formatting-only follow-up after the buildRules coverage tests." \
  -m "Confidence: high
Scope-risk: narrow
Tested: cargo fmt --check
Not-tested: No behavior change"
```

If Step 1 did not change files, skip this step.

- [ ] **Step 6: Mark Multica issues done**

Run only after Steps 1-4 pass:

```bash
multica issue status d4b0f653-9936-4d0b-a6c4-1624d7981441 done --output json
multica issue status 7e61fcd7-9fa9-4957-a4fc-b51aba625b9e done --output json
multica issue status 597471f4-ef85-488d-8868-343fa00cf770 done --output json
multica issue status b6edf6a3-36b6-4143-98cd-592a9cd7c680 done --output json
```

Expected: each command returns JSON with `"status":"done"` for MAP-16, MAP-17, MAP-18, and MAP-38.

- [ ] **Step 7: Final status check**

Run:

```bash
multica issue list --project 07ece43a-f577-4739-a33f-dbc7ad7afcdd --output json
git status --short
```

Expected: MAP-16, MAP-17, MAP-18, and MAP-38 are no longer `todo`; `git status --short` contains no uncommitted changes from this plan's implementation. Pre-existing unrelated working tree changes may still be present and must not be reverted.

---

## Self-Review Notes

- Spec coverage: MAP-16 is Task 1; MAP-17 is Task 2; MAP-18 is Tasks 3-4; parent MAP-38 closes in Task 5 after all child coverage passes.
- Placeholder scan: this plan contains concrete test bodies, exact commands, exact Multica issue IDs, and exact expected outcomes.
- Type consistency: all new functions are `#[pg_test] fn ...()` inside `src/lib.rs`; all SQL calls use existing `Spi::run` and `Spi::get_one` patterns from the current test module.
- Scope check: the plan is focused on existing `buildRules` coverage and does not include future LOD API design.
