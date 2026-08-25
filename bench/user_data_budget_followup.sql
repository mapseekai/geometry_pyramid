\set ON_ERROR_STOP on
\timing on

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;

\ir user_data_helpers.sql

DELETE FROM gpbench.run_results
WHERE dataset = 'mixed_budget'
  AND strategy = 'budget_4mb';

SELECT gpbench.run_custom_config_bench(
    'mixed_budget',
    'mixed_3857_budget_4mb_z8',
    3857,
    'budget_4mb',
    'gpbench.mixed_3857'::regclass,
    'gid',
    8,
    jsonb_build_object(
        'attributes', jsonb_build_array('kind', 'source_id'),
        'tileSizeBudgetBytes', 4194304,
        'maxTileSizeAttempts', 2
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
WHERE dataset IN ('mixed', 'mixed_budget')
ORDER BY dataset, strategy;
