-- bench/build_speed.sql — first-full-build speed benchmark for ST_BuildPyramid.
--
-- Manual by design: it creates a large deterministic mixed point/line/polygon
-- dataset and records wall-clock timing + per-zoom build stats, so it is the
-- stable baseline the build-speed work (docs/build-speed-deep-dive.md,
-- docs/superpowers/plans/2026-06-26-build-speed-feature-major.md) compares
-- against. It does NOT depend on any unimplemented config key.
--
-- Usage:
--   psql "$DATABASE_URL" \
--     -v pyramid_name=gp_bench \
--     -v max_zoom=8 \
--     -v scale=1000000 \
--     -f bench/build_speed.sql
--
--   scale = number of dense points (lines ≈ scale/3, polygons ≈ scale/4).
--           Crank it to 1e7 / 1e8 on target hardware for the real numbers.
--
-- Read the output bottom-up:
--   * "geometry distribution"  — what was built.
--   * "Rz tile-cells per zoom" — how many tile cells the CURRENT (tile-major)
--     build enumerates and GiST-probes at each zoom, INCLUDING empty ones.
--     This is the cost feature-major construction removes; watch it explode
--     at high zoom.
--   * total build wall-clock + per-zoom geometry_pyramid_stats.

\set ON_ERROR_STOP on
\timing on

\if :{?pyramid_name}
\else
  \set pyramid_name gp_bench
\endif
\if :{?max_zoom}
\else
  \set max_zoom 8
\endif
\if :{?scale}
\else
  \set scale 1000000
\endif

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;

\echo '== building source dataset (scale =' :scale ') =='

DROP TABLE IF EXISTS _gp_bench_mixed;
CREATE TABLE _gp_bench_mixed (
    gid  bigserial PRIMARY KEY,
    kind text    NOT NULL,
    rank integer NOT NULL,
    geom geometry(Geometry, 4326) NOT NULL
);

-- Dense points: a lattice over ~1°×1° (Beijing-ish), many points per high-zoom tile.
INSERT INTO _gp_bench_mixed (kind, rank, geom)
SELECT 'point', i,
       ST_SetSRID(ST_MakePoint(
           116.0 + ((i % 1000)::float8) * 0.001,
           39.5  + (((i / 1000) % 1000)::float8) * 0.001), 4326)
FROM generate_series(1, (:scale)::bigint) AS i;

-- Lines: short segments over the same area.
INSERT INTO _gp_bench_mixed (kind, rank, geom)
SELECT 'line', i,
       ST_SetSRID(ST_MakeLine(
           ST_MakePoint(116.0 + ((i % 800)::float8)*0.00125,        39.5 + (((i / 800) % 800)::float8)*0.00125),
           ST_MakePoint(116.0 + ((i % 800)::float8)*0.00125 + 8e-4, 39.5 + (((i / 800) % 800)::float8)*0.00125 + 8e-4)
       ), 4326)
FROM generate_series(1, ((:scale)/3)::bigint) AS i;

-- Polygons: small boxes over the same area.
INSERT INTO _gp_bench_mixed (kind, rank, geom)
SELECT 'polygon', i,
       ST_SetSRID(ST_MakeEnvelope(
           116.0 + ((i % 600)::float8)*0.0016,        39.5 + (((i / 600) % 600)::float8)*0.0016,
           116.0 + ((i % 600)::float8)*0.0016 + 1e-3, 39.5 + (((i / 600) % 600)::float8)*0.0016 + 1e-3,
           4326), 4326)
FROM generate_series(1, ((:scale)/4)::bigint) AS i;

-- Sparse points: 50 widely scattered points. They each land alone in their tile
-- (exercise splitSize deferral) and widen the dataset bbox, inflating R(z).
INSERT INTO _gp_bench_mixed (kind, rank, geom)
SELECT 'sparse_point', i,
       ST_SetSRID(ST_MakePoint(100.0 + i * 0.4, 25.0 + i * 0.2), 4326)
FROM generate_series(1, 50) AS i;

CREATE INDEX _gp_bench_mixed_gix ON _gp_bench_mixed USING gist (geom);
ANALYZE _gp_bench_mixed;

\echo '== geometry distribution =='
SELECT
    count(*)                                                                      AS total_rows,
    count(*) FILTER (WHERE GeometryType(geom) IN ('POINT','MULTIPOINT'))          AS point_rows,
    count(*) FILTER (WHERE GeometryType(geom) IN ('LINESTRING','MULTILINESTRING'))AS line_rows,
    count(*) FILTER (WHERE GeometryType(geom) IN ('POLYGON','MULTIPOLYGON'))      AS polygon_rows
FROM _gp_bench_mixed;

\echo '== Rz: tile-cells enumerated+probed per zoom by the current tile-major build =='
-- R(z) = (#tiles spanned in x) * (#tiles spanned in y) over the data bbox.
-- Web Mercator half-extent H = 20037508.342789244; tile span at z = 2H / 2^z.
WITH b AS (
    SELECT ST_XMin(e) AS xmin, ST_YMin(e) AS ymin, ST_XMax(e) AS xmax, ST_YMax(e) AS ymax
    FROM (SELECT ST_Extent(ST_Transform(geom, 3857)) AS e FROM _gp_bench_mixed) s
)
SELECT zz AS z,
       (floor((b.xmax + 20037508.342789244) / (2*20037508.342789244 / pow(2, zz)))
      - floor((b.xmin + 20037508.342789244) / (2*20037508.342789244 / pow(2, zz))) + 1)::bigint
     * (floor((20037508.342789244 - b.ymin) / (2*20037508.342789244 / pow(2, zz)))
      - floor((20037508.342789244 - b.ymax) / (2*20037508.342789244 / pow(2, zz))) + 1)::bigint
       AS rect_tile_cells_Rz
FROM generate_series(0, (:max_zoom)::int) AS zz, b
ORDER BY zz;

\echo '== building pyramid' :'pyramid_name' '(maxZoom =' :max_zoom ') =='
SELECT ST_DropPyramid(:'pyramid_name');         -- false (not error) if absent

SELECT clock_timestamp() AS build_started_at;
SELECT ST_BuildPyramid(
    '_gp_bench_mixed', 'geom', 'gid',
    jsonb_build_object(
        'name',       :'pyramid_name',
        'minZoom',    0,
        'maxZoom',    (:max_zoom)::int,
        'splitSize',  100,
        'attributes', jsonb_build_array('kind', 'rank')
    )::text
) AS built;
SELECT clock_timestamp() AS build_finished_at;

\echo '== pyramid metadata =='
SELECT ST_Pyramid(:'pyramid_name') AS pyramid_info;

\echo '== per-zoom build stats (the comparison baseline) =='
SELECT z, tile_count, total_bytes, max_tile_bytes, build_ms, attempts
FROM geometry_pyramid_stats
WHERE pyramid_name = :'pyramid_name'
ORDER BY z;
