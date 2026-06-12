-- geometry_pyramid bootstrap: catalog table that tracks every pyramid
-- registered in the database, plus convenience views/functions.
--
-- Tile data is stored per-pyramid in `_gp_tiles_<name>` tables created by
-- ST_BuildPyramid (see src/storage.rs).

CREATE TABLE IF NOT EXISTS geometry_pyramid_catalog (
    pyramid_name  text PRIMARY KEY,
    source_table  text NOT NULL,
    geom_column   text NOT NULL,
    fid_column    text NOT NULL,
    config        jsonb NOT NULL DEFAULT '{}'::jsonb,
    built_at      timestamptz NOT NULL DEFAULT now()
);

COMMENT ON TABLE geometry_pyramid_catalog IS
    'Registry of vector pyramids managed by the geometry_pyramid extension.';

-- ST_Pyramids() — list every registered pyramid with its tile count.
-- Implemented in plain SQL: a SETOF query over the catalog joined with
-- count(*) from each pyramid's storage table.
CREATE OR REPLACE FUNCTION ST_Pyramids()
RETURNS TABLE (
    name         text,
    source_table text,
    built_at     timestamptz,
    tile_count   bigint
)
LANGUAGE plpgsql STABLE AS $$
DECLARE
    rec RECORD;
    cnt bigint;
BEGIN
    FOR rec IN
        SELECT pyramid_name, source_table AS src, built_at
        FROM geometry_pyramid_catalog
        ORDER BY pyramid_name
    LOOP
        BEGIN
            EXECUTE format('SELECT count(*)::bigint FROM %I',
                           '_gp_tiles_' || rec.pyramid_name)
                INTO cnt;
        EXCEPTION WHEN undefined_table THEN
            cnt := 0;
        END;
        name         := rec.pyramid_name;
        source_table := rec.src;
        built_at     := rec.built_at;
        tile_count   := COALESCE(cnt, 0);
        RETURN NEXT;
    END LOOP;
END;
$$;

COMMENT ON FUNCTION ST_Pyramids() IS
    'List every registered pyramid with its materialized tile count.';
