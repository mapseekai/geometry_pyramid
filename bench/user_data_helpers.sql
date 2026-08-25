-- Shared helpers for the user-data follow-up benchmarks.
--
-- Prerequisite: run `bench/user_data_perf.sql` once to create the gpbench
-- fixtures and `gpbench.run_results`.

CREATE OR REPLACE FUNCTION gpbench.run_lucc_config_bench(
    run_name text,
    srid integer,
    source_table regclass,
    max_zoom integer,
    config jsonb
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
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
        'ogc_fid',
        (
            jsonb_build_object(
                'name', run_name,
                'sourceSRS', srid,
                'tileSRS', 3857,
                'minZoom', 0,
                'maxZoom', max_zoom,
                'attributes', jsonb_build_array()
            ) || config
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
        'lucc_opt',
        srid,
        config->>'strategyName',
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

CREATE OR REPLACE FUNCTION gpbench.run_custom_config_bench(
    dataset_name text,
    run_name text,
    srid integer,
    strategy_name text,
    source_table regclass,
    fid_column text,
    max_zoom integer,
    config jsonb
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
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
        fid_column,
        (
            jsonb_build_object(
                'name', run_name,
                'sourceSRS', srid,
                'tileSRS', 3857,
                'minZoom', 0,
                'maxZoom', max_zoom
            ) || config
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
