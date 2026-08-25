\set ON_ERROR_STOP on
\timing on

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;

SELECT gpbench.run_pyramid_bench('lucc', 4326, 'baseline', 'gpbench.lucc_4326'::regclass, 6, false, false, false);
SELECT gpbench.run_pyramid_bench('lucc', 4326, 'prepared', 'gpbench.lucc_4326'::regclass, 6, true, false, false);
SELECT gpbench.run_pyramid_bench('lucc', 3857, 'baseline', 'gpbench.lucc_3857'::regclass, 6, false, false, false);
SELECT gpbench.run_pyramid_bench('lucc', 3857, 'prepared', 'gpbench.lucc_3857'::regclass, 6, true, false, false);
SELECT gpbench.run_pyramid_bench('lucc', 4490, 'baseline', 'gpbench.lucc_4490'::regclass, 6, false, false, false);
SELECT gpbench.run_pyramid_bench('lucc', 4490, 'prepared', 'gpbench.lucc_4490'::regclass, 6, true, false, false);

SELECT gpbench.run_pyramid_bench('mixed', 4326, 'baseline', 'gpbench.mixed_4326'::regclass, 8, false, false, false);
SELECT gpbench.run_pyramid_bench('mixed', 4326, 'prepared', 'gpbench.mixed_4326'::regclass, 8, true, false, false);
SELECT gpbench.run_pyramid_bench('mixed', 3857, 'baseline', 'gpbench.mixed_3857'::regclass, 8, false, false, false);
SELECT gpbench.run_pyramid_bench('mixed', 3857, 'prepared', 'gpbench.mixed_3857'::regclass, 8, true, false, false);
SELECT gpbench.run_pyramid_bench('mixed', 4490, 'baseline', 'gpbench.mixed_4490'::regclass, 8, false, false, false);
SELECT gpbench.run_pyramid_bench('mixed', 4490, 'prepared', 'gpbench.mixed_4490'::regclass, 8, true, false, false);

SELECT
    dataset,
    srid,
    strategy,
    max_zoom,
    source_rows,
    round(wall_ms::numeric, 1) AS wall_ms,
    round((wall_ms / 1000.0)::numeric, 3) AS wall_s,
    tile_count,
    pg_size_pretty(total_bytes) AS total_mvt,
    pg_size_pretty(max_tile_bytes) AS max_tile
FROM gpbench.run_results
WHERE (dataset = 'lucc' AND max_zoom IN (6, 8))
   OR (dataset = 'mixed' AND max_zoom = 8)
ORDER BY dataset, max_zoom, srid, strategy;
