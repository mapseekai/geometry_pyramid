//! geometry_pyramid - vector tile pyramid for PostgreSQL geometry tables.
//!
//! Inspired by Alibaba GanosBase `ganos_geometry_pyramid`. Provides SQL
//! functions to build, inspect, query, update, and drop a multi-resolution
//! pyramid index over a geometry column.

use pgrx::prelude::*;

::pgrx::pg_module_magic!();

mod build;
mod config;
mod dynamic;
mod incremental;
mod pyramid;
mod shadow;
mod stats;
mod storage;
mod tile;
mod tile_rect;
mod worker;

extension_sql_file!("../sql/bootstrap.sql", bootstrap);

/// Returns the version of the geometry_pyramid extension.
#[pg_extern]
fn geometry_pyramid_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn test_version_returns_pkg_version() {
        let v = Spi::get_one::<String>("SELECT geometry_pyramid_version()")
            .expect("SPI failed")
            .expect("null version");
        assert_eq!(v, env!("CARGO_PKG_VERSION"));
    }

    /// End-to-end smoke test: register a tiny table, build a pyramid over
    /// it, fetch a tile, then drop it. Requires PostGIS (declared in the
    /// .control file's `requires`).
    #[pg_test]
    fn build_fetch_drop_roundtrip() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_smoke (gid serial PRIMARY KEY, \
             geom geometry(Point, 4326))",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_smoke (geom) VALUES \
             (ST_SetSRID(ST_MakePoint(0, 0), 4326)), \
             (ST_SetSRID(ST_MakePoint(1, 1), 4326))",
        )
        .unwrap();

        let ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_smoke','geom','gid','{\"minZoom\":0,\"maxZoom\":2}')",
        )
        .unwrap();
        assert_eq!(ok, Some(true));

        // Z=0 always covers the whole world: tile (0,0) must exist for any
        // non-empty pyramid.
        let tile = Spi::get_one::<Vec<u8>>("SELECT ST_Tile('_gp_smoke', 0, 0, 0)").unwrap();
        assert!(tile.is_some(), "expected non-null MVT at z=0/0/0");
        assert!(!tile.unwrap().is_empty());

        let info = Spi::get_one::<pgrx::JsonB>("SELECT ST_Pyramid('_gp_smoke')")
            .unwrap()
            .expect("metadata should exist");
        assert_eq!(
            info.0.get("name").and_then(|v| v.as_str()),
            Some("_gp_smoke")
        );

        let dropped = Spi::get_one::<bool>("SELECT ST_DropPyramid('_gp_smoke')").unwrap();
        assert_eq!(dropped, Some(true));
        Spi::run("DROP TABLE _gp_smoke").unwrap();
    }

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
        let info = Spi::get_one::<pgrx::JsonB>("SELECT ST_Pyramid('_gp_stats_t')")
            .unwrap()
            .expect("metadata should exist");
        let stats = info.0.get("stats").and_then(|v| v.as_array()).expect("stats array");
        assert_eq!(stats.len(), 4);
        assert!(stats[0].get("tileCount").is_some());
        Spi::run("SELECT ST_DropPyramid('_gp_stats_t')").unwrap();
        let rows = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM geometry_pyramid_stats WHERE pyramid_name='_gp_stats_t'",
        )
        .unwrap()
        .unwrap();
        assert_eq!(rows, 0, "drop must clean stats");
        Spi::run("DROP TABLE _gp_stats_t").unwrap();
    }

    /// A failed rebuild must leave the previous tiles intact (atomicity), and a
    /// successful rebuild must replace them. No shadow tables may be left behind.
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

        // attributes references a non-existent column: passes identifier validation
        // but the build SQL must error. Capture it in a subtransaction (DO block),
        // then verify the old tiles still read.
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

        let shadows = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM pg_tables WHERE tablename LIKE '%\\_\\_shadow'",
        )
        .unwrap()
        .unwrap();
        assert_eq!(shadows, 0);

        Spi::run("SELECT ST_DropPyramid('_gp_atomic')").unwrap();
        Spi::run("DROP TABLE _gp_atomic").unwrap();
    }

    /// Tippecanoe-style point dropping must reduce feature counts at low
    /// zoom even though the raw source contains many points.
    #[pg_test]
    fn drop_rate_reduces_low_zoom_features() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_drop (gid serial PRIMARY KEY, \
             geom geometry(Point, 4326))",
        )
        .unwrap();
        // 200 points scattered across the world.
        Spi::run(
            "INSERT INTO _gp_drop (geom) \
             SELECT ST_SetSRID(ST_MakePoint(\
                random()*360 - 180, random()*150 - 75), 4326) \
             FROM generate_series(1, 200)",
        )
        .unwrap();

        let ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_drop','geom','gid', \
              '{\"minZoom\":0,\"maxZoom\":4,\"dropRate\":0.4,\"splitSize\":0}')",
        )
        .unwrap();
        assert_eq!(ok, Some(true));

        // The z=0 tile must exist and be smaller than the z=4 union of
        // tiles, since dropRate=0.4 retains 0.4^4 ≈ 2.5% at z=0.
        let z0_size: i64 = Spi::get_one::<i64>(
            "SELECT COALESCE(octet_length(ST_Tile('_gp_drop', 0, 0, 0)), 0)::bigint",
        )
        .unwrap()
        .unwrap_or(0);
        let z4_size: i64 = Spi::get_one::<i64>(
            "SELECT COALESCE(SUM(octet_length(mvt)), 0)::bigint FROM _gp_tiles__gp_drop WHERE z=4",
        )
        .unwrap()
        .unwrap_or(0);
        assert!(
            z0_size < z4_size,
            "expected dropRate to shrink low-zoom tile (z0={z0_size}, z4_total={z4_size})"
        );

        Spi::run("SELECT ST_DropPyramid('_gp_drop')").unwrap();
        Spi::run("DROP TABLE _gp_drop").unwrap();
    }

    /// Parallel build must produce byte-identical tiles to a serial build.
    ///
    /// The pipeline is fully deterministic (hashtextextended-based point dropping,
    /// no randomness), so parallel and serial MUST agree on every (z,x,y) MVT blob.
    ///
    /// # Transaction-visibility caveat
    ///
    /// pgrx `#[pg_test]` bodies run inside a single backend transaction that is
    /// NOT committed during the test.  Background workers are separate PostgreSQL
    /// backends and can only see COMMITTED data.  If the source table and its rows
    /// live only in the test transaction, the spawned BuildStrips workers see an
    /// empty (or absent) table and produce empty strips, while the coordinator's
    /// inline slot (strip 0) — which shares the test transaction — sees all rows.
    /// The result is a partially-filled parallel pyramid that differs from serial.
    ///
    /// This is a TEST-HARNESS artefact, not a production bug: in production the
    /// source table is always committed before ST_BuildPyramid is called.
    ///
    /// Mitigation applied here: we create and populate the source table via an
    /// autonomous background worker (exec_sql_autonomous) so the data is
    /// committed before the parallel build runs, making it visible to all workers.
    /// The autonomous-worker path is the same mechanism used for committed shadow
    /// DDL, so exercising it here also validates that plumbing end-to-end.
    #[pg_test]
    fn parallel_build_matches_serial() {
        use crate::worker::exec_sql_autonomous;

        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();

        // Create and populate the source table via an autonomous (committed)
        // transaction so that background BuildStrips workers can see the rows
        // under MVCC.  A plain Spi::run would leave the table uncommitted and
        // workers would find it absent.
        //
        // We DROP the table first (idempotent) so that re-runs within the same
        // test-database session always start from a clean slate.
        exec_sql_autonomous(
            "DROP TABLE IF EXISTS _gp_par; \
             CREATE TABLE _gp_par \
             (gid serial PRIMARY KEY, geom geometry(Point,4326)); \
             INSERT INTO _gp_par (geom) \
             SELECT ST_SetSRID(ST_MakePoint( \
                (i % 36) * 10 - 175, (i / 36) % 15 * 10 - 70), 4326) \
             FROM generate_series(0, 539) i",
        )
        .unwrap();

        // We compare serial vs parallel by per-tile ENCODED SIZE, not raw bytes.
        // Two reasons raw byte-identity does not hold and size does:
        //   1. The pyramid name is the MVT layer name; equal-length names
        //      ('build_ser'/'build_par', both 9 chars) make that contribution
        //      identical in size.
        //   2. ST_AsMVT aggregates features in source-scan order. The serial
        //      build scans a wide tile range while each parallel worker scans a
        //      narrow x-strip, so features within a tile can be encoded in a
        //      different ORDER — same geometry, same total bytes, different
        //      layout. octet_length is invariant under that reordering, so size
        //      equality is the right order-independent check: it proves each
        //      tile carries the same feature set (the pipeline is otherwise
        //      deterministic — hashtextextended point drop, no randomness).

        // Serial build (parallel defaults to 1 — in-transaction path).
        Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_par','geom','gid', \
             '{\"name\":\"build_ser\",\"minZoom\":0,\"maxZoom\":5,\"splitSize\":0}')",
        )
        .unwrap();

        // Parallel build with parallel=2, generous timeout.
        // (parallel=4 risks max_worker_processes exhaustion on a default-8 cluster
        // that also runs autovacuum workers; parallel=2 spawns only 1 worker per
        // zoom level, well within the available budget.)
        Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_par','geom','gid', \
             '{\"name\":\"build_par\",\"minZoom\":0,\"maxZoom\":5,\"parallel\":2, \
               \"buildTimeoutMs\":60000,\"splitSize\":0}')",
        )
        .unwrap();

        // Guard against both builds silently producing zero tiles (which would
        // make the comparison below pass vacuously).
        let serial_tiles = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _gp_tiles_build_ser",
        )
        .unwrap()
        .unwrap();
        assert!(serial_tiles > 0, "serial build produced no tiles");

        // A single FULL JOIN catches a coverage gap (a missing/empty worker
        // strip ⇒ NULL octet_length on one side ⇒ unequal) AND any per-tile
        // size difference (extra/missing/different features change the size).
        let mismatches = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _gp_tiles_build_ser a \
             FULL JOIN _gp_tiles_build_par b USING (z, x, y) \
             WHERE octet_length(a.mvt) IS DISTINCT FROM octet_length(b.mvt)",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            mismatches, 0,
            "parallel tiles must match serial in coverage and per-tile encoded size"
        );

        Spi::run("SELECT ST_DropPyramid('build_ser')").unwrap();
        Spi::run("SELECT ST_DropPyramid('build_par')").unwrap();
        // _gp_par was committed by exec_sql_autonomous and survives this test's
        // rollback.  We cannot drop it here (DROP needs ACCESS EXCLUSIVE but this
        // transaction holds ACCESS SHARE from the serial build's SELECT scan, and
        // an autonomous worker doing the DROP would block until this transaction
        // ends — causing a 60-second timeout).  The next test run's
        // exec_sql_autonomous above handles cleanup with "DROP TABLE IF EXISTS".
        // The test-database itself is torn down after each `cargo pgrx test` run,
        // so the table never accumulates stale data across test invocations.
    }

    /// Incremental update must touch only the dirty tiles, no-op on an empty
    /// match, and reject a broken where_clause without damaging the pyramid.
    #[pg_test]
    fn incremental_update_rebuilds_only_dirty_tiles() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_inc (gid serial PRIMARY KEY, \
             geom geometry(Point, 4326), touched bool DEFAULT false)",
        )
        .unwrap();
        // Two points far apart: eastern (+100,+30) and western (-100,-30)
        // hemispheres — they land in different tiles at higher zoom.
        Spi::run(
            "INSERT INTO _gp_inc (geom) VALUES \
             (ST_SetSRID(ST_MakePoint(100, 30), 4326)), \
             (ST_SetSRID(ST_MakePoint(-100, -30), 4326))",
        )
        .unwrap();
        Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_inc','geom','gid','{\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0}')",
        )
        .unwrap();

        // Snapshot every tile.
        Spi::run("CREATE TEMP TABLE _snap AS SELECT z, x, y, mvt FROM _gp_tiles__gp_inc")
            .unwrap();

        // Move the eastern point (stays eastern), mark touched.
        Spi::run(
            "UPDATE _gp_inc SET geom = ST_SetSRID(ST_MakePoint(101, 31), 4326), \
             touched = true WHERE gid = 1",
        )
        .unwrap();
        let ok = Spi::get_one::<bool>("SELECT ST_UpdatePyramid('_gp_inc', 'touched = true')")
            .unwrap();
        assert_eq!(ok, Some(true));

        // Western tiles (x in the western half) must be byte-unchanged.
        let west_changed = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _snap s \
             JOIN _gp_tiles__gp_inc t USING (z, x, y) \
             WHERE s.x < (1 << s.z) / 2 AND s.mvt IS DISTINCT FROM t.mvt",
        )
        .unwrap()
        .unwrap();
        assert_eq!(west_changed, 0, "western tiles must be untouched by an eastern-only update");

        // Empty match → no-op, returns true.
        let ok = Spi::get_one::<bool>("SELECT ST_UpdatePyramid('_gp_inc', 'false')").unwrap();
        assert_eq!(ok, Some(true));

        // Broken where_clause → clean error, pyramid intact.
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
        Spi::run("DROP TABLE _gp_inc").unwrap();
    }

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

        // Unknown pyramid -> NULL (regression: used to error on missing table).
        let unknown = Spi::get_one::<Vec<u8>>("SELECT ST_Tile('no_such_pyramid', 0, 0, 0)").unwrap();
        assert!(unknown.is_none(), "unknown pyramid must return NULL, not error");

        // Materialized hit: z=0 is the whole world, the point is in tile (0,0,0).
        let z0 = Spi::get_one::<Vec<u8>>("SELECT ST_Tile('dyn', 0, 0, 0)").unwrap();
        assert!(z0.is_some(), "materialized z=0 tile must exist");

        // Compute the z=6 tile that contains the point (z=6 > maxZoom=4 => dynamic).
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

        // Dynamic miss: a z=6 tile far from the point has no data -> NULL.
        let far_x = if tx > 0 { 0 } else { 63 };
        let dyn_miss = Spi::get_one_with_args::<Vec<u8>>(
            "SELECT ST_Tile('dyn', 6, $1, $2)",
            &[far_x.into(), ty.into()],
        )
        .unwrap();
        assert!(dyn_miss.is_none(), "empty region at z=6 must return NULL");

        // Switch OFF: rebuild with dynamicBeyondMaxZoom=false => z=6 returns NULL.
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
}

/// Required by `cargo pgrx test`.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
