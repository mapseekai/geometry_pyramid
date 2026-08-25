\set ON_ERROR_STOP on
\timing on

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;

\ir user_data_helpers.sql

DELETE FROM gpbench.run_results WHERE dataset = 'lucc_opt';

\echo '== LUCC optimization benchmark: z0-6, EPSG:3857 source =='

SELECT gpbench.run_lucc_config_bench(
    'lucc_opt_3857_simplify_z6',
    3857,
    'gpbench.lucc_3857'::regclass,
    6,
    jsonb_build_object(
        'strategyName', 'simplify_2_min_1',
        'simplifyPixels', 2.0,
        'minFeatureSizePixels', 1.0
    )
);

SELECT gpbench.run_lucc_config_bench(
    'lucc_opt_3857_budget_z6',
    3857,
    'gpbench.lucc_3857'::regclass,
    6,
    jsonb_build_object(
        'strategyName', 'budget_512k',
        'tileSizeBudgetBytes', 512000,
        'maxTileSizeAttempts', 5
    )
);

SELECT gpbench.run_lucc_config_bench(
    'lucc_opt_3857_sample25_z6',
    3857,
    'gpbench.lucc_3857'::regclass,
    6,
    jsonb_build_object(
        'strategyName', 'sample25_all',
        'buildRules', jsonb_build_array(
            jsonb_build_object(
                'minZoom', 0,
                'maxZoom', 6,
                'sample', jsonb_build_object('rate', 25, 'key', jsonb_build_array('ogc_fid'))
            )
        )
    )
);

SELECT gpbench.run_lucc_config_bench(
    'lucc_opt_3857_sample25_simplify_budget_z6',
    3857,
    'gpbench.lucc_3857'::regclass,
    6,
    jsonb_build_object(
        'strategyName', 'sample25_simplify_budget',
        'simplifyPixels', 2.0,
        'minFeatureSizePixels', 1.0,
        'tileSizeBudgetBytes', 512000,
        'maxTileSizeAttempts', 5,
        'buildRules', jsonb_build_array(
            jsonb_build_object(
                'minZoom', 0,
                'maxZoom', 6,
                'sample', jsonb_build_object('rate', 25, 'key', jsonb_build_array('ogc_fid'))
            )
        )
    )
);

\echo '== LUCC optimization benchmark: z0-8, EPSG:3857 source =='

SELECT gpbench.run_lucc_config_bench(
    'lucc_opt_3857_sample25_simplify_budget_z8',
    3857,
    'gpbench.lucc_3857'::regclass,
    8,
    jsonb_build_object(
        'strategyName', 'sample25_simplify_budget',
        'simplifyPixels', 2.0,
        'minFeatureSizePixels', 1.0,
        'tileSizeBudgetBytes', 512000,
        'maxTileSizeAttempts', 5,
        'buildRules', jsonb_build_array(
            jsonb_build_object(
                'minZoom', 0,
                'maxZoom', 8,
                'sample', jsonb_build_object('rate', 25, 'key', jsonb_build_array('ogc_fid'))
            )
        )
    )
);

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
WHERE dataset IN ('lucc', 'lucc_opt')
ORDER BY max_zoom, dataset, strategy;

\echo '== LUCC optimization speedups vs baseline =='
WITH base AS (
    SELECT * FROM gpbench.run_results
    WHERE dataset = 'lucc'
      AND srid = 3857
      AND strategy = 'baseline'
), opt AS (
    SELECT * FROM gpbench.run_results
    WHERE dataset = 'lucc_opt'
)
SELECT
    opt.strategy,
    opt.max_zoom,
    round((base.wall_ms / 1000.0)::numeric, 3) AS baseline_s,
    round((opt.wall_ms / 1000.0)::numeric, 3) AS optimized_s,
    round((base.wall_ms / opt.wall_ms)::numeric, 3) AS speedup,
    base.total_bytes AS baseline_bytes,
    opt.total_bytes AS optimized_bytes,
    round((base.total_bytes::numeric / NULLIF(opt.total_bytes, 0)), 3) AS byte_reduction_ratio
FROM opt
JOIN base ON base.max_zoom = opt.max_zoom
ORDER BY opt.max_zoom, opt.strategy;
