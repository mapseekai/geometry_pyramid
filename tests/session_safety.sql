-- Run with psql -X -v ON_ERROR_STOP=1 in a disposable, trust-authenticated PG17
-- database with the extension installed. These tests require real connections.
SELECT current_user AS admin \gset
CREATE ROLE gp_gate_login LOGIN;
CREATE ROLE gp_gate_group NOLOGIN;
GRANT gp_gate_group TO gp_gate_login;
CREATE SCHEMA gp_gate;
GRANT USAGE, CREATE ON SCHEMA gp_gate TO gp_gate_group;
GRANT SELECT, INSERT, UPDATE, DELETE ON public.geometry_pyramid_catalog, public.geometry_pyramid_stats TO gp_gate_group;
SET search_path = gp_gate, public;
CREATE TABLE tenants (id int, tenant name, geom geometry(Point,3857));
INSERT INTO tenants VALUES
  (1,'gp_gate_login',ST_Centroid(ST_TileEnvelope(4,0,0))),
  (2,'gp_gate_login',ST_Centroid(ST_TileEnvelope(4,15,15))),
  (99,'gp_gate_group',ST_Centroid(ST_TileEnvelope(4,4,5)));
-- Enough visible data to cross the parallel work threshold.
INSERT INTO tenants SELECT 1000+i,'gp_gate_login',ST_Centroid(ST_TileEnvelope(4,0,0))
  FROM generate_series(1,400)i;
CREATE TABLE tenant_setting AS SELECT * FROM tenants;
ALTER TABLE tenant_setting ENABLE ROW LEVEL SECURITY;
CREATE POLICY by_setting ON tenant_setting USING
  (tenant=coalesce(nullif(current_setting('gp.tenant',true),''),'gp_gate_group')::name);
GRANT SELECT, UPDATE ON tenant_setting TO gp_gate_group;
ALTER TABLE tenants ENABLE ROW LEVEL SECURITY;
CREATE POLICY visible ON tenants USING (tenant = session_user);
GRANT SELECT, UPDATE ON tenants TO gp_gate_group;

\connect - gp_gate_login
SET ROLE gp_gate_group;
SET search_path = gp_gate, public;
SELECT ST_BuildPyramid('tenants','geom','id','{"name":"gate_rls","sourceSRS":3857,"minZoom":7,"maxZoom":7,"parallel":4,"preparedBuild":true}');
DO $$ BEGIN
    IF (SELECT count(*) FROM tenants) <> 402 OR
       ST_Tile('gate_rls',7,36,44) IS NOT NULL OR ST_Tile('gate_rls',7,4,4) IS NULL OR
       ST_Tile('gate_rls',7,124,124) IS NULL THEN RAISE EXCEPTION 'worker changed tenant visibility'; END IF;
END $$;
SELECT ST_DropPyramid('gate_rls');

RESET ROLE;
SET gp.tenant = 'gp_gate_login';
SELECT ST_BuildPyramid('tenant_setting','geom','id','{"name":"gate_setting","sourceSRS":3857,"minZoom":7,"maxZoom":7,"parallel":4,"preparedBuild":true}');
DO $$ BEGIN
    IF ST_Tile('gate_setting',7,36,44) IS NOT NULL OR ST_Tile('gate_setting',7,4,4) IS NULL
       THEN RAISE EXCEPTION 'worker lost session tenant setting'; END IF;
END $$;
SELECT ST_DropPyramid('gate_setting');

\connect - :admin
SET search_path = gp_gate, public;
CREATE TABLE points AS SELECT x AS id, ST_Centroid(ST_TileEnvelope(4,x,x)) AS geom
  FROM (VALUES (0),(15)) AS s(x);
-- A complex feature makes this a real worker candidate even in a modest rectangle.
INSERT INTO points VALUES (50, ST_Buffer(ST_Centroid(ST_TileEnvelope(4,0,0)),1000,4096));
BEGIN;
INSERT INTO points VALUES (3,ST_Centroid(ST_TileEnvelope(4,3,7)));
SELECT ST_BuildPyramid('points','geom','id','{"name":"gate_tx","sourceSRS":3857,"minZoom":4,"maxZoom":4,"parallel":2}');
DO $$ BEGIN
    IF ST_Tile('gate_tx',4,3,7) IS NULL THEN RAISE EXCEPTION 'own write disappeared'; END IF;
END $$;
COMMIT;
GRANT SELECT ON _gp_tiles_gate_tx TO gp_gate_group;
-- Reject the final publication once. Independently committed scratch rows then
-- expose their actual writer XIDs, and retry must reclaim the same stage safely.
INSERT INTO points SELECT 100+i,ST_Centroid(ST_TileEnvelope(4,i%16,i%16)) FROM generate_series(1,1000)i;
-- GDAL and other clients install sql_drop hooks. All destructive DDL,
-- including recovery, must stay on the originating connection.
SET gp.gate_marker = 'caller';
CREATE FUNCTION check_ddl_session() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN IF current_setting('gp.gate_marker',true) IS DISTINCT FROM 'caller' THEN
    RAISE EXCEPTION 'worker ran a caller-dependent DDL hook'; END IF; END $$;
CREATE EVENT TRIGGER gp_gate_drop ON sql_drop EXECUTE FUNCTION check_ddl_session();
CREATE TABLE _gp_shadow_gate_tx (business_data int);
INSERT INTO _gp_shadow_gate_tx VALUES (73);
\set ON_ERROR_STOP off
SELECT ST_UpdatePyramid('gate_tx','');
\set failure_state :SQLSTATE
\set ON_ERROR_STOP on
SELECT 1 / (:'failure_state' <> '00000')::int AS rejected_foreign_stage;
SELECT 1 / ((SELECT business_data FROM _gp_shadow_gate_tx)=73)::int AS sentinel_preserved;
DROP TABLE _gp_shadow_gate_tx;
ALTER TABLE _gp_tiles_gate_tx ADD CONSTRAINT fail_publish CHECK (z<>4) NOT VALID;
\set ON_ERROR_STOP off
SELECT ST_BuildPyramid('points','geom','id','{"name":"gate_tx","sourceSRS":3857,"minZoom":4,"maxZoom":4,"parallel":4}');
\set failure_state :SQLSTATE
\set ON_ERROR_STOP on
SELECT 1 / (:'failure_state' = '23514')::int AS expected_publication_failure;
DO $$ BEGIN
    IF (SELECT count(DISTINCT xmin::text) FROM ONLY _gp_shadow_gate_tx)<2
       THEN RAISE EXCEPTION 'workers did not contribute independent transactions'; END IF;
    IF (SELECT count(*) FROM ONLY _gp_tiles_gate_tx)<>3 THEN RAISE EXCEPTION 'previous publication was lost'; END IF;
END $$;
ALTER TABLE _gp_tiles_gate_tx DROP CONSTRAINT fail_publish;
SELECT ST_BuildPyramid('points','geom','id','{"name":"gate_tx","sourceSRS":3857,"minZoom":4,"maxZoom":4,"parallel":4}');
DO $$ BEGIN
    IF to_regclass('gp_gate._gp_shadow_gate_tx') IS NOT NULL THEN RAISE EXCEPTION 'stage was left behind'; END IF;
    IF NOT has_table_privilege('gp_gate_group','gp_gate._gp_tiles_gate_tx','SELECT') THEN RAISE EXCEPTION 'reader lost grant'; END IF;
END $$;

-- DDL hooks must also run with the caller's session settings.
DROP EVENT TRIGGER gp_gate_drop;
CREATE EVENT TRIGGER gp_gate_ddl ON ddl_command_end WHEN TAG IN ('CREATE TABLE','CREATE TABLE AS')
  EXECUTE FUNCTION check_ddl_session();
SELECT ST_BuildPyramid('points','geom','id','{"name":"gate_tx","sourceSRS":3857,"minZoom":4,"maxZoom":4,"parallel":4,"preparedBuild":true}');
DROP EVENT TRIGGER gp_gate_ddl;

-- An output trigger can write source rows in the caller's transaction.
CREATE FUNCTION add_point() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN INSERT INTO gp_gate.points VALUES(51,ST_Centroid(ST_TileEnvelope(4,3,4))); RETURN NULL; END $$;
CREATE TRIGGER add_source_point AFTER DELETE ON _gp_tiles_gate_tx
    FOR EACH STATEMENT EXECUTE FUNCTION add_point();
SELECT ST_BuildPyramid('points','geom','id','{"name":"gate_tx","sourceSRS":3857,"minZoom":4,"maxZoom":4,"parallel":4,"preparedBuild":true}');
DO $$ BEGIN
    IF ST_Tile('gate_tx',4,3,4) IS NULL THEN RAISE EXCEPTION 'write from output trigger disappeared'; END IF;
END $$;
DROP TRIGGER add_source_point ON _gp_tiles_gate_tx;
ALTER TABLE points ADD COLUMN observed timestamptz DEFAULT '2025-01-01 00:00:00+00';
SET TIME ZONE 'America/New_York';
SELECT ST_BuildPyramid('points','geom','id','{"name":"gate_tx","sourceSRS":3857,"minZoom":4,"maxZoom":4,"parallel":4,"attributes":["observed"]}');
DO $$ BEGIN
    IF NOT (SELECT bool_and(position(convert_to('2024-12-31 19:00:00-05','UTF8') IN mvt)>0)
            FROM ONLY _gp_tiles_gate_tx) THEN RAISE EXCEPTION 'mixed session time zones in MVT'; END IF;
END $$;

-- Both the registered source and output must remain bound after a path change.
CREATE SCHEMA gp_gate_other;
CREATE TABLE gp_gate_other.points AS SELECT 90 AS id,ST_Centroid(ST_TileEnvelope(5,5,6)) AS geom;
CREATE TABLE gp_gate_other._gp_tiles_gate_tx(dummy int);
SET search_path = gp_gate_other, gp_gate, public;
DO $$ BEGIN
    IF ST_Tile('gate_tx',4,3,7) IS NULL OR ST_Tile('gate_tx',5,5,6) IS NOT NULL
       THEN RAISE EXCEPTION 'search_path rebound the pyramid'; END IF;
END $$;
SELECT ST_DropPyramid('gate_tx');
DO $$ BEGIN
    IF to_regclass('gp_gate_other._gp_tiles_gate_tx') IS NULL THEN RAISE EXCEPTION 'foreign table was dropped'; END IF;
END $$;

SET search_path = public;
DROP SCHEMA gp_gate_other, gp_gate CASCADE;
REVOKE ALL ON public.geometry_pyramid_catalog, public.geometry_pyramid_stats FROM gp_gate_group;
DROP ROLE gp_gate_login, gp_gate_group;
