\set ON_ERROR_STOP on
\timing on

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;

\ir user_data_helpers.sql

CREATE TABLE IF NOT EXISTS gpbench.tile_profile_results (
    label text PRIMARY KEY,
    run_name text NOT NULL,
    source_table text NOT NULL,
    z integer NOT NULL,
    x integer NOT NULL,
    y integer NOT NULL,
    mvt_bytes bigint NOT NULL,
    source_feature_count bigint NOT NULL,
    source_total_vertices bigint NOT NULL,
    source_max_vertices integer NOT NULL,
    breakdown jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE OR REPLACE FUNCTION gpbench.profile_largest_tile(
    profile_label text,
    profile_run_name text,
    tile_table regclass,
    source_table regclass,
    kind_column text DEFAULT NULL
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    kind_expr text;
BEGIN
    kind_expr := CASE
        WHEN kind_column IS NULL THEN '''all'''
        ELSE format('COALESCE(s.%I::text, ''null'')', kind_column)
    END;

    DELETE FROM gpbench.tile_profile_results WHERE label = profile_label;

    EXECUTE format($sql$
        INSERT INTO gpbench.tile_profile_results (
            label, run_name, source_table, z, x, y, mvt_bytes,
            source_feature_count, source_total_vertices, source_max_vertices,
            breakdown
        )
        WITH largest AS (
            SELECT z::integer AS z, x::integer AS x, y::integer AS y,
                   octet_length(mvt)::bigint AS mvt_bytes
            FROM %s
            ORDER BY octet_length(mvt) DESC
            LIMIT 1
        ), env AS (
            SELECT *, ST_TileEnvelope(z, x, y) AS geom
            FROM largest
        ), matches AS (
            SELECT
                %s AS kind,
                GeometryType(s.geom) AS geom_type,
                ST_NPoints(s.geom)::integer AS npoints
            FROM %s s
            JOIN env e ON s.geom && e.geom AND ST_Intersects(s.geom, e.geom)
            WHERE s.geom IS NOT NULL
        ), summary AS (
            SELECT
                count(*)::bigint AS feature_count,
                COALESCE(sum(npoints), 0)::bigint AS total_vertices,
                COALESCE(max(npoints), 0)::integer AS max_vertices
            FROM matches
        ), grouped AS (
            SELECT kind, geom_type, count(*)::bigint AS feature_count,
                   COALESCE(sum(npoints), 0)::bigint AS total_vertices
            FROM matches
            GROUP BY kind, geom_type
        ), breakdown AS (
            SELECT COALESCE(
                jsonb_agg(
                    jsonb_build_object(
                        'kind', kind,
                        'geometry_type', geom_type,
                        'feature_count', feature_count,
                        'total_vertices', total_vertices
                    )
                    ORDER BY kind, geom_type
                ),
                '[]'::jsonb
            ) AS value
            FROM grouped
        )
        SELECT
            %L AS label,
            %L AS run_name,
            %L AS source_table,
            e.z, e.x, e.y, e.mvt_bytes,
            s.feature_count, s.total_vertices, s.max_vertices,
            b.value
        FROM env e
        CROSS JOIN summary s
        CROSS JOIN breakdown b
    $sql$, tile_table, kind_expr, source_table, profile_label, profile_run_name, source_table::text);
END;
$$;

\echo '== profile largest generated tiles =='
SELECT gpbench.profile_largest_tile(
    'points_baseline_z10_largest',
    'points_3857_baseline_z10',
    'gpbench._gp_tiles_points_3857_baseline_z10'::regclass,
    'gpbench.points_3857'::regclass
);

SELECT gpbench.profile_largest_tile(
    'points_tiered_1_5_25_z10_largest',
    'points_3857_tiered_1_5_25_z10',
    'gpbench._gp_tiles_points_3857_tiered_1_5_25_z10'::regclass,
    'gpbench.points_3857'::regclass
);

SELECT gpbench.profile_largest_tile(
    'mixed_baseline_z8_largest',
    'mixed_3857_baseline_z8',
    'gpbench._gp_tiles_mixed_3857_baseline_z8'::regclass,
    'gpbench.mixed_3857'::regclass,
    'kind'
);

SELECT gpbench.profile_largest_tile(
    'mixed_tiered_attr_kind_z8_largest',
    'mixed_3857_tiered_sample_attr_kind_z8',
    'gpbench._gp_tiles_mixed_3857_tiered_sample_attr_kind_z8'::regclass,
    'gpbench.mixed_3857'::regclass,
    'kind'
);

SELECT gpbench.profile_largest_tile(
    'lucc_baseline_z6_largest',
    'lucc_3857_baseline_z6',
    'gpbench._gp_tiles_lucc_3857_baseline_z6'::regclass,
    'gpbench.lucc_3857'::regclass
);

SELECT gpbench.profile_largest_tile(
    'lucc_sample10_z8_largest',
    'lucc_opt_3857_sample10_z8',
    'gpbench._gp_tiles_lucc_opt_3857_sample10_z8'::regclass,
    'gpbench.lucc_3857'::regclass
);

TABLE gpbench.tile_profile_results
ORDER BY mvt_bytes DESC;

\echo '== parallel=4 benchmarks =='
DELETE FROM gpbench.run_results
WHERE dataset = 'parallel_probe';

SELECT gpbench.run_custom_config_bench(
    'parallel_probe',
    'lucc_3857_sample10_z8_parallel4',
    3857,
    'lucc_sample10_parallel4',
    'gpbench.lucc_3857'::regclass,
    'ogc_fid',
    8,
    jsonb_build_object(
        'parallel', 4,
        'buildTimeoutMs', 900000,
        'attributes', jsonb_build_array(),
        'buildRules', jsonb_build_array(
            jsonb_build_object(
                'minZoom', 0,
                'maxZoom', 8,
                'sample', jsonb_build_object('rate', 10, 'key', jsonb_build_array('ogc_fid'))
            )
        )
    )
);

SELECT gpbench.run_custom_config_bench(
    'parallel_probe',
    'mixed_3857_baseline_z8_parallel4',
    3857,
    'mixed_baseline_parallel4',
    'gpbench.mixed_3857'::regclass,
    'gid',
    8,
    jsonb_build_object(
        'parallel', 4,
        'buildTimeoutMs', 900000,
        'attributes', jsonb_build_array('kind', 'source_id')
    )
);

SELECT gpbench.run_custom_config_bench(
    'parallel_probe',
    'mixed_3857_tiered_attr_kind_z8_parallel4',
    3857,
    'mixed_tiered_attr_kind_parallel4',
    'gpbench.mixed_3857'::regclass,
    'gid',
    8,
    jsonb_build_object(
        'parallel', 4,
        'buildTimeoutMs', 900000,
        'attributes', jsonb_build_array('kind', 'source_id'),
        'buildRules', jsonb_build_array(
            jsonb_build_object(
                'minZoom', 0,
                'maxZoom', 4,
                'sample', jsonb_build_object('rate', 5, 'key', jsonb_build_array('gid')),
                'attrFields', jsonb_build_array('kind')
            ),
            jsonb_build_object(
                'minZoom', 5,
                'maxZoom', 6,
                'sample', jsonb_build_object('rate', 10, 'key', jsonb_build_array('gid')),
                'attrFields', jsonb_build_array('kind')
            ),
            jsonb_build_object(
                'minZoom', 7,
                'maxZoom', 8,
                'sample', jsonb_build_object('rate', 25, 'key', jsonb_build_array('gid')),
                'attrFields', jsonb_build_array('kind')
            )
        )
    )
);

\echo '== parallel summary =='
SELECT
    strategy,
    round((wall_ms / 1000.0)::numeric, 3) AS wall_s,
    tile_count,
    pg_size_pretty(total_bytes) AS total_mvt,
    pg_size_pretty(max_tile_bytes) AS max_tile
FROM gpbench.run_results
WHERE dataset = 'parallel_probe'
ORDER BY strategy;
