-- Run through package_smoke.sh in its disposable cluster.
\if :{?baseline_version}
\else
\set baseline_version 0.1.0
\endif
\if :seed
CREATE EXTENSION postgis;
CREATE EXTENSION geometry_pyramid VERSION :'baseline_version';
CREATE SCHEMA gp_upgrade_source;
CREATE SCHEMA gp_upgrade_tiles;
CREATE ROLE gp_upgrade_reader NOLOGIN;
SET search_path = gp_upgrade_tiles, gp_upgrade_source, public;
CREATE TABLE gp_upgrade_source.points (id int PRIMARY KEY, label text, geom geometry(Point,3857));
INSERT INTO gp_upgrade_source.points VALUES
    (1,'west',ST_Centroid(ST_TileEnvelope(1,0,0))),
    (2,'east',ST_Centroid(ST_TileEnvelope(1,1,1)));
SELECT ST_BuildPyramid('points','geom','id',
    '{"name":"gp_upgrade","sourceSRS":3857,"minZoom":1,"maxZoom":1,"attributes":["label"]}');
GRANT USAGE ON SCHEMA gp_upgrade_tiles TO gp_upgrade_reader;
GRANT SELECT ON gp_upgrade_tiles._gp_tiles_gp_upgrade TO gp_upgrade_reader;
CREATE TABLE gp_upgrade_tiles.expected AS
    SELECT 'gp_upgrade_source.points'::regclass::oid AS source_oid,
           'gp_upgrade_tiles._gp_tiles_gp_upgrade'::regclass::oid AS storage_oid,
           z,x,y,ST_Tile('gp_upgrade',z,x,y) AS mvt
    FROM gp_upgrade_tiles._gp_tiles_gp_upgrade;
SELECT geometry_pyramid_version() = :'baseline_version' AS baseline_version_matches \gset
\if :baseline_version_matches
\else
\echo 'The loaded library does not match the historical baseline'
\quit 1
\endif
DO $$ BEGIN
    IF (SELECT count(*) FROM gp_upgrade_tiles.expected WHERE octet_length(mvt)>0) <> 2 THEN
        RAISE EXCEPTION 'expected two readable tiles built with the historical package';
    END IF;
END $$;
\else
BEGIN;
ALTER EXTENSION geometry_pyramid UPDATE;
SELECT :'baseline_version' = '0.1.0' AS needs_relation_binding \gset
\if :needs_relation_binding
UPDATE public.geometry_pyramid_catalog
SET source_rel = 'gp_upgrade_source.points'::regclass,
    storage_rel = 'gp_upgrade_tiles._gp_tiles_gp_upgrade'::regclass
WHERE pyramid_name = 'gp_upgrade';
\endif
DO $$ BEGIN
    IF geometry_pyramid_version() = '0.1.0' OR
       (SELECT extversion FROM pg_extension WHERE extname='geometry_pyramid')
           IS DISTINCT FROM geometry_pyramid_version() THEN
        RAISE EXCEPTION 'candidate SQL version and loaded library must match';
    END IF;
    IF NOT EXISTS (
        SELECT FROM public.geometry_pyramid_catalog c
        JOIN gp_upgrade_tiles.expected e
          ON c.source_rel::oid=e.source_oid AND c.storage_rel::oid=e.storage_oid
        WHERE c.pyramid_name='gp_upgrade'
    ) THEN
        RAISE EXCEPTION 'upgrade did not bind the original source and storage relations';
    END IF;
    IF EXISTS (
        SELECT FROM gp_upgrade_tiles.expected e
        WHERE ST_Tile('gp_upgrade',e.z,e.x,e.y) IS DISTINCT FROM e.mvt
    ) OR (SELECT tile_count FROM ST_Pyramids() WHERE name='gp_upgrade') IS DISTINCT FROM 2 THEN
        RAISE EXCEPTION 'existing tiles or their statistics changed during upgrade';
    END IF;
END $$;
COMMIT;

SET ROLE gp_upgrade_reader;
DO $$ BEGIN
    IF (SELECT count(*) FROM gp_upgrade_tiles._gp_tiles_gp_upgrade) <> 2 THEN
        RAISE EXCEPTION 'the existing reader lost access to the two tiles';
    END IF;
END $$;
RESET ROLE;
\endif
