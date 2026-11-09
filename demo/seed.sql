-- geometry_pyramid demo seed: synthetic data + a pyramid showcasing both
-- dynamic boundaries (z > maxZoom level-dynamic, and splitSize density-dynamic).
CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;

SELECT ST_DropPyramid('demo');
DROP TABLE IF EXISTS demo_src CASCADE;
CREATE TABLE demo_src (gid serial PRIMARY KEY, geom geometry(Point, 4326));

-- Dense cluster near (116.49, 40.0): ~4000 points -> high density -> materialized.
INSERT INTO demo_src (geom)
SELECT ST_SetSRID(ST_MakePoint(116.39 + random() * 0.2, 39.90 + random() * 0.2), 4326)
FROM generate_series(1, 4000);

-- Sparse outliers far away: low density -> skipped by splitSize -> served dynamically.
INSERT INTO demo_src (geom)
SELECT ST_SetSRID(ST_MakePoint(x, y), 4326)
FROM (VALUES (-120, 50), (80, -45), (10, 10), (150, -30), (-60, -20), (30, 60)) AS p(x, y);

-- Idempotent rebuild. maxZoom=8 (zoom past it -> dynamic), splitSize=500
-- (dense cluster materializes, sparse outliers do not but are served on-demand).
SELECT ST_BuildPyramid('demo_src', 'geom', 'gid',
    '{"name": "demo", "maxZoom": 8, "splitSize": 500}');

-- Quick sanity: z=0 covers the world; the dense cluster makes it non-empty.
SELECT 'z0 bytes = ' || COALESCE(octet_length(ST_Tile('demo', 0, 0, 0)), 0) AS sanity;
