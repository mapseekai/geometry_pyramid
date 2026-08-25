\set ON_ERROR_STOP on
\timing on

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;

\ir user_data_helpers.sql

DELETE FROM gpbench.run_results
WHERE dataset = 'split_component'
  AND max_zoom = 8;

SELECT gpbench.run_custom_config_bench(
    'split_component',
    'split_points_3857_baseline_z8',
    3857,
    'points_baseline',
    'gpbench.points_3857'::regclass,
    'ogc_fid',
    8,
    jsonb_build_object('attributes', jsonb_build_array('id'))
);

SELECT gpbench.run_custom_config_bench(
    'split_component',
    'split_line_3857_baseline_z8',
    3857,
    'line_baseline',
    'gpbench.big_line_3857'::regclass,
    'ogc_fid',
    8,
    jsonb_build_object('attributes', jsonb_build_array())
);

SELECT gpbench.run_custom_config_bench(
    'split_component',
    'split_building_3857_baseline_z8',
    3857,
    'building_baseline',
    'gpbench.building_3857'::regclass,
    'ogc_fid',
    8,
    jsonb_build_object('attributes', jsonb_build_array())
);

WITH split AS (
    SELECT
        'split_points_line_building_z8' AS strategy,
        sum(wall_ms) AS wall_ms,
        sum(tile_count) AS tile_count,
        sum(total_bytes) AS total_bytes,
        max(max_tile_bytes) AS max_tile_bytes
    FROM gpbench.run_results
    WHERE dataset = 'split_component'
      AND max_zoom = 8
), mixed_base AS (
    SELECT
        'mixed_baseline_z8' AS strategy,
        wall_ms,
        tile_count,
        total_bytes,
        max_tile_bytes
    FROM gpbench.run_results
    WHERE dataset = 'mixed'
      AND srid = 3857
      AND strategy = 'baseline'
      AND max_zoom = 8
), mixed_opt AS (
    SELECT strategy, wall_ms, tile_count, total_bytes, max_tile_bytes
    FROM gpbench.run_results
    WHERE dataset = 'mixed_opt'
)
SELECT
    strategy,
    round((wall_ms / 1000.0)::numeric, 3) AS wall_s,
    tile_count,
    pg_size_pretty(total_bytes) AS total_mvt,
    pg_size_pretty(max_tile_bytes) AS max_tile
FROM (
    SELECT * FROM mixed_base
    UNION ALL
    SELECT * FROM mixed_opt
    UNION ALL
    SELECT * FROM split
) s
ORDER BY strategy;
