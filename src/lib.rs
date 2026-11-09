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

    fn bytea_contains(bytes: &[u8], needle: &str) -> bool {
        let needle = needle.as_bytes();
        bytes.windows(needle.len()).any(|window| window == needle)
    }

    fn tile_bytes(sql: &str) -> Vec<u8> {
        Spi::get_one::<Vec<u8>>(sql)
            .unwrap()
            .expect("tile should exist")
    }

    // Decode just the MVT fields needed for semantic equality. Dictionary and
    // feature ordering are irrelevant; duplicate features remain visible.
    type MvtFeature = (u64, u64, Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>);

    fn mvt_varint(bytes: &mut &[u8]) -> u64 {
        let mut value = 0;
        for shift in (0..70).step_by(7) {
            let byte = bytes[0];
            *bytes = &bytes[1..];
            value |= u64::from(byte & 127) << shift;
            if byte & 128 == 0 {
                return value;
            }
        }
        panic!("invalid MVT varint")
    }

    fn mvt_fields(mut bytes: &[u8]) -> Vec<(u64, Vec<u8>)> {
        let mut fields = Vec::new();
        while !bytes.is_empty() {
            let tag = mvt_varint(&mut bytes);
            let value = match tag & 7 {
                0 => mvt_varint(&mut bytes).to_le_bytes().to_vec(),
                wire => {
                    let len = match wire {
                        1 => 8,
                        2 => mvt_varint(&mut bytes) as usize,
                        5 => 4,
                        _ => panic!("unsupported MVT wire type {wire}"),
                    };
                    let value = bytes[..len].to_vec();
                    bytes = &bytes[len..];
                    value
                }
            };
            fields.push((tag >> 3, value));
        }
        fields
    }

    fn decoded_mvt_features(bytes: &[u8]) -> Vec<MvtFeature> {
        let mut features = Vec::new();
        for (_, layer) in mvt_fields(bytes).into_iter().filter(|(tag, _)| *tag == 3) {
            let fields = mvt_fields(&layer);
            let keys: Vec<_> = fields.iter().filter(|(tag, _)| *tag == 3).collect();
            let values: Vec<_> = fields.iter().filter(|(tag, _)| *tag == 4).collect();
            for (_, feature) in fields.iter().filter(|(tag, _)| *tag == 2) {
                let (mut id, mut kind, mut geometry, mut attributes) =
                    (0, 0, Vec::new(), Vec::new());
                for (tag, value) in mvt_fields(feature) {
                    match tag {
                        1 => id = u64::from_le_bytes(value.try_into().unwrap()),
                        2 => {
                            let mut packed = value.as_slice();
                            while !packed.is_empty() {
                                let key = mvt_varint(&mut packed) as usize;
                                let val = mvt_varint(&mut packed) as usize;
                                attributes.push((keys[key].1.clone(), values[val].1.clone()));
                            }
                        }
                        3 => kind = u64::from_le_bytes(value.try_into().unwrap()),
                        4 => geometry = value,
                        _ => {}
                    }
                }
                attributes.sort();
                features.push((id, kind, geometry, attributes));
            }
        }
        features.sort();
        features
    }

    fn decoded_stored_tiles(
        name: &str,
    ) -> std::collections::BTreeMap<(i32, i32, i32), Vec<MvtFeature>> {
        Spi::connect(|client| {
            client
                .select(
                    &format!("SELECT z, x, y, mvt FROM _gp_tiles_{name}"),
                    None,
                    &[],
                )
                .unwrap()
                .map(|row| {
                    let z = i32::from(row["z"].value::<i16>().unwrap().unwrap());
                    let x = row["x"].value::<i32>().unwrap().unwrap();
                    let y = row["y"].value::<i32>().unwrap().unwrap();
                    let bytes = row["mvt"].value::<Vec<u8>>().unwrap().unwrap();
                    ((z, x, y), decoded_mvt_features(&bytes))
                })
                .collect()
        })
    }

    #[pg_test]
    fn feature_major_points_buffer_semantics_match_prepared() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        // Offsets from the z=2 central corner are in MVT pixels. Include exact
        // edges, inside/outside the default and large buffers, and world limits.
        Spi::run(
            "CREATE TABLE _gp_buffer_semantics (gid serial PRIMARY KEY, label text,
                 geom geometry(Point,3857));
             INSERT INTO _gp_buffer_semantics(label, geom)
             SELECT 'offset-' || dx || '-' || dy,
                    ST_SetSRID(ST_Point(dx * 2445.98490512564, dy * 2445.98490512564),3857)
             FROM (VALUES (-257),(-255),(-9),(-7),(-1),(0),(1),(7),(9),(255),(257)) a(dx)
             CROSS JOIN (VALUES (-7),(0),(7)) b(dy);
             INSERT INTO _gp_buffer_semantics(label, geom) VALUES
                 ('world-nw', ST_SetSRID(ST_Point(-20037508.342789244,20037508.342789244),3857)),
                 ('world-se', ST_SetSRID(ST_Point(20037508.342789244,-20037508.342789244),3857)),
                 ('empty', ST_GeomFromText('POINT EMPTY',3857)),
                 ('null', NULL)",
        )
        .unwrap();
        for buffer in [0, 8, 256] {
            let mut snapshots = Vec::new();
            for feature_major in [false, true] {
                let mut config = serde_json::json!({
                    "name":"buffer_semantics", "minZoom":2, "maxZoom":2,
                    "splitSize":0, "preparedBuild":true, "featureMajorPoints":feature_major,
                    "featureMajorMinZoom":0, "attributes":["label"], "dropRate":1
                });
                // Exercise the default rather than merely explicitly setting 8.
                if buffer != 8 {
                    config["tileExtend"] = buffer.into();
                }
                assert_eq!(
                    Spi::get_one::<bool>(&format!(
                        "SELECT ST_BuildPyramid('_gp_buffer_semantics','geom','gid','{config}')"
                    ))
                    .unwrap(),
                    Some(true)
                );
                snapshots.push(decoded_stored_tiles("buffer_semantics"));
                Spi::run("SELECT ST_DropPyramid('buffer_semantics')").unwrap();
            }
            assert!(!snapshots[0].is_empty());
            assert_eq!(
                snapshots[0], snapshots[1],
                "buffer {buffer}: feature IDs, types, geometry or attributes differ"
            );
        }
        Spi::run("DROP TABLE _gp_buffer_semantics").unwrap();
    }

    #[pg_test]
    fn prepared_geometry_functions_run_once_per_feature() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis; SET LOCAL track_functions = 'all'")
            .unwrap();
        Spi::run(
            "CREATE TABLE _gp_eval_once (gid serial PRIMARY KEY, geom geometry(Polygon,3857));
             INSERT INTO _gp_eval_once(geom)
             SELECT ST_Buffer(ST_SetSRID(ST_Point(i * 10000, 1000000),3857), 50000, 32)
             FROM generate_series(1,100) i",
        )
        .unwrap();
        let calls = || {
            Spi::get_one::<i64>(
                "SELECT coalesce(sum(calls),0)::bigint FROM pg_stat_xact_user_functions
                 WHERE funcname IN ('st_simplifypreservetopology','st_asmvtgeom')",
            )
            .unwrap()
            .unwrap()
        };
        let before = calls();
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT ST_BuildPyramid('_gp_eval_once','geom','gid',
             '{\"name\":\"eval_once\",\"minZoom\":0,\"maxZoom\":0,
               \"preparedBuild\":true,\"simplifyPixels\":1,\"splitSize\":0}')"
            )
            .unwrap(),
            Some(true)
        );
        let after = calls();
        let tiles = decoded_stored_tiles("eval_once");
        assert_eq!(tiles.values().map(Vec::len).sum::<usize>(), 100);
        assert_eq!(
            after - before,
            200,
            "100 surviving polygons must each simplify and clip exactly once"
        );
        // Shrink the same polygons into the center of z8 tile (128,127).
        // All 100 features expand to exactly nine padded candidates each.
        Spi::run(
            "CREATE TEMP TABLE _gp_eval_driven AS SELECT gid::bigint AS fid,
             'POLYGON'::text AS geom_type,
             ST_Translate(ST_Scale(geom,0.001,0.001),78000,77000) AS geom_tile
             FROM _gp_eval_once",
        )
        .unwrap();
        let function_calls = |name: &str| {
            Spi::get_one_with_args::<i64>(
            "SELECT coalesce(sum(calls),0)::bigint FROM pg_stat_xact_user_functions WHERE funcname=$1",
            &[name.into()]
        ).unwrap().unwrap()
        };
        let simplify_before = function_calls("st_simplifypreservetopology");
        let clip_before = function_calls("st_asmvtgeom");
        let cfg: crate::config::PyramidConfig = serde_json::from_value(serde_json::json!({
            "minZoom":8,"maxZoom":8,"splitSize":0,"simplifyPixels":1,"sourceSRS":3857
        }))
        .unwrap();
        let source = crate::source::PyramidSource {
            name: "eval_once",
            source_table: "_gp_eval_once",
            geom_column: "geom",
            fid_column: "gid",
            cfg: &cfg,
        };
        let rect = crate::tile_rect::TileRect {
            z: 8,
            xmin: 127,
            xmax: 129,
            ymin: 126,
            ymax: 128,
        };
        let prepared = crate::prepared::PreparedBuildSource::new("_gp_eval_driven".into(), false);
        let knobs = crate::build::ZoomKnobs::from_config(&cfg, 8);
        assert_eq!(
            crate::build::build_zoom_level_dispatch(
                source,
                "_gp_tiles_eval_once",
                &rect,
                &knobs,
                Some(&prepared),
                false,
                true
            )
            .unwrap(),
            1
        );
        assert_eq!(
            function_calls("st_simplifypreservetopology") - simplify_before,
            100,
            "feature-driven simplification must happen once before candidate expansion"
        );
        assert_eq!(
            function_calls("st_asmvtgeom") - clip_before,
            100,
            "only the candidate intersecting each source envelope should be clipped"
        );
        assert_eq!(decoded_stored_tiles("eval_once")[&(8, 128, 127)].len(), 100);
        Spi::run("DROP TABLE _gp_eval_driven").unwrap();
        Spi::run("SELECT ST_DropPyramid('eval_once'); DROP TABLE _gp_eval_once").unwrap();
    }

    #[pg_test]
    fn zoom_insert_count_excludes_existing_tiles() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_insert_count_src(gid int, geom geometry(Point,4326));
             INSERT INTO _gp_insert_count_src VALUES (1,ST_SetSRID(ST_Point(30,-30),4326));
             CREATE TABLE _gp_insert_count_tiles(z smallint,x int,y int,format text,srid int,mvt bytea);
             INSERT INTO _gp_insert_count_tiles VALUES (1,0,0,'pbf',3857,'\\x'::bytea)"
        ).unwrap();
        let cfg: crate::config::PyramidConfig = serde_json::from_value(serde_json::json!({
            "minZoom":1,"maxZoom":1,"splitSize":0
        }))
        .unwrap();
        let source = crate::source::PyramidSource {
            name: "insert_count",
            source_table: "_gp_insert_count_src",
            geom_column: "geom",
            fid_column: "gid",
            cfg: &cfg,
        };
        let rect = crate::tile_rect::TileRect {
            z: 1,
            xmin: 1,
            xmax: 1,
            ymin: 1,
            ymax: 1,
        };
        let knobs = crate::build::ZoomKnobs::from_config(&cfg, 1);
        assert_eq!(
            crate::build::build_zoom_level(source, "_gp_insert_count_tiles", &rect, &knobs)
                .unwrap(),
            1
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM _gp_insert_count_tiles").unwrap(),
            Some(2)
        );
        let empty = crate::tile_rect::TileRect {
            z: 1,
            xmin: 0,
            xmax: 0,
            ymin: 1,
            ymax: 1,
        };
        assert_eq!(
            crate::build::build_zoom_level(source, "_gp_insert_count_tiles", &empty, &knobs)
                .unwrap(),
            0
        );
        Spi::run("DROP TABLE _gp_insert_count_src, _gp_insert_count_tiles").unwrap();
    }

    #[pg_test]
    fn mixed_geometry_build_paths_preserve_lod_semantics() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run(
            "CREATE TABLE _gp_mixed_semantics(gid int PRIMARY KEY, kind text, detail text,
                 geom geometry(Geometry,3857));
             INSERT INTO _gp_mixed_semantics VALUES
               (1,'point','point-detail',ST_SetSRID(ST_Point(100000,100000),3857)),
               (2,'line','line-detail',ST_GeomFromText('LINESTRING(80000 80000,100000 100010,120000 120000)',3857)),
               (3,'polygon','polygon-detail',ST_Buffer(ST_SetSRID(ST_Point(100000,100000),3857),20000,32)),
               (4,'tiny-line','filtered',ST_GeomFromText('LINESTRING(100000 100000,100001 100001)',3857)),
               (5,'tiny-polygon','filtered',ST_MakeEnvelope(100000,100000,100001,100001,3857)),
               (6,'excluded','filtered',ST_SetSRID(ST_Point(100000,100000),3857)),
               (7,'sparse','deferred',ST_SetSRID(ST_Point(-19000000,-19000000),3857)),
               (8,'sparse','deferred',ST_SetSRID(ST_Point(19000000,19000000),3857))"
        ).unwrap();
        let mut snapshots = Vec::new();
        for (prepared, driven) in [(false, false), (true, false), (true, true)] {
            let config = serde_json::json!({
                "name":"mixed_semantics","sourceSRS":3857,"minZoom":5,"maxZoom":6,"splitSize":2,
                "simplifyPixels":1,"minFeatureSizePixels":2,"dropRate":1,
                "preparedBuild":prepared,"featureDrivenEnum":driven,
                "attributes":["kind","detail"],
                "buildRules":[{"minZoom":5,"maxZoom":6,"filter":"gid <> 6"},
                              {"minZoom":5,"maxZoom":5,"attrFields":["kind"]}]
            });
            assert_eq!(
                Spi::get_one::<bool>(&format!(
                    "SELECT ST_BuildPyramid('_gp_mixed_semantics','geom','gid','{config}')"
                ))
                .unwrap(),
                Some(true)
            );
            let tiles = decoded_stored_tiles("mixed_semantics");
            assert_eq!(
                tiles.len(),
                2,
                "only the dense tile should survive at each zoom"
            );
            for ((z, _, _), features) in &tiles {
                assert_eq!(
                    features.iter().map(|f| f.0).collect::<Vec<_>>(),
                    vec![1, 2, 3]
                );
                for feature in features {
                    let names: Vec<_> = feature.3.iter().map(|(key, _)| key.as_slice()).collect();
                    assert_eq!(
                        names,
                        if *z == 5 {
                            vec![b"kind".as_slice()]
                        } else {
                            vec![b"detail".as_slice(), b"kind".as_slice()]
                        }
                    );
                }
            }
            snapshots.push(tiles);
            Spi::run("SELECT ST_DropPyramid('mixed_semantics')").unwrap();
        }
        assert_eq!(
            snapshots[0], snapshots[1],
            "source and prepared MVT semantics differ"
        );
        assert_eq!(
            snapshots[0], snapshots[2],
            "source and feature-driven MVT semantics differ"
        );
        Spi::run("DROP TABLE _gp_mixed_semantics").unwrap();
    }

    #[pg_test]
    fn feature_driven_candidate_counts_include_padding_and_ignore_empty() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        // The valid point lies at tile coordinates (1.5, 1.5) at z=2.
        Spi::run(
            "CREATE TABLE _gp_candidate_stats(geom_tile geometry(Point,3857));
             INSERT INTO _gp_candidate_stats VALUES
               (ST_SetSRID(ST_Point(-5009377.085697311,5009377.085697311),3857)),
               (ST_GeomFromText('POINT EMPTY',3857)), (NULL)",
        )
        .unwrap();
        let prepared =
            crate::prepared::PreparedBuildSource::new("_gp_candidate_stats".into(), false);
        let rect = crate::tile_rect::TileRect {
            z: 2,
            xmin: 0,
            xmax: 3,
            ymin: 0,
            ymax: 3,
        };
        let mut cfg = crate::config::PyramidConfig {
            tile_size: 4096,
            ..Default::default()
        };
        for (buffer, pairs) in [(0, 9), (4096, 16)] {
            // Direct statistics invocation deliberately includes a full-tile
            // buffer to exercise clamping independently of config validation.
            cfg.tile_extend = buffer;
            assert_eq!(
                crate::build::feature_driven_prep_stats(&prepared, &rect, &cfg).unwrap(),
                (1, pairs)
            );
        }
        let strip = crate::tile_rect::TileRect {
            z: 2,
            xmin: 1,
            xmax: 1,
            ymin: 1,
            ymax: 2,
        };
        assert_eq!(
            crate::build::feature_driven_prep_stats(&prepared, &strip, &cfg).unwrap(),
            (1, 2)
        );
        Spi::run("DELETE FROM _gp_candidate_stats WHERE geom_tile IS NOT NULL AND NOT ST_IsEmpty(geom_tile)").unwrap();
        assert_eq!(
            crate::build::feature_driven_prep_stats(&prepared, &rect, &cfg).unwrap(),
            (0, 0),
            "empty and null geometry must not enumerate the tile lattice"
        );
        Spi::run("DROP TABLE _gp_candidate_stats").unwrap();
    }

    #[pg_test]
    fn small_parallel_build_uses_local_preparation() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        // This uncommitted relation is deliberately invisible to autonomous
        // workers. Every zoom fits in a single tile, so the build stays local.
        Spi::run(
            "CREATE TABLE _gp_small_parallel_src(gid int PRIMARY KEY, geom geometry(Point,3857));
             INSERT INTO _gp_small_parallel_src
             SELECT i, ST_SetSRID(ST_Point(100000 + i * 100,100000 + i * 100),3857)
             FROM generate_series(1,10) i",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT ST_BuildPyramid('_gp_small_parallel_src','geom','gid',
             '{\"name\":\"small_parallel\",\"preparedBuild\":true,\"parallel\":4,
               \"minZoom\":0,\"maxZoom\":6,\"sourceSRS\":3857,
               \"splitSize\":0,\"dropRate\":1,\"buildTimeoutMs\":2000}')"
            )
            .unwrap(),
            Some(true)
        );
        let tiles = decoded_stored_tiles("small_parallel");
        assert_eq!(
            tiles.len(),
            7,
            "one populated tile at every zoom from 0 through 6"
        );
        for features in tiles.values() {
            assert_eq!(
                features.iter().map(|feature| feature.0).collect::<Vec<_>>(),
                (1..=10).collect::<Vec<_>>()
            );
        }
        assert_eq!(
            Spi::get_one::<bool>("SELECT to_regclass('_gp_prep_small_parallel') IS NULL").unwrap(),
            Some(true)
        );
        Spi::run("SELECT ST_DropPyramid('small_parallel'); DROP TABLE _gp_small_parallel_src")
            .unwrap();
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

        record_prepared_indexes();
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

        let z6_tiles =
            Spi::get_one::<i64>("SELECT count(*)::bigint FROM _gp_tiles_brfd WHERE z = 6")
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

        record_prepared_indexes();
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

        record_prepared_indexes();
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

    // Observe CREATE INDEX while the prepared table exists, including when
    // successful builds free their temporary tables before returning.
    fn record_prepared_indexes() {
        Spi::run("CREATE TEMP TABLE gp_created_indexes(id oid);
            CREATE FUNCTION pg_temp.record_index() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN
            INSERT INTO pg_temp.gp_created_indexes SELECT c.oid FROM pg_event_trigger_ddl_commands() e
            JOIN pg_class c ON c.oid=e.objid WHERE c.relnamespace=pg_my_temp_schema()
                AND c.relkind='i' AND c.relname LIKE '_gp_prep_%_geom_tile_idx'; END $$").unwrap();
        Spi::run(&format!(
            "CREATE EVENT TRIGGER gp_index_{} ON ddl_command_end WHEN TAG IN ('CREATE INDEX')
                   EXECUTE FUNCTION pg_temp.record_index()",
            unsafe { pgrx::pg_sys::MyProcPid }
        ))
        .unwrap();
    }

    fn prepared_geom_index_count() -> i64 {
        Spi::get_one("SELECT count(*) FROM pg_temp.gp_created_indexes")
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

        let shadow_left =
            Spi::get_one::<bool>("SELECT to_regclass('\"_gp_shadow__gp_atomic\"') IS NOT NULL")
                .unwrap()
                .unwrap();
        assert!(!shadow_left);

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
    fn local_fallback_preserves_an_unmanaged_prepared_name() {
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

        // An unmarked relation is ordinary user data, even with our old scratch name.
        exec_sql_autonomous(
            r#"DROP TABLE IF EXISTS "_gp_prep_heal"; CREATE TABLE "_gp_prep_heal" (bogus int);
               INSERT INTO "_gp_prep_heal" VALUES (73)"#,
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

        assert_eq!(
            Spi::get_one::<i32>("SELECT bogus FROM _gp_prep_heal").unwrap(),
            Some(73)
        );
        Spi::run("DROP TABLE _gp_prep_heal").unwrap();

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

    #[pg_test]
    fn ganos_max_level_caps_materialization_and_allows_dynamic_reads() {
        Spi::run(
            r#"CREATE TABLE gp_level_src AS
                 SELECT 1 AS id, ST_SetSRID(ST_Point(1000,1000),3857) AS geom;
                 SELECT ST_BuildPyramid('gp_level_src','geom','id',
                   '{"name":"gp_level","sourceSRS":3857,"maxLevel":1}')"#,
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i16>("SELECT max(z) FROM _gp_tiles_gp_level").unwrap(),
            Some(1)
        );
        assert_eq!(
            Spi::get_one::<i32>("SELECT (ST_Pyramid('gp_level')->'config'->>'maxZoom')::int")
                .unwrap(),
            Some(1)
        );
        assert!(!tile_bytes("SELECT ST_Tile('gp_level',2,2,1)").is_empty());
    }

    #[pg_test]
    fn split_pruning_preserves_buffered_and_deferred_tiles() {
        Spi::run(
            "CREATE TABLE gp_split_src (id int, label text, geom geometry(Point,3857));
             INSERT INTO gp_split_src
             SELECT i, 'edge-' || i, ST_SetSRID(ST_Point(ST_XMax(g)-100, ST_Y(ST_Centroid(g))),3857)
             FROM (SELECT ST_TileEnvelope(8,3,4) g) t CROSS JOIN generate_series(1,2) i;
             INSERT INTO gp_split_src VALUES
                (3,'outlier',ST_Centroid(ST_TileEnvelope(8,253,249))),
                (4,'filtered tile',ST_Centroid(ST_TileEnvelope(8,19,21))),
                (5,'filtered out',ST_Centroid(ST_TileEnvelope(8,19,21)));
             CREATE INDEX ON gp_split_src USING gist(geom)",
        )
        .unwrap();
        let build = |split, path: &str| {
            Spi::run(&format!(
                r#"SELECT ST_BuildPyramid('gp_split_src','geom','id',
                  '{{"name":"gp_split","sourceSRS":3857,"minZoom":8,"maxZoom":8,
                     "splitSize":{split},"attributes":["label"],
                     "buildRules":[{{"minZoom":8,"maxZoom":8,"filter":"id <> 5"}}]{path}}}')"#
            ))
            .unwrap();
        };
        build(0, "");
        let all = decoded_stored_tiles("gp_split");
        assert_eq!(all.len(), 4);
        assert_eq!(all[&(8, 3, 4)].len(), 2);
        assert_eq!(all[&(8, 4, 4)].len(), 2);
        let mut stored = all.clone();
        stored.retain(|_, features| features.len() >= 2);
        for path in [
            "",
            r#","preparedBuild":true"#,
            r#","preparedBuild":true,"featureMajorPoints":true,"featureMajorMinZoom":0"#,
            r#","preparedBuild":true,"featureDrivenEnum":true"#,
        ] {
            build(2, path);
            assert_eq!(decoded_stored_tiles("gp_split"), stored, "{path}");
            for (&(z, x, y), features) in &all {
                let bytes = tile_bytes(&format!("SELECT ST_Tile('gp_split',{z},{x},{y})"));
                assert_eq!(
                    &decoded_mvt_features(&bytes),
                    features,
                    "{path}: {z}/{x}/{y}"
                );
            }
            Spi::run("SELECT ST_UpdatePyramid('gp_split','id=1')").unwrap();
            assert_eq!(
                decoded_stored_tiles("gp_split"),
                stored,
                "{path} after refresh"
            );
        }
    }

    #[pg_test]
    fn default_sparse_build_matches_full_grid_including_buffer_tiles() {
        Spi::run(
            r#"CREATE TABLE gp_auto_sparse_src (id int, label text, geom geometry(Point,4326));
               INSERT INTO gp_auto_sparse_src
               SELECT i, 'scattered-' || i,
                      ST_Transform(ST_Centroid(ST_TileEnvelope(8,3+(i*37)%250,2+(i*53)%251)),4326)
               FROM generate_series(1,64) i;
               INSERT INTO gp_auto_sparse_src
               SELECT 65, 'near edge',
                      ST_Transform(ST_SetSRID(ST_Point(ST_XMax(g)-100,ST_Y(ST_Centroid(g))),3857),4326)
               FROM (SELECT ST_TileEnvelope(8,3,4) AS g) edge"#,
        )
        .unwrap();
        let build = || {
            Spi::run(
                r#"SELECT ST_BuildPyramid('gp_auto_sparse_src','geom','id',
                   '{"name":"gp_auto_sparse","minZoom":8,"maxZoom":8,"attributes":["label"]}')"#,
            )
            .unwrap();
        };
        build();
        let full_grid = decoded_stored_tiles("gp_auto_sparse");
        assert_eq!(full_grid.len(), 66);
        assert_eq!(full_grid[&(8, 3, 4)][0].0, 65);
        assert_eq!(full_grid[&(8, 4, 4)][0].0, 65);
        Spi::run(
            "CREATE INDEX ON gp_auto_sparse_src USING gist(geom);
             ANALYZE gp_auto_sparse_src",
        )
        .unwrap();
        build();
        assert_eq!(decoded_stored_tiles("gp_auto_sparse"), full_grid);
    }

    #[pg_test]
    fn point_grid_preserves_key_and_sparse_points_across_tile_paths() {
        Spi::run(
            r#"CREATE TABLE gp_grid_src (id int PRIMARY KEY, label text, important boolean,
                                         geom geometry);
               INSERT INTO gp_grid_src
               SELECT id, 'point-' || id, important, ST_SetSRID(ST_Point(x,-200000),3857)
               FROM (VALUES (1,200000,false),(2,201000,false),(3,202000,true),(4,203000,true),
                            (5,1000000,false),(6,1001000,false),(7,3000000,NULL)) v(id,x,important);
               INSERT INTO gp_grid_src VALUES
                 (8,'multipoint',false,ST_GeomFromText('MULTIPOINT(200000 -200000,450000 -200000)',3857)),
                 (9,'line',false,ST_GeomFromText('LINESTRING(200000 -200000,3300000 -200000)',3857)),
                 (10,'measured point',false,ST_GeomFromText('POINT M (1002000 -200000 3)',3857));
               CREATE INDEX ON gp_grid_src USING gist(geom);
               SELECT ST_BuildPyramid('gp_grid_src','geom','id',
                 '{"name":"gp_grid","sourceSRS":3857,"maxZoom":1,"attributes":["label"]}')"#,
        )
        .unwrap();
        let all = decoded_stored_tiles("gp_grid");
        assert_eq!(all[&(0, 0, 0)].len(), 10);
        let mut thinned = all.clone();
        thinned
            .get_mut(&(0, 0, 0))
            .unwrap()
            .retain(|f| ![1, 2, 6, 10].contains(&f.0));
        let build = |opts: &str| {
            Spi::run(&format!(
                r#"SELECT ST_BuildPyramid('gp_grid_src','geom','id',
                  '{{"name":"gp_grid","sourceSRS":3857,"maxZoom":1,"attributes":["label"],
                     "buildRules":[{{"minZoom":0,"maxZoom":0,
                       "pointGrid":{{"cellPixels":64,"keepField":"important"}}}}]{opts}}}')"#
            ))
            .unwrap();
        };
        build("");
        assert_eq!(decoded_stored_tiles("gp_grid"), thinned);
        build(r#","preparedBuild":true,"featureDrivenEnum":true"#);
        assert_eq!(decoded_stored_tiles("gp_grid"), thinned);

        Spi::run(
            "UPDATE gp_grid_src SET important=true WHERE id=2;
             SELECT ST_UpdatePyramid('gp_grid','id=2')",
        )
        .unwrap();
        thinned
            .get_mut(&(0, 0, 0))
            .unwrap()
            .insert(0, all[&(0, 0, 0)][1].clone());
        assert_eq!(decoded_stored_tiles("gp_grid"), thinned);

        build(r#","splitSize":100"#);
        assert!(decoded_stored_tiles("gp_grid").is_empty());
        for ((z, x, y), expected) in thinned {
            let bytes = tile_bytes(&format!("SELECT ST_Tile('gp_grid',{z},{x},{y})"));
            assert_eq!(decoded_mvt_features(&bytes), expected);
        }
    }

    #[pg_test]
    fn coarse_overlapping_lines_reuse_simplification() {
        Spi::run(
            r#"SET LOCAL track_functions='all';
               CREATE TABLE gp_reuse_src (id int PRIMARY KEY, label text, geom geometry(LineString,3857));
               WITH line AS (
                   SELECT ST_MakeLine(ST_SetSRID(ST_Point(
                       i*1000-2000000, i*1000-1800000 + CASE WHEN i%2=0 THEN 100 ELSE -100 END
                   ),3857) ORDER BY i) AS geom
                   FROM generate_series(0,4096) i
               )
               INSERT INTO gp_reuse_src SELECT id, 'line-' || id, geom
               FROM line CROSS JOIN generate_series(1,24) id"#,
        )
        .unwrap();
        let calls = || {
            Spi::get_one::<i64>(
                "SELECT COALESCE(sum(calls),0)::bigint FROM pg_stat_xact_user_functions
                 WHERE funcname='st_simplifypreservetopology'",
            )
            .unwrap()
            .unwrap()
        };
        let build = |feature_driven| {
            Spi::run(&format!(
                r#"SELECT ST_BuildPyramid('gp_reuse_src','geom','id',
                   '{{"name":"gp_reuse","sourceSRS":3857,"minZoom":1,"maxZoom":1,
                      "preparedBuild":true,"featureDrivenEnum":{feature_driven},"attributes":["label"]}}')"#
            ))
            .unwrap();
        };
        let before = calls();
        build(false);
        let tile_major_calls = calls();
        let expected = decoded_stored_tiles("gp_reuse");
        assert!(!expected.is_empty());
        assert_eq!(tile_major_calls - before, 4 * 24);
        build(true);
        assert_eq!(calls() - tile_major_calls, 24);
        assert_eq!(decoded_stored_tiles("gp_reuse"), expected);
    }

    #[pg_test]
    fn typed_point_grid_matches_generic_and_dynamic_paths() {
        Spi::run(
            r#"CREATE TABLE gp_grid_typed_src (id int PRIMARY KEY, label text, important boolean, geom geometry);
               INSERT INTO gp_grid_typed_src
               SELECT id,'point-' || id,keep,ST_SetSRID(ST_Point(x,-200000),3857)
               FROM (VALUES (1,200000,false),(2,201000,true),(3,202000,true),
                            (4,1000000,false),(5,1001000,false)) v(id,x,keep)"#,
        )
        .unwrap();
        let build = |options: &str| {
            Spi::run(&format!(
                r#"SELECT ST_BuildPyramid('gp_grid_typed_src','geom','id',
                   '{{"name":"gp_grid_typed","sourceSRS":3857,"maxZoom":0,"attributes":["label"],
                      "buildRules":[{{"minZoom":0,"maxZoom":0,
                        "pointGrid":{{"cellPixels":64,"keepField":"important"}}}}]{options}}}')"#
            ))
            .unwrap();
        };
        build("");
        let expected = decoded_stored_tiles("gp_grid_typed");
        assert_eq!(
            expected[&(0, 0, 0)].iter().map(|f| f.0).collect::<Vec<_>>(),
            [2, 3, 4]
        );
        Spi::run("ALTER TABLE gp_grid_typed_src ALTER COLUMN geom TYPE geometry(Point,3857)")
            .unwrap();
        for options in ["", r#","preparedBuild":true"#] {
            build(options);
            assert_eq!(decoded_stored_tiles("gp_grid_typed"), expected);
        }
        Spi::run(
            "UPDATE gp_grid_typed_src SET important=true WHERE id=1;
             SELECT ST_UpdatePyramid('gp_grid_typed','id=1')",
        )
        .unwrap();
        let refreshed = decoded_stored_tiles("gp_grid_typed");
        assert_eq!(refreshed[&(0, 0, 0)].len(), 4);
        build(r#","splitSize":10"#);
        assert!(decoded_stored_tiles("gp_grid_typed").is_empty());
        assert_eq!(
            decoded_mvt_features(&tile_bytes("SELECT ST_Tile('gp_grid_typed',0,0,0)")),
            refreshed[&(0, 0, 0)]
        );
    }

    #[pg_test]
    fn split_pruning_skips_a_view_with_changing_visibility() {
        Spi::run(
            r#"CREATE SEQUENCE gp_split_visibility;
               CREATE VIEW gp_split_view AS
               SELECT id,ST_SetSRID(ST_Point(1000,1000),3857) AS geom
               FROM generate_series(1,2) id
               WHERE CASE WHEN id=2 THEN nextval('gp_split_visibility')=2 ELSE true END;
               SELECT ST_BuildPyramid('gp_split_view','geom','id',
                 '{"name":"gp_view_split","sourceSRS":3857,"maxZoom":0,"splitSize":2}')"#,
        )
        .unwrap();
        let tiles = decoded_stored_tiles("gp_view_split");
        assert_eq!(tiles[&(0, 0, 0)].len(), 2);
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

    #[pg_test]
    fn build_rules_attr_fields_and_sample_source_tiles() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("DROP TABLE IF EXISTS _gp_lod_src").unwrap();
        Spi::run(
            "CREATE TABLE _gp_lod_src(
                gid bigint PRIMARY KEY,
                geom geometry(Point, 4326),
                kind text,
                detail text,
                rank int
            )",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_lod_src VALUES
             (1, ST_SetSRID(ST_Point(10, -10), 4326), 'keep', 'large-detail', 1),
             (2, ST_SetSRID(ST_Point(30, -10), 4326), 'drop', 'large-detail', 9)",
        )
        .unwrap();

        let ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_lod_src','geom','gid',
             '{\"minZoom\":0,\"maxZoom\":1,\"attributes\":[\"kind\",\"detail\"],
                \"buildRules\":[
                  {\"minZoom\":0,\"maxZoom\":0,
                   \"filter\":\"rank < 5\",
                   \"attrFields\":[\"kind\"],
                   \"sample\":{\"rate\":100,\"key\":[\"fid\"]}}
                ]}')",
        )
        .unwrap();
        assert_eq!(ok, Some(true));

        let z0 = tile_bytes("SELECT ST_Tile('_gp_lod_src', 0, 0, 0)");
        assert!(bytea_contains(&z0, "kind"));
        assert!(!bytea_contains(&z0, "detail"));

        let z1 = tile_bytes("SELECT ST_Tile('_gp_lod_src', 1, 1, 1)");
        assert!(bytea_contains(&z1, "kind"));
        assert!(bytea_contains(&z1, "detail"));

        Spi::run("SELECT ST_DropPyramid('_gp_lod_src')").unwrap();
        Spi::run("DROP TABLE _gp_lod_src").unwrap();
    }

    #[pg_test]
    fn build_rules_attr_fields_and_sample_dynamic_tiles() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("DROP TABLE IF EXISTS _gp_lod_dyn").unwrap();
        Spi::run(
            "CREATE TABLE _gp_lod_dyn(
                gid bigint PRIMARY KEY,
                geom geometry(Point, 4326),
                kind text,
                detail text,
                rank int
            )",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_lod_dyn VALUES
             (1, ST_SetSRID(ST_Point(10, -10), 4326), 'keep', 'large-detail', 1),
             (2, ST_SetSRID(ST_Point(30, -10), 4326), 'drop', 'large-detail', 9)",
        )
        .unwrap();

        let ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_lod_dyn','geom','gid',
             '{\"minZoom\":0,\"maxZoom\":0,\"splitSize\":9999,
                \"dynamicBeyondMaxZoom\":true,
                \"attributes\":[\"kind\",\"detail\"],
                \"buildRules\":[
                  {\"minZoom\":0,\"maxZoom\":2,
                   \"filter\":\"rank < 5\",
                   \"attrFields\":[\"kind\"],
                   \"sample\":{\"rate\":100,\"key\":[\"fid\"]}}
                ]}')",
        )
        .unwrap();
        assert_eq!(ok, Some(true));

        let z0 = tile_bytes("SELECT ST_Tile('_gp_lod_dyn', 0, 0, 0)");
        assert!(bytea_contains(&z0, "kind"));
        assert!(!bytea_contains(&z0, "detail"));

        let z1 = tile_bytes("SELECT ST_Tile('_gp_lod_dyn', 1, 1, 1)");
        assert!(bytea_contains(&z1, "kind"));
        assert!(!bytea_contains(&z1, "detail"));

        Spi::run("SELECT ST_DropPyramid('_gp_lod_dyn')").unwrap();
        Spi::run("DROP TABLE _gp_lod_dyn").unwrap();
    }

    #[pg_test]
    fn build_rules_attr_fields_and_sample_prepared_tiles() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("DROP TABLE IF EXISTS _gp_lod_prep").unwrap();
        Spi::run(
            "CREATE TABLE _gp_lod_prep(
                gid bigint PRIMARY KEY,
                geom geometry(Point, 4326),
                kind text,
                detail text,
                rank int
            )",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_lod_prep VALUES
             (1, ST_SetSRID(ST_Point(10, -10), 4326), 'keep', 'large-detail', 1),
             (2, ST_SetSRID(ST_Point(30, -10), 4326), 'drop', 'large-detail', 9)",
        )
        .unwrap();

        let ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_lod_prep','geom','gid',
             '{\"minZoom\":0,\"maxZoom\":1,\"preparedBuild\":true,
                \"attributes\":[\"kind\",\"detail\"],
                \"buildRules\":[
                  {\"minZoom\":0,\"maxZoom\":0,
                   \"filter\":\"rank < 5\",
                   \"attrFields\":[\"kind\"],
                   \"sample\":{\"rate\":100,\"key\":[\"fid\",\"kind\"]}}
                ]}')",
        )
        .unwrap();
        assert_eq!(ok, Some(true));

        let z0 = tile_bytes("SELECT ST_Tile('_gp_lod_prep', 0, 0, 0)");
        assert!(bytea_contains(&z0, "kind"));
        assert!(!bytea_contains(&z0, "detail"));

        Spi::run("SELECT ST_DropPyramid('_gp_lod_prep')").unwrap();
        Spi::run("DROP TABLE _gp_lod_prep").unwrap();
    }

    #[pg_test]
    fn build_rules_sample_key_prepared_tiles() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("DROP TABLE IF EXISTS _gp_lod_sample_prep").unwrap();
        Spi::run(
            "CREATE TABLE _gp_lod_sample_prep(
                gid bigint PRIMARY KEY,
                geom geometry(Point, 4326),
                kind text
            )",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_lod_sample_prep VALUES
             (1, ST_SetSRID(ST_Point(10, -10), 4326), 'keep')",
        )
        .unwrap();

        let ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_lod_sample_prep','geom','gid',
             '{\"minZoom\":0,\"maxZoom\":0,\"preparedBuild\":true,
                \"buildRules\":[
                  {\"minZoom\":0,\"maxZoom\":0,
                   \"sample\":{\"rate\":0,\"key\":[\"fid\",\"kind\"]}}
                ]}')",
        )
        .unwrap();
        assert_eq!(ok, Some(true));
        assert!(
            Spi::get_one::<Vec<u8>>("SELECT ST_Tile('_gp_lod_sample_prep', 0, 0, 0)")
                .unwrap()
                .is_none(),
            "sample rate 0 with a non-fid key should build and filter all rows"
        );

        Spi::run("SELECT ST_DropPyramid('_gp_lod_sample_prep')").unwrap();
        Spi::run("DROP TABLE _gp_lod_sample_prep").unwrap();
    }

    #[pg_test]
    fn build_rules_attr_fields_and_sample_feature_major_points() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("DROP TABLE IF EXISTS _gp_lod_fm").unwrap();
        Spi::run(
            "CREATE TABLE _gp_lod_fm(
                gid bigint PRIMARY KEY,
                geom geometry(Point, 4326),
                kind text,
                detail text,
                rank int
            )",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_lod_fm VALUES
             (1, ST_SetSRID(ST_Point(-100, 0), 4326), 'keep', 'large-detail', 1),
             (2, ST_SetSRID(ST_Point(100, 0), 4326), 'drop', 'large-detail', 9)",
        )
        .unwrap();

        record_prepared_indexes();
        let before = prepared_geom_index_count();
        let ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_lod_fm','geom','gid',
             '{\"minZoom\":0,\"maxZoom\":5,\"preparedBuild\":true,
                \"featureMajorPoints\":true,\"featureMajorMinZoom\":0,
                \"attributes\":[\"kind\",\"detail\"],
                \"buildRules\":[
                  {\"minZoom\":5,\"maxZoom\":5,
                   \"filter\":\"rank < 5\",
                   \"attrFields\":[\"kind\"],
                   \"sample\":{\"rate\":100,\"key\":[\"fid\"]}}
                ]}')",
        )
        .unwrap();
        assert_eq!(ok, Some(true));
        assert_eq!(
            prepared_geom_index_count(),
            before,
            "feature-major point path should not create the prepared geom index"
        );

        let z5 = tile_bytes("SELECT ST_Tile('_gp_lod_fm', 5, 7, 16)");
        assert!(bytea_contains(&z5, "kind"));
        assert!(!bytea_contains(&z5, "detail"));

        Spi::run("SELECT ST_DropPyramid('_gp_lod_fm')").unwrap();
        Spi::run("DROP TABLE _gp_lod_fm").unwrap();
    }

    #[pg_test]
    fn build_rules_attr_fields_and_sample_feature_driven() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("DROP TABLE IF EXISTS _gp_lod_fd").unwrap();
        Spi::run(
            "CREATE TABLE _gp_lod_fd(
                gid bigint PRIMARY KEY,
                geom geometry(Geometry, 4326),
                kind text,
                detail text,
                rank int
            )",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO _gp_lod_fd VALUES
             (1, ST_SetSRID(ST_Point(-100, 0), 4326), 'keep', 'large-detail', 1),
             (2, ST_SetSRID(ST_Point(100, 0), 4326), 'drop', 'large-detail', 900),
             (3, ST_SetSRID(ST_MakeLine(ST_MakePoint(-130, 20), ST_MakePoint(-120, 25)), 4326), 'keep_line', 'large-detail', 2),
             (4, ST_SetSRID(ST_MakeEnvelope(-140, 30, -135, 35, 4326), 4326), 'keep_poly', 'large-detail', 3),
             (5, ST_SetSRID(ST_Point(-170, -75), 4326), 'outlier', 'large-detail', 9001),
             (6, ST_SetSRID(ST_Point(170, 78), 4326), 'outlier', 'large-detail', 9002)",
        )
        .unwrap();

        let ok = Spi::get_one::<bool>(
            "SELECT ST_BuildPyramid('_gp_lod_fd','geom','gid',
             '{\"minZoom\":0,\"maxZoom\":6,\"preparedBuild\":true,
                \"featureDrivenEnum\":true,
                \"attributes\":[\"kind\",\"detail\"],
                \"buildRules\":[
                  {\"minZoom\":6,\"maxZoom\":6,
                   \"filter\":\"rank < 100\",
                   \"attrFields\":[\"kind\"],
                   \"sample\":{\"rate\":100,\"key\":[\"fid\"],
                             \"geometryTypes\":[\"POINT\",\"LINESTRING\",\"POLYGON\"]}}
                ]}')",
        )
        .unwrap();
        assert_eq!(ok, Some(true));

        let z6 = tile_bytes("SELECT ST_Tile('_gp_lod_fd', 6, 14, 32)");
        assert!(bytea_contains(&z6, "kind"));
        assert!(!bytea_contains(&z6, "detail"));

        Spi::run("SELECT ST_DropPyramid('_gp_lod_fd')").unwrap();
        Spi::run("DROP TABLE _gp_lod_fd").unwrap();
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
    #[pg_test]
    fn shadow_names_do_not_destroy_other_pyramids() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("CREATE TABLE _gp_name_src AS SELECT 1::bigint AS id, ST_SetSRID(ST_MakePoint(1,1),4326) AS geom").unwrap();
        for name in ["name_collision__shadow", "name_collision"] {
            Spi::run(&format!(r#"SELECT ST_BuildPyramid('_gp_name_src','geom','id','{{"name":"{name}","minZoom":0,"maxZoom":0}}')"#)).unwrap();
        }
        let before = tile_bytes("SELECT ST_Tile('name_collision__shadow',0,0,0)");
        Spi::run(
            "SELECT ST_UpdatePyramid('name_collision',''); SELECT ST_DropPyramid('name_collision')",
        )
        .unwrap();
        assert_eq!(
            tile_bytes("SELECT ST_Tile('name_collision__shadow',0,0,0)"),
            before
        );
        // Exercise the longest permitted name, including its owned sequence.
        let long_name = "n".repeat(45);
        Spi::run(&format!(r#"SELECT ST_BuildPyramid('_gp_name_src','geom','id','{{"name":"{long_name}","minZoom":0,"maxZoom":0}}'); SELECT ST_UpdatePyramid('{long_name}',''); SELECT ST_DropPyramid('{long_name}')"#)).unwrap();
        Spi::run("SELECT ST_DropPyramid('name_collision__shadow'); DROP TABLE _gp_name_src")
            .unwrap();
    }

    #[pg_test]
    fn full_rebuild_replaces_zoom_stats_including_empty_source() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("CREATE TABLE _gp_replace_stats AS SELECT 1::bigint AS id, ST_SetSRID(ST_MakePoint(1,1),4326) AS geom").unwrap();
        for (min_zoom, max_zoom) in [(0, 2), (1, 1)] {
            Spi::run(&format!(r#"SELECT ST_BuildPyramid('_gp_replace_stats','geom','id','{{"minZoom":{min_zoom},"maxZoom":{max_zoom}}}')"#)).unwrap();
        }
        assert_eq!(Spi::get_one::<String>("SELECT array_agg(z ORDER BY z)::text FROM geometry_pyramid_stats WHERE pyramid_name='_gp_replace_stats'").unwrap().as_deref(), Some("{1}"));
        // Failure after deleting stats must restore the old statistics as well.
        Spi::run(r#"DO $$ BEGIN
          PERFORM ST_BuildPyramid('_gp_replace_stats','geom','id','{"minZoom":0,"maxZoom":0,"attributes":["missing_column"]}');
          EXCEPTION WHEN undefined_column THEN NULL; END $$"#).unwrap();
        assert_eq!(Spi::get_one::<i64>("SELECT count(*) FROM geometry_pyramid_stats WHERE pyramid_name='_gp_replace_stats' AND z=1 AND tile_count>0").unwrap(), Some(1));
        Spi::run("DELETE FROM _gp_replace_stats; SELECT ST_UpdatePyramid('_gp_replace_stats','')")
            .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM _gp_tiles__gp_replace_stats").unwrap(),
            Some(0)
        );
        assert_eq!(Spi::get_one::<i64>("SELECT count(*) FROM geometry_pyramid_stats WHERE pyramid_name='_gp_replace_stats'").unwrap(), Some(0));
        Spi::run("SELECT ST_DropPyramid('_gp_replace_stats'); DROP TABLE _gp_replace_stats")
            .unwrap();
    }

    #[pg_test]
    fn incremental_refresh_matches_full_build_at_buffer_boundaries() {
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        Spi::run("CREATE TABLE _gp_buffer_src (id bigint, geom geometry(Point,3857), label text)")
            .unwrap();
        Spi::run("INSERT INTO _gp_buffer_src VALUES (1,ST_SetSRID(ST_MakePoint(-10000000,-10000000),3857),'anchor'),(2,ST_SetSRID(ST_MakePoint(10000000,10000000),3857),'anchor')").unwrap();
        for (buffer, x) in [(0, 0), (8, 1), (256, 1)] {
            Spi::run(&format!("INSERT INTO _gp_buffer_src VALUES (3,ST_SetSRID(ST_MakePoint({x},1000000),3857),'before')")).unwrap();
            Spi::run(&format!(r#"SELECT ST_BuildPyramid('_gp_buffer_src','geom','id','{{"name":"buffer_refresh","sourceSRS":3857,"minZoom":1,"maxZoom":3,"tileExtend":{buffer},"attributes":["label"]}}')"#)).unwrap();
            Spi::run("UPDATE _gp_buffer_src SET label='after' WHERE id=3; SELECT ST_UpdatePyramid('buffer_refresh','id=3')").unwrap();
            let incremental = decoded_stored_tiles("buffer_refresh");
            Spi::run("SELECT ST_UpdatePyramid('buffer_refresh','')").unwrap();
            assert_eq!(
                incremental,
                decoded_stored_tiles("buffer_refresh"),
                "attribute update, buffer={buffer}"
            );
            Spi::run("DELETE FROM _gp_buffer_src WHERE id=3").unwrap();
            Spi::run(&format!(
                "SELECT ST_UpdatePyramidByExtent('buffer_refresh',{x},1000000,{x},1000000,3857)"
            ))
            .unwrap();
            let incremental = decoded_stored_tiles("buffer_refresh");
            Spi::run("SELECT ST_UpdatePyramid('buffer_refresh','')").unwrap();
            assert_eq!(
                incremental,
                decoded_stored_tiles("buffer_refresh"),
                "deletion, buffer={buffer}"
            );
            Spi::run("SELECT ST_DropPyramid('buffer_refresh')").unwrap();
        }
        Spi::run("DROP TABLE _gp_buffer_src").unwrap();
    }

    #[pg_test(error = "tileSRS must be 3857; other tile coordinate systems are not supported")]
    fn build_rejects_unsupported_tile_srs_before_registering() {
        Spi::run(r#"SELECT ST_BuildPyramid('unused_source','geom','id','{"tileSRS":4326}')"#)
            .unwrap();
    }

    #[pg_test(error = "tileSRS must be 3857; other tile coordinate systems are not supported")]
    fn tile_rejects_unsupported_srs_in_existing_catalog() {
        Spi::run(r#"INSERT INTO geometry_pyramid_catalog (pyramid_name,source_table,geom_column,fid_column,config) VALUES ('invalid_srs','unused_source','geom','id','{"tileSRS":4326}')"#).unwrap();
        Spi::run("SELECT ST_Tile('invalid_srs',0,0,0)").unwrap();
    }

    #[pg_test]
    fn worker_sql_error_reports_cause_and_rolls_back() {
        use crate::worker::{exec_sql_autonomous, exec_sql_autonomous_timeout};
        exec_sql_autonomous(
            "DROP TABLE IF EXISTS _gp_worker_rollback; CREATE TABLE _gp_worker_rollback (id int)",
        )
        .unwrap();
        let error = exec_sql_autonomous_timeout(
            "INSERT INTO _gp_worker_rollback VALUES (1); SELECT 1/0",
            0,
        )
        .unwrap_err();
        assert!(error.to_string().contains("division by zero"), "{error}");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM _gp_worker_rollback").unwrap(),
            Some(0)
        );
        Spi::run("DROP TABLE _gp_worker_rollback").unwrap();
    }

    #[pg_test]
    fn worker_exit_without_status_is_reported_without_timeout() {
        let error = crate::worker::exec_sql_autonomous_timeout(
            "SELECT pg_terminate_backend(pg_backend_pid())",
            0,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("worker exited without reporting completion"),
            "{error}"
        );
    }

    #[pg_test]
    fn worker_timeout_stops_process_and_removes_ipc() {
        use crate::worker::{exec_sql_autonomous, exec_sql_autonomous_timeout};
        exec_sql_autonomous(
            "DROP TABLE IF EXISTS _gp_worker_timeout; CREATE TABLE _gp_worker_timeout (id int)",
        )
        .unwrap();
        let start = std::time::Instant::now();
        let error = exec_sql_autonomous_timeout(
            "INSERT INTO _gp_worker_timeout VALUES (1); SELECT pg_sleep(10)",
            200,
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(
            start.elapsed().as_secs() < 5,
            "termination must interrupt SQL"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM _gp_worker_timeout").unwrap(),
            Some(0)
        );
        let prefix = format!("gp_{}_", unsafe { pg_sys::MyProcPid });
        assert!(
            !std::fs::read_dir(crate::worker::job_dir())
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&prefix)),
            "worker IPC files must be removed after shutdown"
        );
        Spi::run("DROP TABLE _gp_worker_timeout").unwrap();
    }

    #[pg_test]
    fn worker_guard_stops_process_on_coordinator_error() {
        use crate::worker::{Job, WorkerJob, WorkerMode};
        let job = Job::new(WorkerMode::ExecSql {
            sql: "SELECT pg_sleep(10)".into(),
        })
        .unwrap();
        let filename = crate::worker::job_filename(
            unsafe { pg_sys::MyProcPid } as u32,
            crate::worker::next_job_idx(),
        );
        let start = std::time::Instant::now();
        // A Rust panic follows the same Drop path as PostgreSQL errors converted
        // by pgrx. It avoids poisoning the coordinator transaction in this test.
        let result = std::panic::catch_unwind(|| {
            let _worker = WorkerJob::start(&filename, &job).unwrap();
            panic!("coordinator failed after spawning a worker");
        });
        assert!(result.is_err());
        assert!(start.elapsed().as_secs() < 5);
        assert!(!crate::worker::job_dir().join(&filename).exists());
    }

    #[pg_test]
    fn parallel_build_preserves_effective_role_rls_and_search_path() {
        use crate::worker::exec_sql_autonomous;
        Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
        exec_sql_autonomous(r#"
            CREATE ROLE gp_context_role NOLOGIN NOSUPERUSER;
            CREATE SCHEMA gp_context;
            GRANT USAGE, CREATE ON SCHEMA gp_context TO gp_context_role;
            CREATE TABLE gp_context.source (id bigint, geom geometry(Point,4326), allowed bool);
            INSERT INTO gp_context.source VALUES
                (1,ST_SetSRID(ST_MakePoint(-170,70),4326),true),
                (2,ST_SetSRID(ST_MakePoint(170,-70),4326),true),
                (3,ST_SetSRID(ST_MakePoint(10,10),4326),false);
            ALTER TABLE gp_context.source ENABLE ROW LEVEL SECURITY;
            CREATE POLICY visible_rows ON gp_context.source USING (allowed AND current_user='gp_context_role');
            GRANT SELECT ON gp_context.source TO gp_context_role;
            CREATE FUNCTION gp_context.check_role() RETURNS bool LANGUAGE plpgsql AS $$ BEGIN
                IF current_user <> 'gp_context_role' THEN RAISE EXCEPTION 'worker changed effective role'; END IF;
                RETURN true;
            END $$;
        "#).unwrap();
        Spi::run("GRANT SELECT,INSERT,UPDATE,DELETE ON geometry_pyramid_catalog,geometry_pyramid_stats TO gp_context_role; SET LOCAL search_path=gp_context,public; SET LOCAL ROLE gp_context_role").unwrap();
        for (name, parallel, prepared) in [
            ("context_serial", 1, false),
            ("context_parallel", 2, false),
            ("context_prepared", 2, true),
        ] {
            Spi::run(&format!(r#"SELECT ST_BuildPyramid('source','geom','id','{{"name":"{name}","minZoom":4,"maxZoom":4,"parallel":{parallel},"preparedBuild":{prepared},"buildTimeoutMs":5000,"buildRules":[{{"minZoom":4,"maxZoom":4,"filter":"check_role()"}}]}}')"#)).unwrap();
        }
        let serial = decoded_stored_tiles("context_serial");
        assert!(!serial.is_empty());
        assert!(
            serial.values().flatten().all(|f| f.0 != 3),
            "RLS must hide fid=3"
        );
        assert_eq!(serial, decoded_stored_tiles("context_parallel"));
        assert_eq!(serial, decoded_stored_tiles("context_prepared"));
        // A session row_security=off must continue to reject a protected read,
        // rather than silently re-enabling RLS inside a worker.
        Spi::run("SET LOCAL row_security=off").unwrap();
        let error = exec_sql_autonomous("SELECT count(*) FROM source").unwrap_err();
        assert!(error.to_string().contains("row-level security"), "{error}");
        Spi::run("SET LOCAL row_security=on; SELECT ST_DropPyramid('context_serial'); SELECT ST_DropPyramid('context_parallel'); SELECT ST_DropPyramid('context_prepared'); RESET ROLE; SET LOCAL search_path=public; DROP SCHEMA gp_context CASCADE; REVOKE ALL ON geometry_pyramid_catalog,geometry_pyramid_stats FROM gp_context_role; DROP ROLE gp_context_role").unwrap();
    }
    #[pg_test]
    fn full_refresh_and_dynamic_cover_the_outer_buffer() {
        Spi::run("CREATE TABLE edge_src AS SELECT 1 id, ST_SetSRID(ST_Point(1,1),3857) geom")
            .unwrap();
        for buffer in [0, 8] {
            if buffer == 0 {
                Spi::run("UPDATE edge_src SET geom=ST_SetSRID(ST_Point(0,0),3857)").unwrap();
            } else {
                Spi::run("UPDATE edge_src SET geom=ST_SetSRID(ST_Point(1,1),3857)").unwrap();
            }
            Spi::run(&format!(r#"SELECT ST_BuildPyramid('edge_src','geom','id','{{"name":"edge","minZoom":1,"maxZoom":1,"sourceSRS":3857,"tileExtend":{buffer}}}')"#)).unwrap();
            let before = decoded_stored_tiles("edge");
            assert_eq!(before.len(), 4);
            Spi::run("SELECT ST_UpdatePyramid('edge','id=1')").unwrap();
            assert_eq!(before, decoded_stored_tiles("edge"));
        }
    }

    #[pg_test]
    fn projected_crs_preserves_tiles_across_build_and_refresh() {
        for (srid, lon, lat) in [(32631, 3, 50), (3413, -90, 75)] {
            Spi::run(&format!("CREATE TEMP TABLE projected AS SELECT 1 id, ST_Transform(ST_SetSRID(ST_Point({lon},{lat}),4326),{srid}) geom")).unwrap();
            for prepared in [false, true] {
                Spi::run(&format!(r#"SELECT ST_BuildPyramid('projected','geom','id','{{"name":"crs","sourceSRS":{srid},"maxZoom":0,"preparedBuild":{prepared}}}')"#)).unwrap();
                let full = decoded_stored_tiles("crs");
                assert_eq!(full.len(), 1);
                Spi::run("SELECT ST_UpdatePyramid('crs','id=1')").unwrap();
                assert_eq!(full, decoded_stored_tiles("crs"));
            }
            Spi::run(&format!(r#"SELECT ST_BuildPyramid('projected','geom','id','{{"name":"crs","sourceSRS":{srid},"maxZoom":0,"splitSize":2}}')"#)).unwrap();
            assert!(
                Spi::get_one::<bool>("SELECT ST_Tile('crs',0,0,0) IS NOT NULL")
                    .unwrap()
                    .unwrap()
            );
            Spi::run("SELECT ST_DropPyramid('crs'); DROP TABLE projected").unwrap();
        }
    }

    #[pg_test]
    fn geographic_candidate_keeps_wrapped_longitudes_and_world_edges() {
        Spi::run("CREATE TABLE geo_edge AS SELECT id,ST_SetSRID(ST_MakePoint(lon,lat),4326) geom FROM
                  (VALUES (1,181.,1.),(2,-181.,-1.),(3,180.,85.0511287),(4,-180.,-85.0511287),
                          (5,0.000001,0.000001)) p(id,lon,lat); CREATE INDEX ON geo_edge USING gist(geom)").unwrap();
        let mut baseline = None;
        for prepared in [false, true] {
            Spi::run(&format!(r#"SELECT ST_BuildPyramid('geo_edge','geom','id','{{"name":"geo","maxZoom":3,"preparedBuild":{prepared}}}')"#)).unwrap();
            let tiles = decoded_stored_tiles("geo");
            if let Some(previous) = &baseline {
                assert_eq!(previous, &tiles);
            }
            assert!(tiles.values().flatten().any(|f| f.0 == 1));
            baseline = Some(tiles);
        }
    }

    #[pg_test]
    fn a_wrapped_refresh_extent_removes_tiles_at_the_world_edge() {
        Spi::run("CREATE TABLE date_edge AS SELECT 1 id,ST_SetSRID(ST_Point(179.99,1),4326) geom")
            .unwrap();
        Spi::run(r#"SELECT ST_BuildPyramid('date_edge','geom','id','{"minZoom":9,"maxZoom":9}')"#)
            .unwrap();
        assert!(!decoded_stored_tiles("date_edge").is_empty());
        Spi::run(
            "DELETE FROM date_edge; SELECT ST_UpdatePyramidByExtent('date_edge',179,0,181,2,4326)",
        )
        .unwrap();
        assert!(decoded_stored_tiles("date_edge").is_empty());
    }

    #[pg_test]
    fn prepared_input_is_pinned_to_the_temp_schema() {
        Spi::run(
            "SET LOCAL search_path=public,pg_temp; CREATE TABLE actual_src AS
                 SELECT 1 id,ST_SetSRID(ST_Point(100,200),3857) geom",
        )
        .unwrap();
        let next = crate::worker::next_job_idx() + 1;
        let pid = unsafe { pgrx::pg_sys::MyProcPid };
        Spi::run(&format!(
            "CREATE TABLE public._gp_prep_{pid}_{next}(unrelated int)"
        ))
        .unwrap();
        Spi::run(r#"SELECT ST_BuildPyramid('actual_src','geom','id','{"maxZoom":0,"sourceSRS":3857,"preparedBuild":true}')"#).unwrap();
        assert_eq!(decoded_stored_tiles("actual_src").len(), 1);
        assert_eq!(
            Spi::get_one::<i64>(&format!(
                "SELECT count(unrelated) FROM public._gp_prep_{pid}_{next}"
            ))
            .unwrap(),
            Some(0)
        );
    }

    #[pg_test]
    fn unmanaged_names_and_replacement_objects_survive_lifecycle_calls() {
        Spi::run(
            "CREATE TABLE _gp_tiles_unmanaged(id int); INSERT INTO _gp_tiles_unmanaged VALUES(42)",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT ST_DropPyramid('unmanaged')").unwrap(),
            Some(false)
        );
        assert_eq!(
            Spi::get_one::<i32>("SELECT id FROM _gp_tiles_unmanaged").unwrap(),
            Some(42)
        );
        Spi::run(
            "CREATE TABLE _gp_tiles_collision AS SELECT 1 id,ST_SetSRID(ST_Point(1,1),3857) geom",
        )
        .unwrap();
        Spi::run(r#"DO $$ BEGIN
            BEGIN PERFORM ST_BuildPyramid('_gp_tiles_collision','geom','id','{"name":"collision","sourceSRS":3857,"maxZoom":0}');
            EXCEPTION WHEN OTHERS THEN RETURN; END;
            RAISE EXCEPTION 'unmanaged output was accepted'; END $$"#).unwrap();
        assert_eq!(
            Spi::get_one::<i32>("SELECT id FROM _gp_tiles_collision WHERE geom IS NOT NULL")
                .unwrap(),
            Some(1)
        );
        Spi::run(r#"SELECT ST_BuildPyramid('_gp_tiles_collision','geom','id','{"name":"managed","sourceSRS":3857,"maxZoom":0}'); DROP TABLE _gp_tiles_managed; CREATE TABLE _gp_tiles_managed(id int); INSERT INTO _gp_tiles_managed VALUES(73)"#).unwrap();
        Spi::run("DO $$ BEGIN PERFORM ST_DropPyramid('managed'); EXCEPTION WHEN OTHERS THEN NULL; END $$").unwrap();
        assert_eq!(
            Spi::get_one::<i32>("SELECT id FROM _gp_tiles_managed").unwrap(),
            Some(73)
        );
    }

    #[pg_test]
    fn a_registered_table_can_be_dropped_after_its_config_becomes_invalid() {
        Spi::run("CREATE TABLE bad_config_src AS SELECT 1 id,ST_SetSRID(ST_Point(1,1),4326) geom")
            .unwrap();
        Spi::run(r#"SELECT ST_BuildPyramid('bad_config_src','geom','id','{"maxZoom":0}');
                  UPDATE public.geometry_pyramid_catalog SET config='{"tileSRS":4326}' WHERE pyramid_name='bad_config_src'"#).unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT ST_DropPyramid('bad_config_src')").unwrap(),
            Some(true)
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT to_regclass('public._gp_tiles_bad_config_src') IS NULL")
                .unwrap(),
            Some(true)
        );
    }

    #[pg_test]
    fn rebuilding_only_writes_the_registered_heap() {
        Spi::run("CREATE TABLE heap_source AS SELECT 1 id,ST_SetSRID(ST_Point(1,1),3857) geom")
            .unwrap();
        Spi::run(r#"SELECT ST_BuildPyramid('heap_source','geom','id','{"name":"heap","sourceSRS":3857,"maxZoom":0}');
                 CREATE TABLE child_data () INHERITS (_gp_tiles_heap);
                 INSERT INTO child_data SELECT * FROM ONLY _gp_tiles_heap;
                 SELECT ST_UpdatePyramid('heap','id=1'); SELECT ST_UpdatePyramid('heap','')"#).unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM child_data").unwrap(),
            Some(1)
        );
        assert_eq!(Spi::get_one::<i64>("SELECT tile_count FROM public.geometry_pyramid_stats WHERE pyramid_name='heap' AND z=0").unwrap(), Some(1));
    }

    #[pg_test]
    fn a_deferred_tile_uses_the_same_effective_budget_as_a_stored_tile() {
        Spi::run(
            "CREATE TABLE budget_split AS SELECT i id, ST_Translate(ST_Centroid(
            ST_TileEnvelope(1,CASE WHEN i<=1000 THEN 0 ELSE 1 END,0)),i%100,i/100) geom
            FROM generate_series(1,1060)i",
        )
        .unwrap();
        let build = |split| {
            Spi::run(&format!(
                r#"SELECT ST_BuildPyramid('budget_split','geom','id',
          '{{"name":"bs","sourceSRS":3857,"minZoom":1,"maxZoom":1,"splitSize":{split},
            "tileSizeBudgetBytes":1200,"maxTileSizeAttempts":12}}')"#
            ))
            .unwrap()
        };
        build(0);
        let expected = decoded_mvt_features(&tile_bytes("SELECT ST_Tile('bs',1,1,0)"));
        assert!(!expected.is_empty() && expected.len() < 30);
        build(30);
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM _gp_tiles_bs WHERE x=1").unwrap(),
            Some(0)
        );
        assert_eq!(
            decoded_mvt_features(&tile_bytes("SELECT ST_Tile('bs',1,1,0)")),
            expected
        );
        // A 0.1.0 row cannot reconstruct a tightened budget. A refresh recovers it.
        Spi::run("UPDATE public.geometry_pyramid_stats SET effective_knobs=NULL WHERE pyramid_name='bs';
            DO $$ BEGIN BEGIN PERFORM ST_Tile('bs',1,1,0);
                EXCEPTION WHEN OTHERS THEN IF SQLERRM LIKE '%legacy budget settings%' THEN RETURN; END IF; RAISE; END;
                RAISE EXCEPTION 'legacy LOD silently changed'; END $$;
            SELECT ST_UpdatePyramid('bs','id=1001')").unwrap();
        assert_eq!(
            decoded_mvt_features(&tile_bytes("SELECT ST_Tile('bs',1,1,0)")),
            expected
        );
    }

    #[pg_test]
    fn budgeted_refresh_and_first_incremental_stats_match_full() {
        Spi::run("CREATE TABLE budget_src(id int,geom geometry(Point,3857))").unwrap();
        Spi::run(r#"SELECT ST_BuildPyramid('budget_src','geom','id','{"name":"budget","sourceSRS":3857,"maxZoom":0,"tileSizeBudgetBytes":1200,"maxTileSizeAttempts":12}')"#).unwrap();
        Spi::run("INSERT INTO budget_src SELECT i,ST_SetSRID(ST_Point(10+i%100,10+i/100),3857) FROM generate_series(1,1000)i; SELECT ST_UpdatePyramid('budget','true')").unwrap();
        let full_first = decoded_stored_tiles("budget");
        assert_eq!(
            Spi::get_one::<i64>("SELECT tile_count FROM public.ST_Pyramids() WHERE name='budget'")
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT tile_count FROM public.geometry_pyramid_stats WHERE pyramid_name='budget'"
            )
            .unwrap(),
            Some(1)
        );
        Spi::run("SELECT ST_UpdatePyramid('budget','')").unwrap();
        let full = decoded_stored_tiles("budget");
        assert_eq!(full_first, full);
        Spi::run("SELECT ST_UpdatePyramid('budget','id=1')").unwrap();
        assert_eq!(full, decoded_stored_tiles("budget"));
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
