# 首次全量构建加速 impl Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make first full `ST_BuildPyramid` builds measurable and faster by adding a benchmark harness, removing one duplicate MVT aggregation, and adding an opt-in single-backend `preparedBuild` path that pre-projects source geometries into a temporary indexed table.

**Architecture:** Keep existing dynamic tile and incremental update paths unchanged. Add a small `src/prepared.rs` unit for build-local prepared tables, route only full-build `parallel=1 && preparedBuild=true` through a new prepared tile pipeline, and let `parallel>1` fall back to the existing worker path with a NOTICE. Benchmarking stays manual and documented so runtime-heavy checks do not slow default tests.

**Tech Stack:** Rust, pgrx 0.18, PostgreSQL/PostGIS, SPI dynamic SQL, GitNexus impact/detect_changes, `cargo check`, `cargo pgrx test`.

## Task Structure

### Task 1: Benchmark Harness

- Create: `bench/build_speed.sql`
- Modify: `README.md` benchmark section only
- Test: manual `psql` run against an installed extension

- [ ] **Step 1: Create the benchmark directory and SQL file**

  Create `bench/build_speed.sql` with this exact content:

  ```sql
  \set ON_ERROR_STOP on
  \timing on

  \if :{?pyramid_name}
  \else
    \set pyramid_name gp_bench_baseline
  \endif

  \if :{?prepared_build}
  \else
    \set prepared_build false
  \endif

  \if :{?max_zoom}
  \else
    \set max_zoom 8
  \endif

  CREATE EXTENSION IF NOT EXISTS postgis;
  CREATE EXTENSION IF NOT EXISTS geometry_pyramid;

  DROP TABLE IF EXISTS _gp_bench_mixed;
  CREATE TABLE _gp_bench_mixed (
      gid  bigserial PRIMARY KEY,
      kind text NOT NULL,
      rank integer NOT NULL,
      geom geometry(Geometry, 4326) NOT NULL
  );

  INSERT INTO _gp_bench_mixed (kind, rank, geom)
  SELECT
      'point',
      i,
      ST_SetSRID(
          ST_MakePoint(
              116.35 + ((i % 120)::float8 * 0.001),
              39.85 + ((i / 120)::float8 * 0.001)
          ),
          4326
      )
  FROM generate_series(1, 6000) AS i;

  INSERT INTO _gp_bench_mixed (kind, rank, geom)
  SELECT
      'line',
      i,
      ST_SetSRID(
          ST_MakeLine(
              ST_MakePoint(116.20 + ((i % 80)::float8 * 0.0015), 39.70 + ((i / 80)::float8 * 0.0015)),
              ST_MakePoint(116.22 + ((i % 80)::float8 * 0.0015), 39.72 + ((i / 80)::float8 * 0.0015))
          ),
          4326
      )
  FROM generate_series(1, 2000) AS i;

  INSERT INTO _gp_bench_mixed (kind, rank, geom)
  SELECT
      'polygon',
      i,
      ST_SetSRID(
          ST_MakeEnvelope(
              116.45 + ((i % 60)::float8 * 0.001),
              39.92 + ((i / 60)::float8 * 0.001),
              116.4506 + ((i % 60)::float8 * 0.001),
              39.9206 + ((i / 60)::float8 * 0.001),
              4326
          ),
          4326
      )
  FROM generate_series(1, 1500) AS i;

  INSERT INTO _gp_bench_mixed (kind, rank, geom)
  SELECT
      'sparse_point',
      i,
      ST_SetSRID(ST_MakePoint(115.0 + i * 0.02, 38.0 + i * 0.01), 4326)
  FROM generate_series(1, 50) AS i;

  CREATE INDEX _gp_bench_mixed_geom_gix ON _gp_bench_mixed USING gist (geom);
  ANALYZE _gp_bench_mixed;

  SELECT
      count(*) AS total_rows,
      count(*) FILTER (WHERE GeometryType(geom) IN ('POINT','MULTIPOINT')) AS point_rows,
      count(*) FILTER (WHERE GeometryType(geom) IN ('LINESTRING','MULTILINESTRING')) AS line_rows,
      count(*) FILTER (WHERE GeometryType(geom) IN ('POLYGON','MULTIPOLYGON')) AS polygon_rows
  FROM _gp_bench_mixed;

  SELECT ST_DropPyramid(:'pyramid_name');

  SELECT clock_timestamp() AS build_started_at;

  SELECT ST_BuildPyramid(
      '_gp_bench_mixed',
      'geom',
      'gid',
      jsonb_build_object(
          'name', :'pyramid_name',
          'minZoom', 0,
          'maxZoom', :max_zoom::integer,
          'splitSize', 100,
          'attributes', jsonb_build_array('kind', 'rank'),
          'preparedBuild', :prepared_build::boolean
      )::text
  ) AS built;

  SELECT clock_timestamp() AS build_finished_at;

  SELECT ST_Pyramid(:'pyramid_name') AS pyramid_info;

  SELECT
      z,
      tile_count,
      total_bytes,
      max_tile_bytes,
      build_ms,
      attempts
  FROM geometry_pyramid_stats
  WHERE pyramid_name = :'pyramid_name'
  ORDER BY z;
  ```

- [ ] **Step 2: Add benchmark instructions to `README.md`**

  Add this section after the existing development commands:

  ```markdown
  ## Build-Speed Benchmark

  `bench/build_speed.sql` creates a deterministic mixed point/line/polygon dataset
  and runs a first full `ST_BuildPyramid` build. It is manual by design because it
  creates thousands of PostGIS geometries and records wall-clock timing.

  Baseline:

  ```bash
  psql "$DATABASE_URL" \
    -v pyramid_name=gp_bench_baseline \
    -v prepared_build=false \
    -v max_zoom=8 \
    -f bench/build_speed.sql
  ```

  Prepared build comparison, after `preparedBuild` is implemented:

  ```bash
  psql "$DATABASE_URL" \
    -v pyramid_name=gp_bench_prepared \
    -v prepared_build=true \
    -v max_zoom=8 \
    -f bench/build_speed.sql
  ```

  Compare total wall-clock time and the per-zoom rows in
  `geometry_pyramid_stats`. `preparedBuild` is opt-in and defaults to `false`.
  ```

- [ ] **Step 3: Verify the benchmark file parses under psql variables**

  Run:

  ```bash
  psql "$DATABASE_URL" -v pyramid_name=gp_bench_baseline -v prepared_build=false -v max_zoom=3 -f bench/build_speed.sql
  ```

  Expected result: the script creates `_gp_bench_mixed`, `ST_BuildPyramid` returns `t`, and the final query prints rows from `geometry_pyramid_stats`. If `DATABASE_URL` is unavailable in the worker environment, record `Not-tested: benchmark requires a configured PostgreSQL database with the extension installed` in the task commit.

- [ ] **Step 4: Commit Task 1**

  ```bash
  git add bench/build_speed.sql README.md
  git commit -m "Add benchmark harness for full pyramid builds" \
    -m "The prepared build work needs a stable baseline before changing the build pipeline. This script creates mixed geometry data and records per-zoom build stats for baseline and prepared runs." \
    -m "Constraint: Benchmark is manual to avoid slowing default tests" \
    -m "Confidence: high" \
    -m "Scope-risk: narrow" \
    -m "Tested: psql benchmark smoke run or documented Not-tested gap"
  ```

### Task 2: `preparedBuild` Configuration

- Modify: `src/config.rs:14-296`
- Test: `cargo test config::tests`

- [ ] **Step 1: Run GitNexus impact before editing**

  Use GitNexus:

  ```text
  impact({target: "PyramidConfig", direction: "upstream", repo: "geometry_pyramid"})
  impact({target: "validate", direction: "upstream", repo: "geometry_pyramid"})
  ```

  If either result is HIGH or CRITICAL risk, stop and report before editing.

- [ ] **Step 2: Add the default constant in `src/config.rs`**

  Near the existing defaults, add:

  ```rust
  pub const DEFAULT_PREPARED_BUILD: bool = false;
  ```

- [ ] **Step 3: Add the struct field**

  In `PyramidConfig`, place this after `parallel`:

  ```rust
      /// Opt-in first-full-build speed path. When true and parallel=1, the build
      /// pre-projects source geometries into a temporary indexed table before
      /// slicing tiles. With parallel>1 the first implementation falls back to
      /// the existing worker path and emits a NOTICE.
      #[serde(default = "default_prepared_build", rename = "preparedBuild")]
      pub prepared_build: bool,
  ```

- [ ] **Step 4: Add the default value**

  In `impl Default for PyramidConfig`, place this after `parallel: DEFAULT_PARALLEL,`:

  ```rust
              prepared_build: DEFAULT_PREPARED_BUILD,
  ```

- [ ] **Step 5: Add the default function**

  Near `default_parallel`, add:

  ```rust
  fn default_prepared_build() -> bool {
      DEFAULT_PREPARED_BUILD
  }
  ```

- [ ] **Step 6: Update config tests**

  In `empty_config_uses_defaults`, add:

  ```rust
          assert!(!cfg.prepared_build);
  ```

  In `parses_known_keys`, change the JSON and assertion to:

  ```rust
          let cfg = PyramidConfig::parse(
              r#"{"sourceSRS":4490,"tileSize":512,"maxZoom":14,"parallel":4,"preparedBuild":true}"#,
          )
          .unwrap();
          assert_eq!(cfg.source_srs, 4490);
          assert_eq!(cfg.tile_size, 512);
          assert_eq!(cfg.max_zoom, 14);
          assert_eq!(cfg.parallel, 4);
          assert!(cfg.prepared_build);
  ```

- [ ] **Step 7: Verify config tests**

  Run:

  ```bash
  cargo test config::tests
  ```

  Expected result: all `config::tests` pass.

- [ ] **Step 8: Commit Task 2**

  ```bash
  git add src/config.rs
  git commit -m "Add opt-in prepared build configuration" \
    -m "The build-speed path needs a guarded switch before changing execution. preparedBuild defaults to false and can be enabled only by config." \
    -m "Constraint: Existing builds must keep current behavior by default" \
    -m "Confidence: high" \
    -m "Scope-risk: narrow" \
    -m "Tested: cargo test config::tests"
  ```

### Task 3: Remove Duplicate MVT Aggregation

- Modify: `src/build.rs:441-493`
- Test: `cargo test build::tests`, `cargo pgrx test` when available

- [ ] **Step 1: Run GitNexus impact before editing**

  Use GitNexus:

  ```text
  impact({target: "build_zoom_level", direction: "upstream", repo: "geometry_pyramid"})
  impact({target: "tile_pipeline_from_sql", direction: "upstream", repo: "geometry_pyramid"})
  ```

  If either result is HIGH or CRITICAL risk, stop and report before editing.

- [ ] **Step 2: Replace the SQL assembly in `build_zoom_level`**

  Keep the existing parameter list unchanged. Replace the `let sql = format!(...)` block with:

  ```rust
      let sql = format!(
          r#"WITH grouped AS (
               SELECT t.x, t.y,
                      count(*) AS feature_count,
                      ST_AsMVT(features.*, $2, $3, 'geom', 'fid') AS mvt
               {pipeline}
               GROUP BY t.x, t.y
             )
             INSERT INTO "{storage_table}" (z, x, y, format, srid, mvt)
             SELECT $1::smallint, x, y, 'pbf'::text, $8::integer, mvt
             FROM grouped
             WHERE feature_count >= {split_size}
               AND mvt IS NOT NULL"#
      );
  ```

  Do not change `split_size`, the 14 parameter bindings, or `inserted_count`.

- [ ] **Step 3: Verify unit tests**

  Run:

  ```bash
  cargo test build::tests
  ```

  Expected result: both build unit tests pass.

- [ ] **Step 4: Verify pg tests if PostgreSQL is configured**

  Run:

  ```bash
  cargo pgrx test
  ```

  Expected result: all pg tests pass. If the worker has no configured pgrx/PostgreSQL runtime, record the exact failure in `Not-tested`.

- [ ] **Step 5: Commit Task 3**

  ```bash
  git add src/build.rs
  git commit -m "Avoid repeated MVT aggregation during tile inserts" \
    -m "The existing INSERT computed ST_AsMVT in both SELECT and HAVING. Grouping once and filtering the computed mvt preserves splitSize behavior while removing duplicate aggregate work." \
    -m "Constraint: Keep existing output columns and parameter bindings unchanged" \
    -m "Confidence: medium" \
    -m "Scope-risk: moderate" \
    -m "Tested: cargo test build::tests; cargo pgrx test or documented Not-tested gap"
  ```

### Task 4: Prepared Build Support Module

- Create: `src/prepared.rs`
- Modify: `src/lib.rs:11-22`
- Modify: `src/build.rs:363-438`
- Test: `cargo test prepared::tests`, `cargo test build::tests`

- [ ] **Step 1: Run GitNexus impact before editing**

  Use GitNexus:

  ```text
  impact({target: "attribute_projections", direction: "upstream", repo: "geometry_pyramid"})
  impact({target: "tile_pipeline_from_sql", direction: "upstream", repo: "geometry_pyramid"})
  ```

  If either result is HIGH or CRITICAL risk, stop and report before editing.

- [ ] **Step 2: Register the new module**

  In `src/lib.rs`, add:

  ```rust
  mod prepared;
  ```

  Place it near the other internal modules, for example after `mod pyramid;`.

- [ ] **Step 3: Add generic attribute projection helper in `src/build.rs`**

  Replace `attribute_projections` with this pair:

  ```rust
  pub(crate) fn attribute_projections(cfg: &PyramidConfig) -> (String, String) {
      attribute_projections_for(cfg, "src", "prep")
  }

  pub(crate) fn attribute_projections_for(
      cfg: &PyramidConfig,
      inner_alias: &str,
      outer_alias: &str,
  ) -> (String, String) {
      let mut inner = String::new();
      let mut outer = String::new();
      for col in &cfg.attributes {
          if col.is_empty() || !col.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
              notice!(
                  "geometry_pyramid: attribute column {:?} is empty or has invalid characters; skipped",
                  col
              );
              continue;
          }
          inner.push_str(&format!(r#", {inner_alias}."{col}" AS "{col}""#));
          outer.push_str(&format!(r#", {outer_alias}."{col}""#));
      }
      (inner, outer)
  }
  ```

- [ ] **Step 4: Create `src/prepared.rs`**

  Add this file:

  ```rust
  //! Build-local source preparation for first full pyramid builds.
  //!
  //! The prepared table is intentionally session-local and only used by the
  //! coordinator backend (`parallel=1`). Dynamic tiles and incremental refreshes
  //! continue to read the source table directly.

  use crate::source::PyramidSource;
  use pgrx::prelude::*;
  use std::sync::atomic::{AtomicU32, Ordering};

  static PREP_COUNTER: AtomicU32 = AtomicU32::new(0);

  #[derive(Debug)]
  pub(crate) struct PreparedBuildSource {
      pub(crate) table: String,
  }

  pub(crate) fn temp_table_name(pyramid_name: &str) -> String {
      let pid = unsafe { pg_sys::MyProcPid } as u32;
      let seq = PREP_COUNTER.fetch_add(1, Ordering::Relaxed);
      let prefix: String = pyramid_name.chars().take(20).collect();
      format!("_gp_prep_{prefix}_{pid}_{seq}")
  }

  pub(crate) fn prepare_source_for_build(
      src: PyramidSource,
  ) -> Result<PreparedBuildSource, Box<dyn std::error::Error + Send + Sync>> {
      let table = temp_table_name(src.name);
      let index = format!("{table}_gix");
      let (attrs_inner, _) = crate::build::attribute_projections_for(src.cfg, "src", "prep");

      let create_sql = format!(
          r#"CREATE TEMP TABLE "{table}" ON COMMIT DROP AS
             SELECT
                src."{fid}"::bigint AS fid,
                ST_Transform(src."{geom}", $1) AS geom_tile,
                GeometryType(src."{geom}") AS geom_type
                {attrs_inner}
             FROM "{source}" src
             WHERE src."{geom}" IS NOT NULL"#,
          fid = src.fid_column,
          geom = src.geom_column,
          source = src.source_table,
      );
      Spi::run_with_args(&create_sql, &[src.cfg.tile_srs.into()])?;

      Spi::run(&format!(r#"CREATE INDEX "{index}" ON "{table}" USING gist (geom_tile)"#))?;
      Spi::run(&format!(r#"ANALYZE "{table}""#))?;

      Ok(PreparedBuildSource { table })
  }

  #[cfg(test)]
  mod tests {
      use super::*;

      #[test]
      fn temp_table_name_is_short_and_prefixed() {
          let name = temp_table_name("abcdefghijklmnopqrstuvwxyz0123456789");
          assert!(name.starts_with("_gp_prep_"));
          assert!(name.len() <= 63, "{name}");
          assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
      }
  }
  ```

- [ ] **Step 5: Add prepared tile pipeline constructor to `src/build.rs`**

  Below `tile_pipeline_from_sql`, add:

  ```rust
  pub(crate) fn tile_pipeline_from_prepared_sql(
      prep_table: &str,
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
                              AND prep_src.geom_type NOT IN ('POINT','MULTIPOINT')
                            THEN ST_SimplifyPreserveTopology(prep_src.geom_tile, $11::float8)
                         ELSE prep_src.geom_tile
                      END AS gtile,
                      prep_src.fid
                      {attrs_inner}
                   FROM "{prep_table}" prep_src
                   WHERE prep_src.geom_tile && ST_TileEnvelope(
                            $1, t.x, t.y, margin => ($9::float8 / $3::float8)
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

- [ ] **Step 6: Verify helper unit tests**

  Run:

  ```bash
  cargo test prepared::tests
  cargo test build::tests
  ```

  Expected result: prepared temp-name test and build unit tests pass.

- [ ] **Step 7: Commit Task 4**

  ```bash
  git add src/lib.rs src/prepared.rs src/build.rs
  git commit -m "Add build-local prepared source support" \
    -m "Prepared full builds need an isolated unit for temp table naming, source projection, indexing, and a prepared tile pipeline. Existing dynamic and source-table pipelines remain available." \
    -m "Constraint: Prepared tables are session-local and only intended for parallel=1 in this iteration" \
    -m "Confidence: medium" \
    -m "Scope-risk: moderate" \
    -m "Tested: cargo test prepared::tests; cargo test build::tests"
  ```

### Task 5: Wire Prepared Path Into Full Builds

- Modify: `src/build.rs:91-187`
- Test: new pg tests in `src/lib.rs`

- [ ] **Step 1: Run GitNexus impact before editing**

  Use GitNexus:

  ```text
  impact({target: "build_into", direction: "upstream", repo: "geometry_pyramid"})
  impact({target: "build_zoom_level_with_budget", direction: "upstream", repo: "geometry_pyramid"})
  impact({target: "run_zoom_strips", direction: "upstream", repo: "geometry_pyramid"})
  ```

  If any result is HIGH or CRITICAL risk, stop and report before editing.

- [ ] **Step 2: Create the optional prepared source once per full build**

  In `build_into`, before the zoom loop, add:

  ```rust
      let prepared = if src.cfg.prepared_build {
          if src.cfg.parallel.max(1) == 1 {
              Some(crate::prepared::prepare_source_for_build(src)?)
          } else {
              notice!(
                  "geometry_pyramid: preparedBuild=true is ignored when parallel > 1; using existing parallel build path"
              );
              None
          }
      } else {
          None
      };
  ```

  Then change the zoom call from:

  ```rust
          let result = build_zoom_level_with_budget(src, storage_table, &rect)?;
  ```

  to:

  ```rust
          let result = build_zoom_level_with_budget(src, storage_table, &rect, prepared.as_ref())?;
  ```

- [ ] **Step 3: Change `build_zoom_level_with_budget` signature**

  Change:

  ```rust
  fn build_zoom_level_with_budget(
      src: PyramidSource,
      storage_table: &str,
      rect: &TileRect,
  ) -> Result<ZoomBuildResult, Box<dyn std::error::Error + Send + Sync>> {
  ```

  to:

  ```rust
  fn build_zoom_level_with_budget(
      src: PyramidSource,
      storage_table: &str,
      rect: &TileRect,
      prepared: Option<&crate::prepared::PreparedBuildSource>,
  ) -> Result<ZoomBuildResult, Box<dyn std::error::Error + Send + Sync>> {
  ```

- [ ] **Step 4: Dispatch per zoom to prepared or existing path**

  In `build_zoom_level_with_budget`, replace:

  ```rust
          run_zoom_strips(src, storage_table, rect, &knobs)?;
  ```

  with:

  ```rust
          if let Some(prepared) = prepared {
              build_zoom_level_prepared(src, storage_table, rect, &knobs, prepared)?;
          } else {
              run_zoom_strips(src, storage_table, rect, &knobs)?;
          }
  ```

- [ ] **Step 5: Add `build_zoom_level_prepared`**

  Place this next to `build_zoom_level`:

  ```rust
  fn build_zoom_level_prepared(
      src: PyramidSource,
      storage_table: &str,
      rect: &TileRect,
      knobs: &ZoomKnobs,
      prepared: &crate::prepared::PreparedBuildSource,
  ) -> Result<i64, spi::Error> {
      let cfg = src.cfg;
      let z = rect.z;
      let (xmin_t, ymin_t, xmax_t, ymax_t) = (rect.xmin, rect.ymin, rect.xmax, rect.ymax);

      let simplify_tol_m = knobs.simplify_tol_m;
      let min_area_m2 = knobs.min_area_m2;
      let min_len_m = knobs.min_len_m;
      let drop_threshold = knobs.drop_threshold;

      let (attrs_inner, attrs_outer) =
          attribute_projections_for(cfg, "prep_src", "prep");
      let pipeline =
          tile_pipeline_from_prepared_sql(&prepared.table, &attrs_inner, &attrs_outer);
      let split_size = cfg.split_size;
      let sql = format!(
          r#"WITH grouped AS (
               SELECT t.x, t.y,
                      count(*) AS feature_count,
                      ST_AsMVT(features.*, $2, $3, 'geom', 'fid') AS mvt
               {pipeline}
               GROUP BY t.x, t.y
             )
             INSERT INTO "{storage_table}" (z, x, y, format, srid, mvt)
             SELECT $1::smallint, x, y, 'pbf'::text, $8::integer, mvt
             FROM grouped
             WHERE feature_count >= {split_size}
               AND mvt IS NOT NULL"#
      );

      let inserted_before = inserted_count(storage_table, z)?;
      Spi::run_with_args(
          &sql,
          &[
              (z as i32).into(),
              src.name.into(),
              (cfg.tile_size as i32).into(),
              (xmin_t as i32).into(),
              (xmax_t as i32).into(),
              (ymin_t as i32).into(),
              (ymax_t as i32).into(),
              cfg.tile_srs.into(),
              (cfg.tile_extend as i32).into(),
              cfg.source_srs.into(),
              simplify_tol_m.into(),
              min_area_m2.into(),
              min_len_m.into(),
              drop_threshold.into(),
          ],
      )?;
      let inserted_after = inserted_count(storage_table, z)?;
      Ok(inserted_after - inserted_before)
  }
  ```

- [ ] **Step 6: Run focused compile check**

  Run:

  ```bash
  cargo check
  ```

  Expected result: build succeeds.

- [ ] **Step 7: Commit Task 5 after tests from Task 6 are added**

  Do not commit this task until Task 6 pg tests are added and passing, because this task changes runtime behavior.

### Task 6: Prepared Build Behavior Tests

- Modify: `src/lib.rs:34-760`
- Test: `cargo pgrx test`

- [ ] **Step 1: Run GitNexus impact before editing**

  Use GitNexus:

  ```text
  impact({target: "build_fetch_drop_roundtrip", direction: "upstream", repo: "geometry_pyramid"})
  ```

  This is a test symbol; if GitNexus reports ambiguity, proceed after confirming it only refers to `src/lib.rs`.

- [ ] **Step 2: Add a helper to create mixed geometry rows**

  Inside `#[pg_schema] mod tests`, add this helper near other helpers:

  ```rust
      fn create_mixed_geometry_table(table: &str) {
          Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
          Spi::run(&format!(
              r#"CREATE TABLE "{table}" (
                   gid serial PRIMARY KEY,
                   kind text NOT NULL,
                   geom geometry(Geometry, 4326) NOT NULL
                 )"#
          ))
          .unwrap();
          Spi::run(&format!(
              r#"INSERT INTO "{table}" (kind, geom) VALUES
                   ('point', ST_SetSRID(ST_MakePoint(0, 0), 4326)),
                   ('point', ST_SetSRID(ST_MakePoint(0.1, 0.1), 4326)),
                   ('line', ST_SetSRID(ST_MakeLine(ST_MakePoint(-0.2, -0.2), ST_MakePoint(0.2, 0.2)), 4326)),
                   ('polygon', ST_SetSRID(ST_MakeEnvelope(-0.05, -0.05, 0.05, 0.05, 4326), 4326))"#
          ))
          .unwrap();
          Spi::run(&format!(r#"CREATE INDEX ON "{table}" USING gist (geom)"#)).unwrap();
      }
  ```

- [ ] **Step 3: Add prepared/default equivalence test**

  Add:

  ```rust
      #[pg_test]
      fn prepared_build_matches_default_for_mixed_geometries() {
          create_mixed_geometry_table("_gp_prep_mixed");

          let baseline = Spi::get_one::<bool>(
              "SELECT ST_BuildPyramid(
                   '_gp_prep_mixed','geom','gid',
                   '{\"name\":\"_gp_prep_base\",\"minZoom\":0,\"maxZoom\":2,\"splitSize\":0,\"attributes\":[\"kind\"]}'
               )",
          )
          .unwrap();
          assert_eq!(baseline, Some(true));

          let prepared = Spi::get_one::<bool>(
              "SELECT ST_BuildPyramid(
                   '_gp_prep_mixed','geom','gid',
                   '{\"name\":\"_gp_prep_on\",\"minZoom\":0,\"maxZoom\":2,\"splitSize\":0,\"attributes\":[\"kind\"],\"preparedBuild\":true}'
               )",
          )
          .unwrap();
          assert_eq!(prepared, Some(true));

          let baseline_count = Spi::get_one::<i64>(
              "SELECT count(*)::bigint FROM \"_gp_tiles__gp_prep_base\"",
          )
          .unwrap()
          .unwrap();
          let prepared_count = Spi::get_one::<i64>(
              "SELECT count(*)::bigint FROM \"_gp_tiles__gp_prep_on\"",
          )
          .unwrap()
          .unwrap();
          assert_eq!(prepared_count, baseline_count);

          let baseline_tile =
              Spi::get_one::<Vec<u8>>("SELECT ST_Tile('_gp_prep_base', 0, 0, 0)")
                  .unwrap()
                  .expect("baseline tile");
          let prepared_tile =
              Spi::get_one::<Vec<u8>>("SELECT ST_Tile('_gp_prep_on', 0, 0, 0)")
                  .unwrap()
                  .expect("prepared tile");
          assert!(!baseline_tile.is_empty());
          assert!(!prepared_tile.is_empty());
      }
  ```

- [ ] **Step 4: Add parallel fallback test**

  Add:

  ```rust
      #[pg_test]
      fn prepared_build_parallel_falls_back_to_existing_path() {
          create_mixed_geometry_table("_gp_prep_parallel");

          let ok = Spi::get_one::<bool>(
              "SELECT ST_BuildPyramid(
                   '_gp_prep_parallel','geom','gid',
                   '{\"name\":\"_gp_prep_parallel\",\"minZoom\":0,\"maxZoom\":1,\"splitSize\":0,\"preparedBuild\":true,\"parallel\":2}'
               )",
          )
          .unwrap();
          assert_eq!(ok, Some(true));

          let tile =
              Spi::get_one::<Vec<u8>>("SELECT ST_Tile('_gp_prep_parallel', 0, 0, 0)")
                  .unwrap()
                  .expect("parallel fallback tile");
          assert!(!tile.is_empty());
      }
  ```

- [ ] **Step 5: Run pg tests**

  Run:

  ```bash
  cargo pgrx test
  ```

  Expected result: all pg tests pass. If a byte-order issue appears, keep the tests at non-empty/count level rather than byte equality.

- [ ] **Step 6: Commit Tasks 5 and 6 together**

  ```bash
  git add src/build.rs src/lib.rs
  git commit -m "Route full builds through optional prepared source tables" \
    -m "preparedBuild now pre-projects source geometries into a temp GiST-indexed table for single-backend full builds. Parallel builds deliberately fall back to the existing worker path." \
    -m "Constraint: Dynamic tiles and incremental refreshes must keep reading the source table directly" \
    -m "Rejected: Worker-visible prepared tables in this iteration | needs separate cleanup and MVCC design" \
    -m "Confidence: medium" \
    -m "Scope-risk: moderate" \
    -m "Tested: cargo check; cargo pgrx test"
  ```

### Task 7: Documentation Updates

- Modify: `README.md:45-65`
- Test: Markdown review

- [ ] **Step 1: Add `preparedBuild` to the config table**

  In the README config table, add:

  ```markdown
  | `preparedBuild` | `false` | Opt-in first-full-build speed path. When true and `parallel` is 1, the builder pre-projects source geometries into a temporary indexed table before slicing. With `parallel` > 1, the first implementation falls back to the existing worker path. |
  ```

- [ ] **Step 2: Add a short usage note after the build example**

  Add:

  ```markdown
  For large first builds, benchmark `preparedBuild` on your data:

  ```sql
  SELECT ST_BuildPyramid(
      'buildings',
      'geom',
      'gid',
      '{"name":"buildings_fast","maxZoom":14,"preparedBuild":true}'
  );
  ```

  The option is disabled by default because the temporary projection/index step
  has a fixed setup cost. It is most useful when repeated per-tile transforms
  dominate the full build.
  ```

- [ ] **Step 3: Review markdown formatting**

  Run:

  ```bash
  sed -n '45,130p' README.md
  ```

  Expected result: table columns remain aligned enough to read, and fenced blocks are balanced.

- [ ] **Step 4: Commit Task 7**

  ```bash
  git add README.md
  git commit -m "Document prepared build benchmarking and configuration" \
    -m "preparedBuild is opt-in and data-dependent, so users need both the config key and the benchmark path documented before relying on it." \
    -m "Confidence: high" \
    -m "Scope-risk: narrow" \
    -m "Tested: README markdown section reviewed"
  ```

### Task 8: Final Verification and Scope Audit

- Modify: none unless verification finds defects
- Test: full verification suite

- [ ] **Step 1: Run formatting**

  Run:

  ```bash
  cargo fmt
  ```

  Expected result: command exits 0.

- [ ] **Step 2: Run compile check**

  Run:

  ```bash
  cargo check
  ```

  Expected result: command exits 0.

- [ ] **Step 3: Run unit tests**

  Run:

  ```bash
  cargo test
  ```

  Expected result: all Rust unit tests pass.

- [ ] **Step 4: Run pg tests**

  Run:

  ```bash
  cargo pgrx test
  ```

  Expected result: all pg tests pass.

- [ ] **Step 5: Run benchmark comparison if PostgreSQL is configured**

  Run:

  ```bash
  psql "$DATABASE_URL" -v pyramid_name=gp_bench_baseline -v prepared_build=false -v max_zoom=8 -f bench/build_speed.sql
  psql "$DATABASE_URL" -v pyramid_name=gp_bench_prepared -v prepared_build=true -v max_zoom=8 -f bench/build_speed.sql
  ```

  Expected result: both builds succeed and print comparable per-zoom stats. Do not require a fixed speedup percentage in this iteration.

- [ ] **Step 6: Run GitNexus changed-scope audit**

  Use GitNexus:

  ```text
  detect_changes({scope: "all", repo: "geometry_pyramid"})
  ```

  Expected result: changed symbols are limited to config, build/prepared pipeline, tests, benchmark docs, and README. If unrelated symbols appear, inspect before finalizing.

- [ ] **Step 7: Commit verification fixes only if needed**

  If verification required fixes, commit them with the Lore protocol. If no fixes were needed, do not create an empty commit.

## Execution Notes

- Do not change `ST_Tile`, `dynamic.rs`, `incremental.rs`, or worker internals for this iteration.
- Keep `preparedBuild` default `false` until benchmark evidence supports changing it.
- Preserve current `splitSize`, simplification, point-drop, and tile byte-budget semantics.
- Before any implementation edit to an indexed symbol, run GitNexus impact as listed in the task.
- Before final commit or PR, run GitNexus `detect_changes`.
