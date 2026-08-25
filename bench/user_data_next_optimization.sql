\set ON_ERROR_STOP on
\timing on

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;

\ir user_data_helpers.sql

DELETE FROM gpbench.run_results
WHERE dataset IN ('lucc_subdiv', 'lucc_opt_tiered', 'mixed_budget', 'points_budget');

\echo '== prepare LUCC subdivided source =='
DROP TABLE IF EXISTS gpbench.lucc_subdiv_3857;
CREATE TABLE gpbench.lucc_subdiv_3857 AS
SELECT
    row_number() OVER ()::bigint AS gid,
    ogc_fid,
    (dumped).geom::geometry(Polygon, 3857) AS geom
FROM (
    SELECT ogc_fid, ST_Dump(ST_Subdivide(geom, 2048)) AS dumped
    FROM gpbench.lucc_3857
    WHERE geom IS NOT NULL
) s;
CREATE INDEX lucc_subdiv_3857_geom_idx ON gpbench.lucc_subdiv_3857 USING gist (geom);
ANALYZE gpbench.lucc_subdiv_3857;

\echo '== LUCC subdivide and tiered sampling =='
SELECT gpbench.run_custom_config_bench(
    'lucc_subdiv',
    'lucc_subdiv_3857_baseline_z6',
    3857,
    'subdivide_2048_baseline',
    'gpbench.lucc_subdiv_3857'::regclass,
    'gid',
    6,
    jsonb_build_object('attributes', jsonb_build_array())
);

SELECT gpbench.run_custom_config_bench(
    'lucc_subdiv',
    'lucc_subdiv_3857_sample10_z6',
    3857,
    'subdivide_2048_sample10',
    'gpbench.lucc_subdiv_3857'::regclass,
    'gid',
    6,
    jsonb_build_object(
        'attributes', jsonb_build_array(),
        'buildRules', jsonb_build_array(
            jsonb_build_object(
                'minZoom', 0,
                'maxZoom', 6,
                'sample', jsonb_build_object('rate', 10, 'key', jsonb_build_array('gid'))
            )
        )
    )
);

SELECT gpbench.run_lucc_config_bench(
    'lucc_opt_3857_tiered_5_10_25_z8',
    3857,
    'gpbench.lucc_3857'::regclass,
    8,
    jsonb_build_object(
        'strategyName', 'tiered_5_10_25',
        'buildRules', jsonb_build_array(
            jsonb_build_object(
                'minZoom', 0,
                'maxZoom', 4,
                'sample', jsonb_build_object('rate', 5, 'key', jsonb_build_array('ogc_fid'))
            ),
            jsonb_build_object(
                'minZoom', 5,
                'maxZoom', 6,
                'sample', jsonb_build_object('rate', 10, 'key', jsonb_build_array('ogc_fid'))
            ),
            jsonb_build_object(
                'minZoom', 7,
                'maxZoom', 8,
                'sample', jsonb_build_object('rate', 25, 'key', jsonb_build_array('ogc_fid'))
            )
        )
    )
);

\echo '== 1MB tile budget for points and mixed =='
SELECT gpbench.run_custom_config_bench(
    'points_budget',
    'points_3857_budget_1mb_z10',
    3857,
    'budget_1mb',
    'gpbench.points_3857'::regclass,
    'ogc_fid',
    10,
    jsonb_build_object(
        'attributes', jsonb_build_array('id'),
        'tileSizeBudgetBytes', 1048576,
        'maxTileSizeAttempts', 3
    )
);

SELECT gpbench.run_custom_config_bench(
    'mixed_budget',
    'mixed_3857_budget_1mb_z8',
    3857,
    'budget_1mb',
    'gpbench.mixed_3857'::regclass,
    'gid',
    8,
    jsonb_build_object(
        'attributes', jsonb_build_array('kind', 'source_id'),
        'tileSizeBudgetBytes', 1048576,
        'maxTileSizeAttempts', 3
    )
);

\echo '== next optimization summary =='
SELECT
    dataset,
    srid,
    strategy,
    max_zoom,
    source_rows,
    round((wall_ms / 1000.0)::numeric, 3) AS wall_s,
    tile_count,
    pg_size_pretty(total_bytes) AS total_mvt,
    pg_size_pretty(max_tile_bytes) AS max_tile
FROM gpbench.run_results
WHERE dataset IN ('lucc', 'lucc_opt', 'lucc_subdiv', 'lucc_opt_tiered', 'mixed', 'mixed_budget', 'points', 'points_budget')
ORDER BY dataset, max_zoom, strategy;
