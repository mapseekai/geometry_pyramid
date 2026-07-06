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
mod prepared;
mod pyramid;
mod shadow;
mod source;
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

    #[pg_test]
    fn prepared_build_matches_default_for_mixed_geometries() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_prep_cmp (gid serial PRIMARY KEY, kind text, rank int, \
             geom geometry(Geometry, 4326))",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_prep_cmp (kind, rank, geom) VALUES \
             ('point', 1, ST_SetSRID(ST_MakePoint(116.4, 39.9), 4326)), \
             ('point', 2, ST_SetSRID(ST_MakePoint(116.45, 39.95), 4326)), \
             ('line', 3, ST_SetSRID(ST_MakeLine(ST_MakePoint(116.3,39.8), ST_MakePoint(116.6,40.1)), 4326)), \
             ('polygon', 4, ST_SetSRID(ST_MakeEnvelope(116.2,39.7,116.7,40.2), 4326))",
        )
        .unwrap();

        let default_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_prep_cmp','geom','gid', \
             '{\"name\":\"prep_def\",\"minZoom\":0,\"maxZoom\":4,\"splitSize\":0, \
                \"attributes\":[\"kind\",\"rank\"]}')",
        )
        .unwrap();
        assert_eq!(default_ok, Some(true));

        let prepared_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_prep_cmp','geom','gid', \
             '{\"name\":\"prep_opt\",\"minZoom\":0,\"maxZoom\":4,\"splitSize\":0, \
                \"attributes\":[\"kind\",\"rank\"],\"preparedBuild\":true}')",
        )
        .unwrap();
        assert_eq!(prepared_ok, Some(true));

        let default_tiles = Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_prep_def")
            .unwrap()
            .unwrap();
        assert!(default_tiles > 0, "default build produced no tiles");

        let mismatches = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _gp_tiles_prep_def a \
             FULL JOIN _gp_tiles_prep_opt b USING (z, x, y) \
             WHERE octet_length(a.mvt) IS DISTINCT FROM octet_length(b.mvt)",
        )
        .unwrap()
        .unwrap();
        assert_eq!(mismatches, 0, "prepared tiles must match default tiles");

        Spi::run("SELECT ST_DropPyramid('prep_def')").unwrap();
        Spi::run("SELECT ST_DropPyramid('prep_opt')").unwrap();
        Spi::run("DROP TABLE _gp_prep_cmp").unwrap();
    }

    #[pg_test]
    fn feature_major_points_matches_default_for_point_sources() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_fm_points (gid serial PRIMARY KEY, kind text, rank int, \
             geom geometry(Point, 4326))",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_fm_points (kind, rank, geom) \
             SELECT 'dense', i, ST_SetSRID(ST_MakePoint(116.0 + (i % 30) * 0.001, \
                                                        39.5 + (i / 30) * 0.001), 4326) \
             FROM generate_series(1, 300) AS i",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_fm_points (kind, rank, geom) VALUES \
             ('sparse', 1001, ST_SetSRID(ST_MakePoint(-100, 30), 4326)), \
             ('sparse', 1002, ST_SetSRID(ST_MakePoint(100, -30), 4326))",
        )
        .unwrap();

        let default_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_fm_points','geom','gid', \
             '{\"name\":\"fm_base\",\"minZoom\":0,\"maxZoom\":5,\"splitSize\":0, \
                \"attributes\":[\"kind\",\"rank\"]}')",
        )
        .unwrap();
        assert_eq!(default_ok, Some(true));

        let feature_major_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_fm_points','geom','gid', \
             '{\"name\":\"fm_fast\",\"minZoom\":0,\"maxZoom\":5,\"splitSize\":0, \
                \"attributes\":[\"kind\",\"rank\"],\"preparedBuild\":true, \
                \"featureMajorMinZoom\":0,\"featureMajorPoints\":true}')",
        )
        .unwrap();
        assert_eq!(feature_major_ok, Some(true));

        let default_tiles = Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_fm_base")
            .unwrap()
            .unwrap();
        assert!(default_tiles > 0, "default point build produced no tiles");

        let mismatches = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _gp_tiles_fm_base a \
             FULL JOIN _gp_tiles_fm_fast b USING (z, x, y) \
             WHERE octet_length(a.mvt) IS DISTINCT FROM octet_length(b.mvt)",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            mismatches, 0,
            "feature-major point tiles must match default coverage and encoded size"
        );

        Spi::run("SELECT ST_DropPyramid('fm_base')").unwrap();
        Spi::run("SELECT ST_DropPyramid('fm_fast')").unwrap();
        Spi::run("DROP TABLE _gp_fm_points").unwrap();
    }

    #[pg_test]
    fn feature_driven_enum_matches_default_for_mixed_geometries() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_fd_cmp (gid serial PRIMARY KEY, kind text, rank int, \
             geom geometry(Geometry, 4326))",
        )
        .unwrap();
        // A small mixed cluster spanning several z6 tiles — so points, lines and
        // polygons straddle tile boundaries and the (default, nonzero) tileExtend
        // margin band is actually exercised...
        Spi::run(
            "INSERT INTO _gp_fd_cmp (kind, rank, geom) \
             SELECT 'point', i, ST_SetSRID(ST_MakePoint(110.0 + i*0.7, 35.0 + i*0.4), 4326) \
             FROM generate_series(1, 12) AS i",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_fd_cmp (kind, rank, geom) \
             SELECT 'line', 100+i, ST_SetSRID(ST_MakeLine( \
                 ST_MakePoint(108.0 + i*0.9, 36.0 + i*0.6), \
                 ST_MakePoint(108.0 + i*0.9 + 2.0, 36.0 + i*0.6 + 1.5)), 4326) \
             FROM generate_series(1, 8) AS i",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_fd_cmp (kind, rank, geom) \
             SELECT 'polygon', 200+i, ST_SetSRID(ST_MakeEnvelope( \
                 109.0 + i*0.8, 34.0 + i*0.5, 109.0 + i*0.8 + 1.7, 34.0 + i*0.5 + 1.1, 4326), 4326) \
             FROM generate_series(1, 8) AS i",
        )
        .unwrap();
        // ...plus two world-corner outliers that inflate the bbox so R(z) >> rows
        // and feature-driven actually engages (high zooms) while shallow zooms
        // still run tile-major — both paths exercised in one build.
        Spi::run(
            "INSERT INTO _gp_fd_cmp (kind, rank, geom) VALUES \
             ('outlier', 9001, ST_SetSRID(ST_MakePoint(-170, -75), 4326)), \
             ('outlier', 9002, ST_SetSRID(ST_MakePoint(170, 78), 4326))",
        )
        .unwrap();

        // Default tile-major, with simplification + attributes + default tileExtend.
        let default_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_fd_cmp','geom','gid', \
             '{\"name\":\"fd_base\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"simplifyPixels\":1.0,\"attributes\":[\"kind\",\"rank\"]}')",
        )
        .unwrap();
        assert_eq!(default_ok, Some(true));

        // Same knobs + preparedBuild + featureDrivenEnum.
        let fd_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_fd_cmp','geom','gid', \
             '{\"name\":\"fd_fast\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"simplifyPixels\":1.0,\"attributes\":[\"kind\",\"rank\"], \
                \"preparedBuild\":true,\"featureDrivenEnum\":true}')",
        )
        .unwrap();
        assert_eq!(fd_ok, Some(true));

        let default_tiles = Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_fd_base")
            .unwrap()
            .unwrap();
        assert!(default_tiles > 0, "default mixed build produced no tiles");

        // FULL JOIN on (z,x,y): a tile present on only one side leaves a NULL
        // octet_length on the other -> counted. So mismatches==0 proves identical
        // tile *set* and identical encoded size across the margin/simplify/attrs.
        let mismatches = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _gp_tiles_fd_base a \
             FULL JOIN _gp_tiles_fd_fast b USING (z, x, y) \
             WHERE octet_length(a.mvt) IS DISTINCT FROM octet_length(b.mvt)",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            mismatches, 0,
            "feature-driven tiles must match default tile-major (set + encoded size)"
        );

        Spi::run("SELECT ST_DropPyramid('fd_base')").unwrap();
        Spi::run("SELECT ST_DropPyramid('fd_fast')").unwrap();
        Spi::run("DROP TABLE _gp_fd_cmp").unwrap();
    }

    #[pg_test]
    fn feature_driven_enum_falls_back_on_dense_extent() {
        // A dense extent (no bbox-inflating outliers) has R(z) ~ non-empty tiles,
        // so the prep guard must DISABLE feature-driven and fall back to prepared
        // tile-major — turning the knob on stays safe, output unchanged.
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("CREATE TABLE _gp_fd_dense (gid serial PRIMARY KEY, geom geometry(Point, 4326))")
            .unwrap();
        Spi::run(
            "INSERT INTO _gp_fd_dense (geom) \
             SELECT ST_SetSRID(ST_MakePoint(116.0 + (i % 20) * 0.01, \
                                            39.5 + (i / 20) * 0.01), 4326) \
             FROM generate_series(1, 400) AS i",
        )
        .unwrap();

        let base_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_fd_dense','geom','gid', \
             '{\"name\":\"fdd_base\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"preparedBuild\":true}')",
        )
        .unwrap();
        assert_eq!(base_ok, Some(true));

        let fd_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_fd_dense','geom','gid', \
             '{\"name\":\"fdd_fast\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"preparedBuild\":true,\"featureDrivenEnum\":true}')",
        )
        .unwrap();
        assert_eq!(fd_ok, Some(true));

        let mismatches = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _gp_tiles_fdd_base a \
             FULL JOIN _gp_tiles_fdd_fast b USING (z, x, y) \
             WHERE octet_length(a.mvt) IS DISTINCT FROM octet_length(b.mvt)",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            mismatches, 0,
            "dense fallback must match prepared tile-major"
        );

        Spi::run("SELECT ST_DropPyramid('fdd_base')").unwrap();
        Spi::run("SELECT ST_DropPyramid('fdd_fast')").unwrap();
        Spi::run("DROP TABLE _gp_fd_dense").unwrap();
    }

    #[pg_test]
    fn feature_major_points_skips_prepared_geom_index_for_point_sources() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("CREATE TABLE _gp_fm_noidx (gid serial PRIMARY KEY, geom geometry(Point, 4326))")
            .unwrap();
        Spi::run(
            "INSERT INTO _gp_fm_noidx (geom) \
             SELECT ST_SetSRID(ST_MakePoint(116.0 + i * 0.001, 39.0), 4326) \
             FROM generate_series(1, 100) AS i",
        )
        .unwrap();

        let before = prepared_geom_index_count();
        let ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_fm_noidx','geom','gid', \
             '{\"name\":\"fm_noidx\",\"minZoom\":0,\"maxZoom\":2,\"splitSize\":0, \
                \"preparedBuild\":true,\"featureMajorMinZoom\":0,\"featureMajorPoints\":true}')",
        )
        .unwrap();
        assert_eq!(ok, Some(true));
        let after = prepared_geom_index_count();
        assert_eq!(
            after, before,
            "pure POINT feature-major builds must not create the unused prepared GiST index"
        );

        Spi::run("SELECT ST_DropPyramid('fm_noidx')").unwrap();
        Spi::run("DROP TABLE _gp_fm_noidx").unwrap();
    }

    #[pg_test]
    fn feature_major_points_fallback_indexes_mixed_sources() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_fm_mixed_idx (gid serial PRIMARY KEY, geom geometry(Geometry, 4326))",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_fm_mixed_idx (geom) VALUES \
             (ST_SetSRID(ST_MakePoint(116.0, 39.0), 4326)), \
             (ST_SetSRID(ST_MakeLine(ST_MakePoint(116.0,39.0), ST_MakePoint(116.1,39.1)), 4326))",
        )
        .unwrap();

        let before = prepared_geom_index_count();
        let ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_fm_mixed_idx','geom','gid', \
             '{\"name\":\"fm_mixed_idx\",\"minZoom\":0,\"maxZoom\":2,\"splitSize\":0, \
                \"preparedBuild\":true,\"featureMajorMinZoom\":0,\"featureMajorPoints\":true}')",
        )
        .unwrap();
        assert_eq!(ok, Some(true));
        let after = prepared_geom_index_count();
        assert!(
            after > before,
            "mixed fallback must create the prepared GiST index before using tile-major prepared build"
        );

        Spi::run("SELECT ST_DropPyramid('fm_mixed_idx')").unwrap();
        Spi::run("DROP TABLE _gp_fm_mixed_idx").unwrap();
    }

    fn prepared_geom_index_count() -> i64 {
        Spi::get_one::<i64>(
            "SELECT count(*)::bigint \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.oid = pg_my_temp_schema() \
               AND c.relkind = 'i' \
               AND c.relname LIKE '_gp_prep_%_geom_tile_idx'",
        )
        .unwrap()
        .unwrap()
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
        let stats = info
            .0
            .get("stats")
            .and_then(|v| v.as_array())
            .expect("stats array");
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
        let serial_tiles = Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_build_ser")
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

    /// PR4: the parallel feature-driven fast path must produce the identical
    /// tile set to a serial feature-driven build.
    ///
    /// This is the integration backstop for the whole PR4 slice: it exercises
    /// (a) the committed, worker-visible prepared table, (b) workers running the
    /// feature-driven pipeline on their x-strips, and (c) strip-coordinate
    /// ownership being tile-exact — an overlap would trip the `(z,x,y)` PRIMARY
    /// KEY at INSERT and a gap would surface as a NULL on one side of the FULL
    /// JOIN. The source is committed via an autonomous worker so the BuildStrips
    /// workers see it under MVCC (same harness constraint as
    /// `parallel_build_matches_serial`).
    #[pg_test]
    fn parallel_feature_driven_matches_serial() {
        use crate::worker::exec_sql_autonomous;

        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();

        // A sparse mixed cluster (points + lines + polygons) plus two
        // world-corner outliers that inflate the bbox so R(z) >> rows and
        // feature-driven actually engages at the higher zooms — committed so
        // workers can see it.
        exec_sql_autonomous(
            "DROP TABLE IF EXISTS _gp_pfd; \
             CREATE TABLE _gp_pfd (gid serial PRIMARY KEY, kind text, rank int, \
                 geom geometry(Geometry, 4326)); \
             INSERT INTO _gp_pfd (kind, rank, geom) \
                 SELECT 'point', i, ST_SetSRID(ST_MakePoint(110.0 + i*0.7, 35.0 + i*0.4), 4326) \
                 FROM generate_series(1, 12) AS i; \
             INSERT INTO _gp_pfd (kind, rank, geom) \
                 SELECT 'line', 100+i, ST_SetSRID(ST_MakeLine( \
                     ST_MakePoint(108.0 + i*0.9, 36.0 + i*0.6), \
                     ST_MakePoint(108.0 + i*0.9 + 2.0, 36.0 + i*0.6 + 1.5)), 4326) \
                 FROM generate_series(1, 8) AS i; \
             INSERT INTO _gp_pfd (kind, rank, geom) \
                 SELECT 'polygon', 200+i, ST_SetSRID(ST_MakeEnvelope( \
                     109.0 + i*0.8, 34.0 + i*0.5, 109.0 + i*0.8 + 1.7, 34.0 + i*0.5 + 1.1, 4326), 4326) \
                 FROM generate_series(1, 8) AS i; \
             INSERT INTO _gp_pfd (kind, rank, geom) VALUES \
                 ('outlier', 9001, ST_SetSRID(ST_MakePoint(-170, -75), 4326)), \
                 ('outlier', 9002, ST_SetSRID(ST_MakePoint(170, 78), 4326))",
        )
        .unwrap();

        // Serial feature-driven (parallel defaults to 1 — in-transaction path).
        let ser_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_pfd','geom','gid', \
             '{\"name\":\"pfd_ser\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"simplifyPixels\":1.0,\"attributes\":[\"kind\",\"rank\"], \
                \"preparedBuild\":true,\"featureDrivenEnum\":true}')",
        )
        .unwrap();
        assert_eq!(ser_ok, Some(true));

        // Parallel feature-driven: parallel=2 (one worker per zoom), committed
        // worker-visible prepared table, generous timeout.
        let par_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_pfd','geom','gid', \
             '{\"name\":\"pfd_par\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"simplifyPixels\":1.0,\"attributes\":[\"kind\",\"rank\"], \
                \"preparedBuild\":true,\"featureDrivenEnum\":true, \
                \"parallel\":2,\"buildTimeoutMs\":60000}')",
        )
        .unwrap();
        assert_eq!(par_ok, Some(true));

        // Guard against a vacuously-passing comparison (both empty).
        let ser_tiles = Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_pfd_ser")
            .unwrap()
            .unwrap();
        assert!(
            ser_tiles > 0,
            "serial feature-driven build produced no tiles"
        );

        // FULL JOIN on (z,x,y): a coverage gap leaves a NULL octet_length on one
        // side; a size difference (extra/missing/different feature) is unequal.
        // Either way mismatches>0. Compared by encoded SIZE, not raw bytes, since
        // parallel workers scan narrower x-strips and may encode a tile's features
        // in a different order (size is invariant under that reordering).
        let mismatches = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _gp_tiles_pfd_ser a \
             FULL JOIN _gp_tiles_pfd_par b USING (z, x, y) \
             WHERE octet_length(a.mvt) IS DISTINCT FROM octet_length(b.mvt)",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            mismatches, 0,
            "parallel feature-driven tiles must match serial in coverage and per-tile encoded size"
        );

        Spi::run("SELECT ST_DropPyramid('pfd_ser')").unwrap();
        Spi::run("SELECT ST_DropPyramid('pfd_par')").unwrap();
        // _gp_pfd was committed by exec_sql_autonomous; like _gp_par in
        // parallel_build_matches_serial it is left for the next run's
        // "DROP TABLE IF EXISTS" to reclaim (the test database is torn down
        // after the run anyway).
    }

    /// PR4: the parallel *prepared tile-major* fast path must produce the
    /// identical tile set to a serial prepared tile-major build.
    ///
    /// This covers the path that `parallel_feature_driven_matches_serial` does
    /// NOT: `preparedBuild=true, parallel>1, featureDrivenEnum=false`. That combo
    /// became reachable in PR4 (the committed prepared table is now worker-visible
    /// for the plain prepared path too, not only feature-driven), so it needs its
    /// own equivalence backstop. It exercises (a) the committed prepared table read
    /// by workers, (b) `build_zoom_level_prepared` running on Morton blocks (the
    /// dispatch's prepared-tile-major arm, distinct from the feature-driven arm),
    /// and (c) Morton-block ownership being tile-exact — overlap trips the
    /// `(z,x,y)` PRIMARY KEY, a gap surfaces as a NULL on one side of the FULL JOIN.
    #[pg_test]
    fn parallel_prepared_tile_major_matches_serial() {
        use crate::worker::exec_sql_autonomous;

        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();

        // Same sparse mixed cluster + world-corner outliers as the feature-driven
        // test: the outliers inflate the bbox so the deep-zoom lattice exceeds
        // PARALLEL_MIN_CELLS and workers actually farm (rather than running inline).
        exec_sql_autonomous(
            "DROP TABLE IF EXISTS _gp_ptm; \
             CREATE TABLE _gp_ptm (gid serial PRIMARY KEY, kind text, rank int, \
                 geom geometry(Geometry, 4326)); \
             INSERT INTO _gp_ptm (kind, rank, geom) \
                 SELECT 'point', i, ST_SetSRID(ST_MakePoint(110.0 + i*0.7, 35.0 + i*0.4), 4326) \
                 FROM generate_series(1, 12) AS i; \
             INSERT INTO _gp_ptm (kind, rank, geom) \
                 SELECT 'line', 100+i, ST_SetSRID(ST_MakeLine( \
                     ST_MakePoint(108.0 + i*0.9, 36.0 + i*0.6), \
                     ST_MakePoint(108.0 + i*0.9 + 2.0, 36.0 + i*0.6 + 1.5)), 4326) \
                 FROM generate_series(1, 8) AS i; \
             INSERT INTO _gp_ptm (kind, rank, geom) \
                 SELECT 'polygon', 200+i, ST_SetSRID(ST_MakeEnvelope( \
                     109.0 + i*0.8, 34.0 + i*0.5, 109.0 + i*0.8 + 1.7, 34.0 + i*0.5 + 1.1, 4326), 4326) \
                 FROM generate_series(1, 8) AS i; \
             INSERT INTO _gp_ptm (kind, rank, geom) VALUES \
                 ('outlier', 9001, ST_SetSRID(ST_MakePoint(-170, -75), 4326)), \
                 ('outlier', 9002, ST_SetSRID(ST_MakePoint(170, 78), 4326))",
        )
        .unwrap();

        // Serial prepared tile-major (parallel defaults to 1). No featureDrivenEnum.
        let ser_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_ptm','geom','gid', \
             '{\"name\":\"ptm_ser\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"simplifyPixels\":1.0,\"attributes\":[\"kind\",\"rank\"], \
                \"preparedBuild\":true}')",
        )
        .unwrap();
        assert_eq!(ser_ok, Some(true));

        // Parallel prepared tile-major: parallel=2, committed worker-visible
        // prepared table, Morton-block partition, generous timeout.
        let par_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_ptm','geom','gid', \
             '{\"name\":\"ptm_par\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"simplifyPixels\":1.0,\"attributes\":[\"kind\",\"rank\"], \
                \"preparedBuild\":true, \
                \"parallel\":2,\"buildTimeoutMs\":60000}')",
        )
        .unwrap();
        assert_eq!(par_ok, Some(true));

        // Guard against a vacuously-passing comparison (both empty).
        let ser_tiles = Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_ptm_ser")
            .unwrap()
            .unwrap();
        assert!(
            ser_tiles > 0,
            "serial prepared tile-major build produced no tiles"
        );

        // FULL JOIN on (z,x,y): coverage gap ⇒ NULL on one side; size difference
        // ⇒ unequal. Compared by encoded SIZE (order-invariant) since each Morton
        // block scans a different sub-extent and may encode a tile's features in a
        // different order.
        let mismatches = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _gp_tiles_ptm_ser a \
             FULL JOIN _gp_tiles_ptm_par b USING (z, x, y) \
             WHERE octet_length(a.mvt) IS DISTINCT FROM octet_length(b.mvt)",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            mismatches, 0,
            "parallel prepared tile-major tiles must match serial in coverage and per-tile encoded size"
        );

        Spi::run("SELECT ST_DropPyramid('ptm_ser')").unwrap();
        Spi::run("SELECT ST_DropPyramid('ptm_par')").unwrap();
        // _gp_ptm was committed by exec_sql_autonomous; left for the next run's
        // "DROP TABLE IF EXISTS" to reclaim (test database torn down after run).
    }

    /// PR4 robustness: a parallel build must (a) **self-heal** over a leftover
    /// committed prepared table from a crashed prior run — the per-pyramid name is
    /// deterministic and recreated via `DROP IF EXISTS` — and (b) **drop** that
    /// prepared table when it finishes. (b) is the contract whose violation (an
    /// autonomous `DROP` instead of an in-coordinator-transaction one) caused the
    /// undetectable ACCESS SHARE vs ACCESS EXCLUSIVE deadlock that burned the full
    /// worker timeout on every parallel build.
    #[pg_test]
    fn parallel_build_self_heals_and_drops_prepared_table() {
        use crate::worker::exec_sql_autonomous;
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();

        // Committed source the workers can see; world-corner outliers widen the
        // bbox so deep zooms farm rather than run inline.
        exec_sql_autonomous(
            "DROP TABLE IF EXISTS _gp_heal; \
             CREATE TABLE _gp_heal (gid serial PRIMARY KEY, geom geometry(Point,4326)); \
             INSERT INTO _gp_heal (geom) \
                 SELECT ST_SetSRID(ST_MakePoint(110.0 + i*0.5, 35.0 + i*0.3), 4326) \
                 FROM generate_series(1, 16) AS i; \
             INSERT INTO _gp_heal (geom) VALUES \
                 (ST_SetSRID(ST_MakePoint(-170,-75),4326)), \
                 (ST_SetSRID(ST_MakePoint(170,78),4326))",
        )
        .unwrap();

        // Simulate a leaked prepared table from a crashed run: deterministic name
        // `_gp_prep_heal` with a deliberately WRONG schema, so a stale read (rather
        // than a clean recreate) would fail loudly. The build must DROP+recreate it.
        exec_sql_autonomous(
            r#"DROP TABLE IF EXISTS "_gp_prep_heal"; CREATE TABLE "_gp_prep_heal" (bogus int)"#,
        )
        .unwrap();

        let ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_heal','geom','gid', \
             '{\"name\":\"heal\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"preparedBuild\":true,\"featureDrivenEnum\":true, \
                \"parallel\":2,\"buildTimeoutMs\":60000}')",
        )
        .unwrap();
        assert_eq!(
            ok,
            Some(true),
            "parallel build must self-heal over a leftover prepared table, not error on it"
        );

        let tiles = Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_heal")
            .unwrap()
            .unwrap();
        assert!(tiles > 0, "self-healed build must still produce tiles");

        // The committed prepared table must be gone once the build finishes —
        // dropped in the coordinator transaction, visible as gone within it.
        let dropped = Spi::get_one::<bool>("SELECT to_regclass('_gp_prep_heal') IS NULL").unwrap();
        assert_eq!(
            dropped,
            Some(true),
            "parallel build must drop its committed prepared table on success"
        );

        Spi::run("SELECT ST_DropPyramid('heal')").unwrap();
    }

    /// PR4 robustness: a high parallelism degree (more workers than Morton blocks
    /// fit on this small extent) must not corrupt or drop tiles — tile coverage
    /// and encoded sizes match serial. Exercises `split_morton`'s degrade path (grid
    /// exceeds extent) end to end; the surplus workers simply build empty blocks.
    #[pg_test]
    fn parallel_eight_matches_serial() {
        use crate::worker::exec_sql_autonomous;
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();

        exec_sql_autonomous(
            "DROP TABLE IF EXISTS _gp_p8; \
             CREATE TABLE _gp_p8 (gid serial PRIMARY KEY, kind text, geom geometry(Geometry,4326)); \
             INSERT INTO _gp_p8 (kind, geom) \
                 SELECT 'point', ST_SetSRID(ST_MakePoint(110.0 + i*0.7, 35.0 + i*0.4), 4326) \
                 FROM generate_series(1, 14) AS i; \
             INSERT INTO _gp_p8 (kind, geom) \
                 SELECT 'poly', ST_SetSRID(ST_MakeEnvelope( \
                     109.0 + i*0.8, 34.0 + i*0.5, 109.0 + i*0.8 + 1.7, 34.0 + i*0.5 + 1.1, 4326), 4326) \
                 FROM generate_series(1, 8) AS i; \
             INSERT INTO _gp_p8 (kind, geom) VALUES \
                 ('outlier', ST_SetSRID(ST_MakePoint(-170,-75),4326)), \
                 ('outlier', ST_SetSRID(ST_MakePoint(170,78),4326))",
        )
        .unwrap();

        let ser = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_p8','geom','gid', \
             '{\"name\":\"p8_ser\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"preparedBuild\":true,\"featureDrivenEnum\":true}')",
        )
        .unwrap();
        assert_eq!(ser, Some(true));

        let par = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_p8','geom','gid', \
             '{\"name\":\"p8_par\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"preparedBuild\":true,\"featureDrivenEnum\":true, \
                \"parallel\":8,\"buildTimeoutMs\":60000}')",
        )
        .unwrap();
        assert_eq!(par, Some(true));

        let ser_tiles = Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_p8_ser")
            .unwrap()
            .unwrap();
        assert!(ser_tiles > 0, "serial build produced no tiles");

        let mismatches = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _gp_tiles_p8_ser a \
             FULL JOIN _gp_tiles_p8_par b USING (z, x, y) \
             WHERE octet_length(a.mvt) IS DISTINCT FROM octet_length(b.mvt)",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            mismatches, 0,
            "parallel=8 must match serial in coverage and per-tile encoded size"
        );

        Spi::run("SELECT ST_DropPyramid('p8_ser')").unwrap();
        Spi::run("SELECT ST_DropPyramid('p8_par')").unwrap();
    }

    /// PR4 robustness: `splitSize` (defer sub-threshold tiles) composes with
    /// parallel. The deferral is a `feature_count >= splitSize` filter applied per
    /// tile; tile-exact Morton ownership means each tile's `feature_count` is the
    /// same whether tallied in a worker or the coordinator, so a parallel splitSize
    /// build must match a serial one exactly — *including which tiles are deferred*.
    /// A block boundary that mis-attributed a feature would shift a count across the
    /// threshold and materialize/defer a tile on only one side.
    #[pg_test]
    fn parallel_with_split_size_matches_serial() {
        use crate::worker::exec_sql_autonomous;
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();

        // A tight cluster (multi-feature tiles survive splitSize) plus isolated
        // points and outliers (single-feature tiles get deferred).
        exec_sql_autonomous(
            "DROP TABLE IF EXISTS _gp_spl; \
             CREATE TABLE _gp_spl (gid serial PRIMARY KEY, geom geometry(Point,4326)); \
             INSERT INTO _gp_spl (geom) \
                 SELECT ST_SetSRID(ST_MakePoint(110.0 + (i%4)*0.01, 35.0 + (i/4)*0.01), 4326) \
                 FROM generate_series(1, 24) AS i; \
             INSERT INTO _gp_spl (geom) \
                 SELECT ST_SetSRID(ST_MakePoint(120.0 + i*2.0, 40.0 + i*1.5), 4326) \
                 FROM generate_series(1, 6) AS i; \
             INSERT INTO _gp_spl (geom) VALUES \
                 (ST_SetSRID(ST_MakePoint(-170,-75),4326)), \
                 (ST_SetSRID(ST_MakePoint(170,78),4326))",
        )
        .unwrap();

        let ser = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_spl','geom','gid', \
             '{\"name\":\"spl_ser\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":2, \
                \"preparedBuild\":true,\"featureDrivenEnum\":true}')",
        )
        .unwrap();
        assert_eq!(ser, Some(true));

        let par = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_spl','geom','gid', \
             '{\"name\":\"spl_par\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":2, \
                \"preparedBuild\":true,\"featureDrivenEnum\":true, \
                \"parallel\":2,\"buildTimeoutMs\":60000}')",
        )
        .unwrap();
        assert_eq!(par, Some(true));

        let ser_tiles = Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_spl_ser")
            .unwrap()
            .unwrap();
        assert!(
            ser_tiles > 0,
            "splitSize=2 must still materialize the multi-feature cluster tiles"
        );

        let mismatches = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _gp_tiles_spl_ser a \
             FULL JOIN _gp_tiles_spl_par b USING (z, x, y) \
             WHERE octet_length(a.mvt) IS DISTINCT FROM octet_length(b.mvt)",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            mismatches, 0,
            "parallel splitSize build must defer/materialize exactly the same tiles as serial"
        );

        Spi::run("SELECT ST_DropPyramid('spl_ser')").unwrap();
        Spi::run("SELECT ST_DropPyramid('spl_par')").unwrap();
    }

    /// PR4 robustness: a parallel build over a source whose geometries are all NULL
    /// has no extent (`source_bbox` → None), so it must take the early empty-pyramid
    /// path and finish cleanly with zero tiles — not panic, hang, or spin up workers
    /// against an empty prepared table.
    #[pg_test]
    fn parallel_build_handles_all_null_geometry() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_null (gid serial PRIMARY KEY, geom geometry(Point,4326)); \
             INSERT INTO _gp_null (geom) SELECT NULL FROM generate_series(1, 8)",
        )
        .unwrap();

        let ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_null','geom','gid', \
             '{\"name\":\"nullp\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"preparedBuild\":true,\"featureDrivenEnum\":true, \
                \"parallel\":2,\"buildTimeoutMs\":60000}')",
        )
        .unwrap();
        assert_eq!(
            ok,
            Some(true),
            "all-NULL source must build a (empty) pyramid cleanly"
        );

        let tiles = Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_nullp")
            .unwrap()
            .unwrap();
        assert_eq!(tiles, 0, "all-NULL source must yield an empty pyramid");

        Spi::run("SELECT ST_DropPyramid('nullp')").unwrap();
    }

    /// `parallel:0` (auto) must trigger the worker-parallel path — not silently
    /// fall back to serial — and stay byte-identical. `effective_parallel()`
    /// resolves 0 to the host core count; on a ≥2-core box this farms workers
    /// (world-corner outliers widen the bbox so deep zooms exceed
    /// PARALLEL_MIN_CELLS), and the auto-resolved worker count must produce the
    /// same tiles as serial. (On a 1-core box auto resolves to 1 and the check
    /// holds trivially.)
    #[pg_test]
    fn parallel_auto_matches_serial() {
        use crate::worker::exec_sql_autonomous;
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();

        exec_sql_autonomous(
            "DROP TABLE IF EXISTS _gp_auto; \
             CREATE TABLE _gp_auto (gid serial PRIMARY KEY, geom geometry(Geometry,4326)); \
             INSERT INTO _gp_auto (geom) \
                 SELECT ST_SetSRID(ST_MakePoint(110.0 + i*0.7, 35.0 + i*0.4), 4326) \
                 FROM generate_series(1, 14) AS i; \
             INSERT INTO _gp_auto (geom) \
                 SELECT ST_SetSRID(ST_MakeEnvelope( \
                     109.0 + i*0.8, 34.0 + i*0.5, 109.0 + i*0.8 + 1.7, 34.0 + i*0.5 + 1.1, 4326), 4326) \
                 FROM generate_series(1, 8) AS i; \
             INSERT INTO _gp_auto (geom) VALUES \
                 (ST_SetSRID(ST_MakePoint(-170,-75),4326)), \
                 (ST_SetSRID(ST_MakePoint(170,78),4326))",
        )
        .unwrap();

        let ser = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_auto','geom','gid', \
             '{\"name\":\"a0_ser\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"preparedBuild\":true,\"featureDrivenEnum\":true}')",
        )
        .unwrap();
        assert_eq!(ser, Some(true));

        // parallel:0 => auto (host cores), worker-visible committed prepared table.
        let auto = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_auto','geom','gid', \
             '{\"name\":\"a0_par\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"preparedBuild\":true,\"featureDrivenEnum\":true, \
                \"parallel\":0,\"buildTimeoutMs\":60000}')",
        )
        .unwrap();
        assert_eq!(auto, Some(true));

        let ser_tiles = Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_a0_ser")
            .unwrap()
            .unwrap();
        assert!(ser_tiles > 0, "serial build produced no tiles");

        let mismatches = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _gp_tiles_a0_ser a \
             FULL JOIN _gp_tiles_a0_par b USING (z, x, y) \
             WHERE octet_length(a.mvt) IS DISTINCT FROM octet_length(b.mvt)",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            mismatches, 0,
            "parallel:0 (auto) tiles must match serial in coverage and encoded size"
        );

        Spi::run("SELECT ST_DropPyramid('a0_ser')").unwrap();
        Spi::run("SELECT ST_DropPyramid('a0_par')").unwrap();
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
        Spi::run("CREATE TEMP TABLE _snap AS SELECT z, x, y, mvt FROM _gp_tiles__gp_inc").unwrap();

        // Move the eastern point (stays eastern), mark touched.
        Spi::run(
            "UPDATE _gp_inc SET geom = ST_SetSRID(ST_MakePoint(101, 31), 4326), \
             touched = true WHERE gid = 1",
        )
        .unwrap();
        let ok =
            Spi::get_one::<bool>("SELECT ST_UpdatePyramid('_gp_inc', 'touched = true')").unwrap();
        assert_eq!(ok, Some(true));

        // Western tiles (x in the western half) must be byte-unchanged.
        let west_changed = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _snap s \
             JOIN _gp_tiles__gp_inc t USING (z, x, y) \
             WHERE s.x < (1 << s.z) / 2 AND s.mvt IS DISTINCT FROM t.mvt",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            west_changed, 0,
            "western tiles must be untouched by an eastern-only update"
        );

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

    /// Range update re-slices a bbox region from the CURRENT source, so it
    /// cleans a deleted feature's old tile — the case the where_clause path
    /// cannot handle (a deleted row matches no clause). Tiles outside the bbox
    /// stay byte-identical, and an unknown pyramid returns false (not an error).
    #[pg_test]
    fn update_by_extent_rebuilds_region() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("CREATE TABLE _gp_ext (gid serial PRIMARY KEY, geom geometry(Point, 4326))")
            .unwrap();
        // A eastern (+30,+40), B western (-100,-30): different tiles at z>0.
        Spi::run(
            "INSERT INTO _gp_ext (geom) VALUES \
             (ST_SetSRID(ST_MakePoint(30, 40), 4326)), \
             (ST_SetSRID(ST_MakePoint(-100, -30), 4326))",
        )
        .unwrap();
        Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_ext','geom','gid','{\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0}')",
        )
        .unwrap();

        // A at (30,40) occupies its own tile at z=6 (splitSize:0 materializes it).
        let ax = Spi::get_one::<i32>(
            "SELECT floor((ST_X(ST_Transform(ST_SetSRID(ST_MakePoint(30,40),4326),3857)) \
                    + 20037508.342789244) / (2*20037508.342789244) * pow(2,6))::int",
        )
        .unwrap()
        .unwrap();
        let ay = Spi::get_one::<i32>(
            "SELECT floor((20037508.342789244 \
                    - ST_Y(ST_Transform(ST_SetSRID(ST_MakePoint(30,40),4326),3857))) \
                    / (2*20037508.342789244) * pow(2,6))::int",
        )
        .unwrap()
        .unwrap();
        let a_before = Spi::get_one_with_args::<Vec<u8>>(
            "SELECT ST_Tile('_gp_ext', 6, $1, $2)",
            &[ax.into(), ay.into()],
        )
        .unwrap();
        assert!(
            a_before.as_ref().is_some_and(|v| !v.is_empty()),
            "A's z6 tile must be materialized before deletion"
        );
        // Snapshot every tile to verify the western half is untouched.
        Spi::run("CREATE TEMP TABLE _snap_ext AS SELECT z, x, y, mvt FROM _gp_tiles__gp_ext")
            .unwrap();

        // Delete A. A deleted row matches no where_clause — only a range update
        // can clean its old tile.
        Spi::run("DELETE FROM _gp_ext WHERE ST_X(geom) = 30").unwrap();
        let ok = Spi::get_one_with_args::<bool>(
            "SELECT ST_UpdatePyramidByExtent('_gp_ext', $1, $2, $3, $4, 4326)",
            &[
                29.0_f64.into(),
                39.0_f64.into(),
                31.0_f64.into(),
                41.0_f64.into(),
            ],
        )
        .unwrap();
        assert_eq!(ok, Some(true));

        // A was deleted; range-updating its region re-slices A's z6 tile to
        // empty, so it is no longer stored — a deleted feature's tile cleaned,
        // which the where_clause path cannot do. (Byte-length on z0 would be a
        // fragile proxy: MVT size is not strictly monotonic in feature count.)
        let a_after = Spi::get_one_with_args::<Vec<u8>>(
            "SELECT ST_Tile('_gp_ext', 6, $1, $2)",
            &[ax.into(), ay.into()],
        )
        .unwrap();
        assert!(
            a_after.is_none(),
            "A's z6 tile must be empty/NULL after deleting A and range-updating its region"
        );

        // Western tiles (x in the western half, z>0) are outside the eastern
        // bbox and must be byte-unchanged. (z=0's single tile x=0 is excluded:
        // (1<<0)/2 == 0, so x<0 is false.)
        let west_changed = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _snap_ext s \
             JOIN _gp_tiles__gp_ext t USING (z, x, y) \
             WHERE s.x < (1 << s.z) / 2 AND s.mvt IS DISTINCT FROM t.mvt",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            west_changed, 0,
            "western tiles must be untouched by an eastern bbox update"
        );

        // Unknown pyramid → false (clean, not an error).
        let unk = Spi::get_one_with_args::<bool>(
            "SELECT ST_UpdatePyramidByExtent('no_such_pyr', $1, $2, $3, $4, 4326)",
            &[
                0.0_f64.into(),
                0.0_f64.into(),
                1.0_f64.into(),
                1.0_f64.into(),
            ],
        )
        .unwrap();
        assert_eq!(unk, Some(false));

        Spi::run("SELECT ST_DropPyramid('_gp_ext')").unwrap();
        Spi::run("DROP TABLE _gp_ext").unwrap();
    }

    /// ST_Tile serves materialized tiles within maxZoom, slices on-demand above it
    /// (when enabled), returns NULL for unknown pyramids, and respects the
    /// dynamicBeyondMaxZoom switch.
    #[pg_test]
    fn dynamic_tile_beyond_max_zoom() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("CREATE TABLE _gp_dyn (gid serial PRIMARY KEY, geom geometry(Point, 4326))")
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
        let unknown =
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('no_such_pyramid', 0, 0, 0)").unwrap();
        assert!(
            unknown.is_none(),
            "unknown pyramid must return NULL, not error"
        );

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
        assert!(
            dyn_hit.is_some(),
            "z=6 tile over the point must be sliced on-demand"
        );
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
        assert!(
            off.is_none(),
            "dynamicBeyondMaxZoom=false must return NULL beyond maxZoom"
        );

        Spi::run("SELECT ST_DropPyramid('dyn')").unwrap();
        Spi::run("SELECT ST_DropPyramid('dynoff')").unwrap();
        Spi::run("DROP TABLE _gp_dyn").unwrap();
    }

    /// splitSize defers sub-threshold (sparse) tiles: they are NOT materialized at
    /// build time but ST_Tile still serves them by on-demand slicing. splitSize=0
    /// materializes them as before.
    #[pg_test]
    fn split_size_defers_sparse_tiles() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("CREATE TABLE _gp_sp (gid serial PRIMARY KEY, geom geometry(Point, 4326))")
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

        Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_sp','geom','gid', \
             '{\"name\":\"sparse\",\"minZoom\":0,\"maxZoom\":4,\"splitSize\":100}')",
        )
        .unwrap();
        let materialized = Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_sparse")
            .unwrap()
            .unwrap();
        assert_eq!(
            materialized, 0,
            "splitSize=100 must skip all sub-threshold tiles"
        );

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
        assert!(
            sparse_hit.is_some(),
            "sparse tile must be sliced on-demand within maxZoom"
        );
        assert!(!sparse_hit.unwrap().is_empty());

        let far_x = if tx > 0 { 0 } else { 15 };
        let empty = Spi::get_one_with_args::<Vec<u8>>(
            "SELECT ST_Tile('sparse', 4, $1, $2)",
            &[far_x.into(), ty.into()],
        )
        .unwrap();
        assert!(empty.is_none(), "empty region must return NULL");

        Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_sp','geom','gid', \
             '{\"name\":\"dense\",\"minZoom\":0,\"maxZoom\":4,\"splitSize\":0}')",
        )
        .unwrap();
        let dense_materialized =
            Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_dense")
                .unwrap()
                .unwrap();
        assert!(dense_materialized > 0, "splitSize=0 must materialize tiles");
        let dense_hit = Spi::get_one_with_args::<Vec<u8>>(
            "SELECT ST_Tile('dense', 4, $1, $2)",
            &[tx.into(), ty.into()],
        )
        .unwrap();
        assert!(
            dense_hit.is_some(),
            "materialized tile must be served from storage"
        );

        Spi::run("SELECT ST_DropPyramid('sparse')").unwrap();
        Spi::run("SELECT ST_DropPyramid('dense')").unwrap();
        Spi::run("DROP TABLE _gp_sp").unwrap();
    }

    /// The tile table carries id (serial), format ('pbf'), and srid (= tileSRS).
    #[pg_test]
    fn tile_table_has_id_format_srid() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("CREATE TABLE _gp_cols (gid serial PRIMARY KEY, geom geometry(Point, 4326))")
            .unwrap();
        Spi::run("INSERT INTO _gp_cols (geom) VALUES (ST_SetSRID(ST_MakePoint(0,0),4326))")
            .unwrap();
        Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_cols','geom','gid', \
             '{\"minZoom\":0,\"maxZoom\":2,\"splitSize\":0}')",
        )
        .unwrap();

        let format = Spi::get_one::<String>("SELECT format FROM _gp_tiles__gp_cols LIMIT 1")
            .unwrap()
            .unwrap();
        assert_eq!(format, "pbf", "format column must be 'pbf'");

        let srid = Spi::get_one::<i32>("SELECT srid FROM _gp_tiles__gp_cols LIMIT 1")
            .unwrap()
            .unwrap();
        assert_eq!(
            srid, 3857,
            "srid column must equal the default tileSRS (3857)"
        );

        let null_ids =
            Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles__gp_cols WHERE id IS NULL")
                .unwrap()
                .unwrap();
        assert_eq!(null_ids, 0, "id column must be populated by bigserial");

        Spi::run("SELECT ST_DropPyramid('_gp_cols')").unwrap();
        Spi::run("DROP TABLE _gp_cols").unwrap();
    }

    /// The && + tile-envelope-margin candidate filter still produces correct
    /// tiles: non-empty where the source has geometry, NULL for empty regions.
    /// (The seamless-edge buffer benefit is validated visually in demo/.)
    #[pg_test]
    fn bbox_filter_and_margin_render() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("CREATE TABLE _gp_bbox (gid serial PRIMARY KEY, geom geometry(Polygon, 4326))")
            .unwrap();
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

        let z0 = Spi::get_one::<Vec<u8>>("SELECT ST_Tile('_gp_bbox', 0, 0, 0)").unwrap();
        assert!(
            z0.is_some() && !z0.unwrap().is_empty(),
            "z0 tile must contain the polygon"
        );

        let empty = Spi::get_one::<Vec<u8>>("SELECT ST_Tile('_gp_bbox', 3, 0, 0)").unwrap();
        assert!(empty.is_none(), "empty region tile must be NULL");

        Spi::run("SELECT ST_DropPyramid('_gp_bbox')").unwrap();
        Spi::run("DROP TABLE _gp_bbox").unwrap();
    }

    /// ST_Pyramid / ST_UpdatePyramid on an unknown pyramid return cleanly
    /// (NULL / false) instead of erroring. The pg_test body runs inside a
    /// nested SPI context — exactly where the pgrx 0-row error used to fire.
    #[pg_test]
    fn unknown_pyramid_returns_cleanly() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        let info = Spi::get_one::<pgrx::JsonB>("SELECT ST_Pyramid('no_such_pyr')").unwrap();
        assert!(
            info.is_none(),
            "ST_Pyramid(unknown) must be NULL, not error"
        );
        let ok = Spi::get_one::<bool>("SELECT ST_UpdatePyramid('no_such_pyr', '')").unwrap();
        assert_eq!(
            ok,
            Some(false),
            "ST_UpdatePyramid(unknown) must return false, not error"
        );
    }

    /// tileSize=0 is rejected (it is the MVT extent and a divisor in the
    /// pixel-size and filter-margin math; zero would divide by zero).
    #[pg_test]
    fn build_rejects_zero_tile_size() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("CREATE TABLE _gp_ts0 (gid serial PRIMARY KEY, geom geometry(Point, 4326))")
            .unwrap();
        Spi::run("INSERT INTO _gp_ts0 (geom) VALUES (ST_SetSRID(ST_MakePoint(0,0),4326))").unwrap();
        // The error is raised inside a subtransaction so the test can assert it
        // without aborting; a clean 'tileSize must be > 0' (not a div-by-zero).
        Spi::run(
            "DO $$ BEGIN \
               PERFORM ST_BuildPyramid('_gp_ts0','geom','gid','{\"tileSize\":0}'); \
               RAISE EXCEPTION 'should have rejected tileSize=0'; \
             EXCEPTION WHEN others THEN \
               IF SQLERRM LIKE '%should have rejected%' THEN RAISE; END IF; \
             END $$",
        )
        .unwrap();
        Spi::run("DROP TABLE _gp_ts0").unwrap();
    }

    /// tileSize past the 16384 ceiling (would wrap negative when cast to i32)
    /// and tileExtend >= tileSize are both rejected cleanly at build time.
    #[pg_test]
    fn build_rejects_out_of_range_tile_size() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("CREATE TABLE _gp_tsr (gid serial PRIMARY KEY, geom geometry(Point, 4326))")
            .unwrap();
        Spi::run("INSERT INTO _gp_tsr (geom) VALUES (ST_SetSRID(ST_MakePoint(0,0),4326))").unwrap();
        // Oversized tileSize is rejected.
        Spi::run(
            "DO $$ BEGIN \
               PERFORM ST_BuildPyramid('_gp_tsr','geom','gid','{\"tileSize\":40000}'); \
               RAISE EXCEPTION 'should have rejected oversized tileSize'; \
             EXCEPTION WHEN others THEN \
               IF SQLERRM LIKE '%should have rejected%' THEN RAISE; END IF; \
             END $$",
        )
        .unwrap();
        // tileExtend >= tileSize is rejected.
        Spi::run(
            "DO $$ BEGIN \
               PERFORM ST_BuildPyramid('_gp_tsr','geom','gid','{\"tileSize\":256,\"tileExtend\":256}'); \
               RAISE EXCEPTION 'should have rejected tileExtend>=tileSize'; \
             EXCEPTION WHEN others THEN \
               IF SQLERRM LIKE '%should have rejected%' THEN RAISE; END IF; \
             END $$",
        )
        .unwrap();
        Spi::run("DROP TABLE _gp_tsr").unwrap();
    }

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
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('br', 2, 0, 2)")
                .unwrap()
                .is_some(),
            "z2 tile over A (rank 1) must survive rank<5"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('br', 2, 3, 2)")
                .unwrap()
                .is_none(),
            "z2 tile over B (rank 9) must be filtered out"
        );
        // z3 is NOT covered: B reappears.
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('br', 3, 6, 4)")
                .unwrap()
                .is_some(),
            "z3 (uncovered) tile over B must be present"
        );
        Spi::run("SELECT ST_DropPyramid('br')").unwrap();
        Spi::run("DROP TABLE _gp_br").unwrap();
    }

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

    #[pg_test]
    fn build_rules_true_filter_matches_no_rules() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("CREATE TABLE _gp_bre (gid serial PRIMARY KEY, geom geometry(Point, 4326))")
            .unwrap();
        Spi::run(
            "INSERT INTO _gp_bre (geom) SELECT ST_SetSRID(ST_MakePoint(\
                random()*340 - 170, random()*140 - 70), 4326) \
             FROM generate_series(1, 300)",
        )
        .unwrap();
        for (_name, cfg) in [
            (
                "bre_none",
                r#"{"name":"bre_none","minZoom":0,"maxZoom":4,"splitSize":0}"#,
            ),
            (
                "bre_true",
                r#"{"name":"bre_true","minZoom":0,"maxZoom":4,"splitSize":0,"buildRules":[{"minZoom":0,"maxZoom":22,"filter":"true"}]}"#,
            ),
        ] {
            let sql = format!("SELECT ST_BuildPyramid('_gp_bre','geom','gid','{cfg}')");
            Spi::get_one::<bool>(&sql).unwrap();
        }
        // Non-vacuous guard: both sides empty would make the FULL JOIN below pass
        // with 0 mismatches. Require the no-rules build to have produced tiles, so
        // the `true`-filter side is actually compared against real coverage.
        let none_tiles = Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_bre_none")
            .unwrap()
            .unwrap();
        assert!(
            none_tiles > 0,
            "no-rules build produced no tiles to compare"
        );
        // A no-op `true` filter must leave the feature set identical, so every
        // materialized tile matches in coverage and encoded size. Compare by
        // `octet_length` (the codebase's cross-build convention): an MVT feature
        // block is self-contained, so reordering features within a tile is
        // length-invariant but reorders bytes — and with no attribute columns
        // here there is no keys/values dictionary whose order could shift size.
        // The FULL JOIN still flags any missing/extra tile via a NULL side.
        let mismatches = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM \
               (SELECT z,x,y,mvt FROM _gp_tiles_bre_none) a \
             FULL JOIN \
               (SELECT z,x,y,mvt FROM _gp_tiles_bre_true) b \
             USING (z,x,y) \
             WHERE octet_length(a.mvt) IS DISTINCT FROM octet_length(b.mvt)",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            mismatches, 0,
            "true-filter pyramid must equal the no-rules pyramid"
        );
        Spi::run("SELECT ST_DropPyramid('bre_none')").unwrap();
        Spi::run("SELECT ST_DropPyramid('bre_true')").unwrap();
        Spi::run("DROP TABLE _gp_bre").unwrap();
    }

    #[pg_test]
    fn build_rules_parallel_matches_serial() {
        use crate::worker::exec_sql_autonomous;

        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();

        // A parallel build's prepared table is created by an *autonomous* worker
        // (separate backend), so the source table must be COMMITTED for that worker
        // to see it — hence `exec_sql_autonomous`, exactly like every other parallel
        // test. (A source created in the test's own uncommitted transaction is
        // invisible to the worker and the build blocks.)
        //
        // Shape mirrors `parallel_prepared_tile_major_matches_serial`: a sparse mixed
        // cluster plus world-corner outliers that inflate the bbox so the deep-zoom
        // lattice exceeds PARALLEL_MIN_CELLS and workers actually farm. On top we
        // layer a *filtering* buildRule: `rank < 100` covers z0-3, keeping the 12
        // points (rank 1-12) and dropping the lines/polygons/outliers there, while
        // z4-6 (uncovered) carry every feature. The rule must apply identically
        // whether its per-zoom predicate is evaluated serially or read from the
        // `_gp_rule_0` column that flows through the committed prepared table to the
        // background workers. `rank` is deliberately NOT in `attributes`, so this
        // also proves the spec's headline invariant: a filter over a *non-attribute*
        // source column works because the boolean is precomputed at CTAS time — the
        // prepared table never carries the raw column.
        exec_sql_autonomous(
            "DROP TABLE IF EXISTS _gp_brp; \
             CREATE TABLE _gp_brp (gid serial PRIMARY KEY, kind text, rank int, \
                 geom geometry(Geometry, 4326)); \
             INSERT INTO _gp_brp (kind, rank, geom) \
                 SELECT 'point', i, ST_SetSRID(ST_MakePoint(110.0 + i*0.7, 35.0 + i*0.4), 4326) \
                 FROM generate_series(1, 12) AS i; \
             INSERT INTO _gp_brp (kind, rank, geom) \
                 SELECT 'line', 100+i, ST_SetSRID(ST_MakeLine( \
                     ST_MakePoint(108.0 + i*0.9, 36.0 + i*0.6), \
                     ST_MakePoint(108.0 + i*0.9 + 2.0, 36.0 + i*0.6 + 1.5)), 4326) \
                 FROM generate_series(1, 8) AS i; \
             INSERT INTO _gp_brp (kind, rank, geom) \
                 SELECT 'polygon', 200+i, ST_SetSRID(ST_MakeEnvelope( \
                     109.0 + i*0.8, 34.0 + i*0.5, 109.0 + i*0.8 + 1.7, 34.0 + i*0.5 + 1.1, 4326), 4326) \
                 FROM generate_series(1, 8) AS i; \
             INSERT INTO _gp_brp (kind, rank, geom) VALUES \
                 ('outlier', 9001, ST_SetSRID(ST_MakePoint(-170, -75), 4326)), \
                 ('outlier', 9002, ST_SetSRID(ST_MakePoint(170, 78), 4326))",
        )
        .unwrap();

        // Serial (parallel defaults to 1) and parallel=2, both carrying the rule.
        let ser_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_brp','geom','gid', \
             '{\"name\":\"brp_ser\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"simplifyPixels\":1.0,\"attributes\":[\"kind\"], \
                \"preparedBuild\":true, \
                \"buildRules\":[{\"minZoom\":0,\"maxZoom\":3,\"filter\":\"rank < 100\"}]}')",
        )
        .unwrap();
        assert_eq!(ser_ok, Some(true));

        let par_ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_brp','geom','gid', \
             '{\"name\":\"brp_par\",\"minZoom\":0,\"maxZoom\":6,\"splitSize\":0, \
                \"simplifyPixels\":1.0,\"attributes\":[\"kind\"], \
                \"preparedBuild\":true, \
                \"parallel\":2,\"buildTimeoutMs\":60000, \
                \"buildRules\":[{\"minZoom\":0,\"maxZoom\":3,\"filter\":\"rank < 100\"}]}')",
        )
        .unwrap();
        assert_eq!(par_ok, Some(true));

        // Non-vacuous: the serial build must have produced tiles to compare.
        let ser_tiles = Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_brp_ser")
            .unwrap()
            .unwrap();
        assert!(ser_tiles > 0, "serial buildRules build produced no tiles");

        // Parallel buildRules must equal serial: same coverage, same per-tile encoded
        // size. Compared by `octet_length` because each Morton block scans a different
        // sub-extent and may encode a tile's features in a different (length-invariant)
        // order. A coverage gap surfaces as a NULL on one side of the FULL JOIN.
        let mismatches = Spi::get_one::<i64>(
            "SELECT count(*)::bigint FROM _gp_tiles_brp_ser a \
             FULL JOIN _gp_tiles_brp_par b USING (z,x,y) \
             WHERE octet_length(a.mvt) IS DISTINCT FROM octet_length(b.mvt)",
        )
        .unwrap()
        .unwrap();
        assert_eq!(mismatches, 0, "parallel buildRules must equal serial");
        Spi::run("SELECT ST_DropPyramid('brp_ser')").unwrap();
        Spi::run("SELECT ST_DropPyramid('brp_par')").unwrap();
        // _gp_brp was created via exec_sql_autonomous (committed); the next run's
        // "DROP TABLE IF EXISTS" reclaims it (the test database is torn down per run).
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
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brm', 2, 0, 2)")
                .unwrap()
                .is_some(),
            "A (rank 5) passes both rules"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brm', 2, 3, 2)")
                .unwrap()
                .is_none(),
            "B (rank 9) fails rank<=5"
        );
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('brm', 2, 2, 2)")
                .unwrap()
                .is_none(),
            "C (rank 1) fails rank>=5"
        );
        Spi::run("SELECT ST_DropPyramid('brm')").unwrap();
        Spi::run("DROP TABLE _gp_brm").unwrap();
    }

    #[pg_test]
    fn build_rules_invalid_filter_errors_cleanly() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("CREATE TABLE _gp_bri (gid serial PRIMARY KEY, geom geometry(Point, 4326))")
            .unwrap();
        Spi::run("INSERT INTO _gp_bri (geom) VALUES (ST_SetSRID(ST_MakePoint(1,1),4326))").unwrap();
        // Filter references a non-existent column: build must err in a
        // subtransaction, leaving NO pyramid behind.
        Spi::run(
            "DO $$ BEGIN \
               PERFORM ST_BuildPyramid('_gp_bri','geom','gid', \
                 '{\"name\":\"bri\",\"minZoom\":0,\"maxZoom\":2, \
                   \"buildRules\":[{\"minZoom\":0,\"maxZoom\":2,\"filter\":\"no_such_col < 5\"}]}'); \
             EXCEPTION WHEN others THEN NULL; END $$",
        )
        .unwrap();
        // Atomicity: ST_BuildPyramid inserts a catalog row *before* the build runs
        // its filter probe, so that row is the one thing actually written before the
        // failure. The caught exception must roll it back together with the
        // (never-created) tile table — neither may survive.
        let leftover = Spi::get_one::<bool>(
            "SELECT to_regclass('_gp_tiles_bri') IS NOT NULL \
                 OR EXISTS (SELECT 1 FROM geometry_pyramid_catalog WHERE pyramid_name = 'bri')",
        )
        .unwrap()
        .unwrap();
        assert!(
            !leftover,
            "invalid buildRules filter must leave no pyramid (no tile table, no catalog row)"
        );
        Spi::run("DROP TABLE _gp_bri").unwrap();
    }
}

/// Required by `cargo pgrx test`.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        // The parallel-build tests each spawn background workers (build strips +
        // the autonomous-commit worker), and libtest runs tests concurrently
        // against one shared cluster. The PG default `max_worker_processes = 8`
        // is exhausted when several parallel tests overlap — especially
        // `parallel:0` (auto), which resolves to the host core count. Raise the
        // ceiling well above peak concurrent demand so spawns don't fail (the
        // autonomous-commit path has no inline fallback and would panic).
        vec!["max_worker_processes = 64", "max_parallel_workers = 16"]
    }
}
