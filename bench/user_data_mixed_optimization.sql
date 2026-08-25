\set ON_ERROR_STOP on
\timing on

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;

\ir user_data_helpers.sql

DELETE FROM gpbench.run_results
WHERE dataset = 'mixed_opt';

\echo '== mixed optimization benchmark: EPSG:3857 source, z0-8 =='

SELECT gpbench.run_custom_config_bench(
    'mixed_opt',
    'mixed_3857_attr_kind_z8',
    3857,
    'attr_kind',
    'gpbench.mixed_3857'::regclass,
    'gid',
    8,
    jsonb_build_object(
        'attributes', jsonb_build_array('kind', 'source_id'),
        'buildRules', jsonb_build_array(
            jsonb_build_object(
                'minZoom', 0,
                'maxZoom', 8,
                'attrFields', jsonb_build_array('kind')
            )
        )
    )
);

SELECT gpbench.run_custom_config_bench(
    'mixed_opt',
    'mixed_3857_tiered_sample_z8',
    3857,
    'tiered_sample_5_10_25',
    'gpbench.mixed_3857'::regclass,
    'gid',
    8,
    jsonb_build_object(
        'attributes', jsonb_build_array('kind', 'source_id'),
        'buildRules', jsonb_build_array(
            jsonb_build_object(
                'minZoom', 0,
                'maxZoom', 4,
                'sample', jsonb_build_object('rate', 5, 'key', jsonb_build_array('gid'))
            ),
            jsonb_build_object(
                'minZoom', 5,
                'maxZoom', 6,
                'sample', jsonb_build_object('rate', 10, 'key', jsonb_build_array('gid'))
            ),
            jsonb_build_object(
                'minZoom', 7,
                'maxZoom', 8,
                'sample', jsonb_build_object('rate', 25, 'key', jsonb_build_array('gid'))
            )
        )
    )
);

SELECT gpbench.run_custom_config_bench(
    'mixed_opt',
    'mixed_3857_tiered_sample_attr_kind_z8',
    3857,
    'tiered_sample_attr_kind',
    'gpbench.mixed_3857'::regclass,
    'gid',
    8,
    jsonb_build_object(
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

\echo '== mixed single-pyramid vs split-pyramids summary =='
WITH split AS (
    SELECT
        'split_points_line_building' AS strategy,
        sum(wall_ms) AS wall_ms,
        sum(tile_count) AS tile_count,
        sum(total_bytes) AS total_bytes,
        max(max_tile_bytes) AS max_tile_bytes
    FROM gpbench.run_results
    WHERE srid = 3857
      AND max_zoom = 8
      AND (
          (dataset = 'points' AND strategy = 'baseline')
          OR (dataset = 'line_boundary' AND strategy = 'baseline')
          OR (dataset = 'building' AND strategy = 'baseline')
      )
), mixed_base AS (
    SELECT
        'mixed_baseline' AS strategy,
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
