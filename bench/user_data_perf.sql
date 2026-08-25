\set ON_ERROR_STOP on
\timing on

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;

CREATE SCHEMA IF NOT EXISTS gpbench;

DROP TABLE IF EXISTS gpbench.run_results;
CREATE TABLE gpbench.run_results (
    run_name text PRIMARY KEY,
    dataset text NOT NULL,
    srid integer NOT NULL,
    strategy text NOT NULL,
    source_table text NOT NULL,
    source_rows bigint NOT NULL,
    max_zoom integer NOT NULL,
    started_at timestamptz NOT NULL,
    finished_at timestamptz NOT NULL,
    wall_ms double precision NOT NULL,
    tile_count bigint,
    total_bytes bigint,
    max_tile_bytes bigint
);

CREATE OR REPLACE FUNCTION gpbench.ensure_bench_tables(srid integer)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    suffix text := srid::text;
BEGIN
    EXECUTE format('DROP TABLE IF EXISTS gpbench.big_line_%s', suffix);
    EXECUTE format(
        'CREATE TABLE gpbench.big_line_%1$s AS
         SELECT ogc_fid, fid, height, ST_Multi(ST_Boundary(geom))::geometry(MultiLineString,%1$s) AS geom
         FROM gpbench.big_poly_%1$s
         WHERE geom IS NOT NULL',
        suffix
    );
    EXECUTE format('CREATE INDEX big_line_%1$s_geom_geom_idx ON gpbench.big_line_%1$s USING gist (geom)', suffix);

    EXECUTE format('DROP TABLE IF EXISTS gpbench.mixed_%s', suffix);
    EXECUTE format(
        'CREATE TABLE gpbench.mixed_%1$s (
             gid bigserial PRIMARY KEY,
             kind text NOT NULL,
             source_id bigint,
             geom geometry(Geometry,%1$s) NOT NULL
         )',
        suffix
    );
    EXECUTE format('INSERT INTO gpbench.mixed_%1$s (kind, source_id, geom)
                    SELECT ''point'', ogc_fid, geom FROM gpbench.points_%1$s', suffix);
    EXECUTE format('INSERT INTO gpbench.mixed_%1$s (kind, source_id, geom)
                    SELECT ''line'', ogc_fid, geom FROM gpbench.big_line_%1$s', suffix);
    EXECUTE format('INSERT INTO gpbench.mixed_%1$s (kind, source_id, geom)
                    SELECT ''building'', ogc_fid, geom FROM gpbench.building_%1$s', suffix);
    EXECUTE format('CREATE INDEX mixed_%1$s_geom_geom_idx ON gpbench.mixed_%1$s USING gist (geom)', suffix);
END;
$$;

CREATE OR REPLACE FUNCTION gpbench.run_pyramid_bench(
    dataset_name text,
    srid integer,
    strategy_name text,
    source_table regclass,
    max_zoom integer DEFAULT 10,
    prepared_build boolean DEFAULT false,
    feature_major_points boolean DEFAULT false,
    feature_driven_enum boolean DEFAULT false
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    run_name text := format('%s_%s_%s_z%s', dataset_name, srid, strategy_name, max_zoom);
    started timestamptz;
    finished timestamptz;
    rows_count bigint;
    source_name text;
BEGIN
    SELECT c.relname
    INTO source_name
    FROM pg_class c
    WHERE c.oid = source_table::oid;

    PERFORM set_config('search_path', 'gpbench,public', true);
    EXECUTE format('SELECT count(*) FROM %s', source_table) INTO rows_count;

    PERFORM ST_DropPyramid(run_name);
    started := clock_timestamp();

    PERFORM ST_BuildPyramid(
        source_name,
        'geom',
        CASE WHEN dataset_name = 'mixed' THEN 'gid' ELSE 'ogc_fid' END,
        jsonb_build_object(
            'name', run_name,
            'sourceSRS', srid,
            'tileSRS', 3857,
            'minZoom', 0,
            'maxZoom', max_zoom,
            'preparedBuild', prepared_build,
            'featureMajorPoints', feature_major_points,
            'featureMajorMinZoom', 0,
            'featureDrivenEnum', feature_driven_enum,
            'attributes', CASE
                WHEN dataset_name = 'points' THEN jsonb_build_array('id')
                WHEN dataset_name = 'mixed' THEN jsonb_build_array('kind', 'source_id')
                ELSE jsonb_build_array()
            END
        )::text
    );

    finished := clock_timestamp();

    INSERT INTO gpbench.run_results (
        run_name, dataset, srid, strategy, source_table, source_rows,
        max_zoom, started_at, finished_at, wall_ms, tile_count, total_bytes,
        max_tile_bytes
    )
    SELECT
        run_name,
        dataset_name,
        srid,
        strategy_name,
        source_name,
        rows_count,
        max_zoom,
        started,
        finished,
        EXTRACT(EPOCH FROM finished - started) * 1000.0,
        COALESCE(sum(tile_count), 0)::bigint,
        COALESCE(sum(total_bytes), 0)::bigint,
        COALESCE(max(max_tile_bytes), 0)::bigint
    FROM geometry_pyramid_stats
    WHERE pyramid_name = run_name;
END;
$$;

SELECT gpbench.ensure_bench_tables(4326);
SELECT gpbench.ensure_bench_tables(3857);
SELECT gpbench.ensure_bench_tables(4490);

ANALYZE gpbench.points_4326;
ANALYZE gpbench.points_3857;
ANALYZE gpbench.points_4490;
ANALYZE gpbench.building_4326;
ANALYZE gpbench.building_3857;
ANALYZE gpbench.building_4490;
ANALYZE gpbench.lucc_4326;
ANALYZE gpbench.lucc_3857;
ANALYZE gpbench.lucc_4490;
ANALYZE gpbench.big_poly_4326;
ANALYZE gpbench.big_poly_3857;
ANALYZE gpbench.big_poly_4490;
ANALYZE gpbench.big_line_4326;
ANALYZE gpbench.big_line_3857;
ANALYZE gpbench.big_line_4490;
ANALYZE gpbench.mixed_4326;
ANALYZE gpbench.mixed_3857;
ANALYZE gpbench.mixed_4490;

\echo '== source inventory =='
SELECT
    n.nspname AS schema_name,
    c.relname AS table_name,
    pg_stat_get_live_tuples(c.oid) AS estimated_rows
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'gpbench'
  AND c.relkind = 'r'
  AND c.relname !~ '^(run_results|spatial_ref_sys)$'
ORDER BY c.relname;

\echo '== benchmark: maxZoom 10 =='

SELECT gpbench.run_pyramid_bench('points', 4326, 'baseline', 'gpbench.points_4326'::regclass, 10, false, false, false);
SELECT gpbench.run_pyramid_bench('points', 4326, 'prepared', 'gpbench.points_4326'::regclass, 10, true, false, false);
SELECT gpbench.run_pyramid_bench('points', 4326, 'feature_major', 'gpbench.points_4326'::regclass, 10, true, true, false);

SELECT gpbench.run_pyramid_bench('points', 3857, 'baseline', 'gpbench.points_3857'::regclass, 10, false, false, false);
SELECT gpbench.run_pyramid_bench('points', 3857, 'prepared', 'gpbench.points_3857'::regclass, 10, true, false, false);
SELECT gpbench.run_pyramid_bench('points', 3857, 'feature_major', 'gpbench.points_3857'::regclass, 10, true, true, false);

SELECT gpbench.run_pyramid_bench('points', 4490, 'baseline', 'gpbench.points_4490'::regclass, 10, false, false, false);
SELECT gpbench.run_pyramid_bench('points', 4490, 'prepared', 'gpbench.points_4490'::regclass, 10, true, false, false);
SELECT gpbench.run_pyramid_bench('points', 4490, 'feature_major', 'gpbench.points_4490'::regclass, 10, true, true, false);

SELECT gpbench.run_pyramid_bench('line_boundary', 4326, 'baseline', 'gpbench.big_line_4326'::regclass, 10, false, false, false);
SELECT gpbench.run_pyramid_bench('line_boundary', 4326, 'prepared', 'gpbench.big_line_4326'::regclass, 10, true, false, false);

SELECT gpbench.run_pyramid_bench('line_boundary', 3857, 'baseline', 'gpbench.big_line_3857'::regclass, 10, false, false, false);
SELECT gpbench.run_pyramid_bench('line_boundary', 3857, 'prepared', 'gpbench.big_line_3857'::regclass, 10, true, false, false);

SELECT gpbench.run_pyramid_bench('line_boundary', 4490, 'baseline', 'gpbench.big_line_4490'::regclass, 10, false, false, false);
SELECT gpbench.run_pyramid_bench('line_boundary', 4490, 'prepared', 'gpbench.big_line_4490'::regclass, 10, true, false, false);

SELECT gpbench.run_pyramid_bench('building', 4326, 'baseline', 'gpbench.building_4326'::regclass, 10, false, false, false);
SELECT gpbench.run_pyramid_bench('building', 4326, 'prepared', 'gpbench.building_4326'::regclass, 10, true, false, false);

SELECT gpbench.run_pyramid_bench('building', 3857, 'baseline', 'gpbench.building_3857'::regclass, 10, false, false, false);
SELECT gpbench.run_pyramid_bench('building', 3857, 'prepared', 'gpbench.building_3857'::regclass, 10, true, false, false);

SELECT gpbench.run_pyramid_bench('building', 4490, 'baseline', 'gpbench.building_4490'::regclass, 10, false, false, false);
SELECT gpbench.run_pyramid_bench('building', 4490, 'prepared', 'gpbench.building_4490'::regclass, 10, true, false, false);

\echo '== summary =='
SELECT
    dataset,
    srid,
    strategy,
    source_rows,
    round(wall_ms::numeric, 1) AS wall_ms,
    round((wall_ms / 1000.0)::numeric, 3) AS wall_s,
    tile_count,
    pg_size_pretty(total_bytes) AS total_mvt,
    pg_size_pretty(max_tile_bytes) AS max_tile
FROM gpbench.run_results
ORDER BY dataset, srid, strategy;

\echo '== per-zoom stats =='
SELECT
    r.dataset,
    r.srid,
    r.strategy,
    s.z,
    s.tile_count,
    s.total_bytes,
    s.max_tile_bytes,
    s.build_ms,
    s.attempts
FROM gpbench.run_results r
JOIN geometry_pyramid_stats s ON s.pyramid_name = r.run_name
ORDER BY r.dataset, r.srid, r.strategy, s.z;
