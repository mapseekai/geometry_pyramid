-- bench/build_speed.sql — first-full-build speed benchmark for ST_BuildPyramid.
--
-- Manual by design: it creates a large deterministic mixed point/line/polygon
-- dataset and records wall-clock timing + per-zoom build stats, so it is the
-- stable baseline the build-speed work (docs/build-speed-deep-dive.md,
-- docs/superpowers/plans/2026-06-26-build-speed-feature-major.md) compares
-- against.
--
-- Usage:
--   psql "$DATABASE_URL" \
--     -v pyramid_name=gp_bench \
--     -v max_zoom=8 \
--     -v scale=1000000 \
--     -v prepared_build=false \
--     -v ab_zoom=8 \
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
--   * "A/B encode-cost" — EXPLAIN ANALYZE of the old (ST_AsMVT in SELECT+HAVING)
--     vs new (ST_AsMVT once in a CTE) tile-aggregation forms over identical
--     input, to settle whether the single-pass refactor (commit 4cc10ff) is a
--     real speedup or a no-op that PostgreSQL's Aggref de-duplication already
--     gave us. Compare the second-run "Execution Time" of each form.

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
\if :{?prepared_build}
\else
  \set prepared_build false
\endif
\if :{?feature_major_points}
\else
  \set feature_major_points false
\endif
\if :{?point_only}
\else
  \set point_only false
\endif
\if :{?ab_zoom}
\else
  \set ab_zoom :max_zoom
\endif
\if :{?ab_split}
\else
  \set ab_split 100
\endif

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;

\echo '== building source dataset (scale =' :scale ', point_only =' :point_only ') =='

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

\if :point_only
\else
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
\endif

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
        'preparedBuild', (:prepared_build)::boolean,
        'featureMajorPoints', (:feature_major_points)::boolean,
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

-- ===========================================================================
-- A/B encode-cost: does ST_AsMVT in both SELECT and HAVING double the encode?
-- ===========================================================================
-- build.rs::build_zoom_level was refactored (commit 4cc10ff) away from
--     ... GROUP BY t.x, t.y
--     HAVING count(*) >= splitSize AND ST_AsMVT(features.*, ...) IS NOT NULL
-- to a "WITH grouped AS (... ST_AsMVT once ...) ... WHERE mvt IS NOT NULL"
-- form, on the premise that the old HAVING form encoded every tile group
-- TWICE. But PostgreSQL collapses equal Aggref nodes to a single per-group
-- computation (ExecInitAgg shares one peragg across equal() aggregates) and
-- evaluates all aggregates BEFORE HAVING filters — so the premise may be false
-- and the refactor a behaviour-preserving no-op. This block settles it.
--
-- It feeds BOTH forms the exact same materialized input (_gp_ab_features) so the
-- ONLY difference is whether ST_AsMVT is referenced once (new) or twice (old).
--
-- How to read it:
--   * within noise (OLD ≈ NEW)      -> PG already deduped; single-pass is
--                                       cleanup, not a speedup. Headline refuted.
--   * OLD ≈ 2× NEW Execution Time   -> the encode really ran twice. Headline holds.
-- Each form runs twice; the first run warms cache — compare the SECOND run's
-- "Execution Time". Override the zoom with -v ab_zoom=N (default = max_zoom);
-- the splitSize cut with -v ab_split=N (default 100).
--
-- Tile assignment here is by feature centroid (one tile per feature) — a
-- deliberate simplification of the real multi-tile build. It is not a faithful
-- pyramid; it only needs to give realistic per-tile encode cost over identical
-- input for the two aggregation forms, which it does.

\echo '== A/B encode-cost: materializing per-feature MVT geom at z =' :ab_zoom '=='

-- Drop table before type: the column depends on the type, so a stale table from
-- an aborted prior run would otherwise make DROP TYPE ... CASCADE strip the
-- column and leave a half-table that CREATE TEMP TABLE then collides with.
DROP TABLE IF EXISTS _gp_ab_features;
DROP TYPE IF EXISTS _gp_ab_feat CASCADE;
CREATE TYPE _gp_ab_feat AS (geom geometry, fid bigint, kind text, rank integer);

CREATE TEMP TABLE _gp_ab_features AS
WITH g AS (
    SELECT gid, kind, rank, ST_Transform(geom, 3857) AS g3857
    FROM _gp_bench_mixed
),
binned AS (
    SELECT gid, kind, rank, g3857,
           floor((ST_X(ST_Centroid(g3857)) + 20037508.342789244)
               / (2*20037508.342789244 / pow(2, (:ab_zoom)::int)))::int AS x,
           floor((20037508.342789244 - ST_Y(ST_Centroid(g3857)))
               / (2*20037508.342789244 / pow(2, (:ab_zoom)::int)))::int AS y
    FROM g
),
mvtgeom AS (
    SELECT x, y, gid, kind, rank,
           ST_AsMVTGeom(g3857, ST_TileEnvelope((:ab_zoom)::int, x, y), 4096, 256, true) AS mg
    FROM binned
)
SELECT x, y, ROW(mg, gid::bigint, kind, rank)::_gp_ab_feat AS feat
FROM mvtgeom
WHERE mg IS NOT NULL;

CREATE INDEX _gp_ab_features_xy ON _gp_ab_features (x, y);
ANALYZE _gp_ab_features;

\echo '== A/B input: feature rows and distinct tile groups =='
SELECT count(*) AS feature_rows, count(DISTINCT (x, y)) AS tile_groups
FROM _gp_ab_features;

\echo '== OLD form (ST_AsMVT in SELECT + HAVING) — run 1/2 (warmup) =='
EXPLAIN (ANALYZE, BUFFERS, TIMING ON, COSTS OFF)
SELECT x, y, ST_AsMVT(feat, 'gp_bench', 4096, 'geom', 'fid') AS mvt
FROM _gp_ab_features
GROUP BY x, y
HAVING count(*) >= (:ab_split)::int
   AND ST_AsMVT(feat, 'gp_bench', 4096, 'geom', 'fid') IS NOT NULL;

\echo '== OLD form (ST_AsMVT in SELECT + HAVING) — run 2/2 (MEASURE THIS) =='
EXPLAIN (ANALYZE, BUFFERS, TIMING ON, COSTS OFF)
SELECT x, y, ST_AsMVT(feat, 'gp_bench', 4096, 'geom', 'fid') AS mvt
FROM _gp_ab_features
GROUP BY x, y
HAVING count(*) >= (:ab_split)::int
   AND ST_AsMVT(feat, 'gp_bench', 4096, 'geom', 'fid') IS NOT NULL;

\echo '== NEW form (ST_AsMVT once in CTE, filter outside) — run 1/2 (warmup) =='
EXPLAIN (ANALYZE, BUFFERS, TIMING ON, COSTS OFF)
WITH grouped AS (
    SELECT x, y, count(*) AS feature_count,
           ST_AsMVT(feat, 'gp_bench', 4096, 'geom', 'fid') AS mvt
    FROM _gp_ab_features
    GROUP BY x, y
)
SELECT x, y, mvt FROM grouped
WHERE feature_count >= (:ab_split)::int AND mvt IS NOT NULL;

\echo '== NEW form (ST_AsMVT once in CTE, filter outside) — run 2/2 (MEASURE THIS) =='
EXPLAIN (ANALYZE, BUFFERS, TIMING ON, COSTS OFF)
WITH grouped AS (
    SELECT x, y, count(*) AS feature_count,
           ST_AsMVT(feat, 'gp_bench', 4096, 'geom', 'fid') AS mvt
    FROM _gp_ab_features
    GROUP BY x, y
)
SELECT x, y, mvt FROM grouped
WHERE feature_count >= (:ab_split)::int AND mvt IS NOT NULL;

DROP TABLE IF EXISTS _gp_ab_features;
DROP TYPE IF EXISTS _gp_ab_feat CASCADE;
