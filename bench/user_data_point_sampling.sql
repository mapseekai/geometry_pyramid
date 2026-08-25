\set ON_ERROR_STOP on
\timing on

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;

\ir user_data_helpers.sql

DELETE FROM gpbench.run_results
WHERE dataset IN ('points_opt', 'mixed_point_opt');

\echo '== points tiered sampling benchmark: EPSG:3857 source, z0-10 =='

SELECT gpbench.run_custom_config_bench(
    'points_opt',
    'points_3857_tiered_1_5_25_z10',
    3857,
    'tiered_1_5_25',
    'gpbench.points_3857'::regclass,
    'ogc_fid',
    10,
    jsonb_build_object(
        'attributes', jsonb_build_array('id'),
        'buildRules', jsonb_build_array(
            jsonb_build_object(
                'minZoom', 0,
                'maxZoom', 6,
                'sample', jsonb_build_object('rate', 1, 'key', jsonb_build_array('ogc_fid'), 'geometryTypes', jsonb_build_array('POINT'))
            ),
            jsonb_build_object(
                'minZoom', 7,
                'maxZoom', 8,
                'sample', jsonb_build_object('rate', 5, 'key', jsonb_build_array('ogc_fid'), 'geometryTypes', jsonb_build_array('POINT'))
            ),
            jsonb_build_object(
                'minZoom', 9,
                'maxZoom', 10,
                'sample', jsonb_build_object('rate', 25, 'key', jsonb_build_array('ogc_fid'), 'geometryTypes', jsonb_build_array('POINT'))
            )
        )
    )
);

SELECT gpbench.run_custom_config_bench(
    'points_opt',
    'points_3857_tiered_5_10_25_z10',
    3857,
    'tiered_5_10_25',
    'gpbench.points_3857'::regclass,
    'ogc_fid',
    10,
    jsonb_build_object(
        'attributes', jsonb_build_array('id'),
        'buildRules', jsonb_build_array(
            jsonb_build_object(
                'minZoom', 0,
                'maxZoom', 6,
                'sample', jsonb_build_object('rate', 5, 'key', jsonb_build_array('ogc_fid'), 'geometryTypes', jsonb_build_array('POINT'))
            ),
            jsonb_build_object(
                'minZoom', 7,
                'maxZoom', 8,
                'sample', jsonb_build_object('rate', 10, 'key', jsonb_build_array('ogc_fid'), 'geometryTypes', jsonb_build_array('POINT'))
            ),
            jsonb_build_object(
                'minZoom', 9,
                'maxZoom', 10,
                'sample', jsonb_build_object('rate', 25, 'key', jsonb_build_array('ogc_fid'), 'geometryTypes', jsonb_build_array('POINT'))
            )
        )
    )
);

\echo '== mixed point-only sampling benchmark: EPSG:3857 source, z0-8 =='

SELECT gpbench.run_custom_config_bench(
    'mixed_point_opt',
    'mixed_3857_point_tiered_attr_kind_z8',
    3857,
    'point_tiered_attr_kind',
    'gpbench.mixed_3857'::regclass,
    'gid',
    8,
    jsonb_build_object(
        'attributes', jsonb_build_array('kind', 'source_id'),
        'buildRules', jsonb_build_array(
            jsonb_build_object(
                'minZoom', 0,
                'maxZoom', 4,
                'sample', jsonb_build_object('rate', 5, 'key', jsonb_build_array('gid'), 'geometryTypes', jsonb_build_array('POINT')),
                'attrFields', jsonb_build_array('kind')
            ),
            jsonb_build_object(
                'minZoom', 5,
                'maxZoom', 6,
                'sample', jsonb_build_object('rate', 10, 'key', jsonb_build_array('gid'), 'geometryTypes', jsonb_build_array('POINT')),
                'attrFields', jsonb_build_array('kind')
            ),
            jsonb_build_object(
                'minZoom', 7,
                'maxZoom', 8,
                'sample', jsonb_build_object('rate', 25, 'key', jsonb_build_array('gid'), 'geometryTypes', jsonb_build_array('POINT')),
                'attrFields', jsonb_build_array('kind')
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
WHERE dataset IN ('points', 'points_opt', 'mixed', 'mixed_opt', 'mixed_point_opt')
  AND srid = 3857
ORDER BY dataset, strategy;
