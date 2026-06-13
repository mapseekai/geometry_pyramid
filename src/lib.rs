//! geometry_pyramid - vector tile pyramid for PostgreSQL geometry tables.
//!
//! Inspired by Alibaba GanosBase `ganos_geometry_pyramid`. Provides SQL
//! functions to build, inspect, query, update, and drop a multi-resolution
//! pyramid index over a geometry column.

use pgrx::prelude::*;

::pgrx::pg_module_magic!();

mod build;
mod config;
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
              '{\"minZoom\":0,\"maxZoom\":4,\"dropRate\":0.4}')",
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
}

/// Required by `cargo pgrx test`.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
