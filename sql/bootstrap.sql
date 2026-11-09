-- geometry_pyramid bootstrap: catalog table that tracks every pyramid
-- registered in the database, plus convenience views/functions.
--
-- Tile data is stored per-pyramid in `_gp_tiles_<name>` tables created by
-- ST_BuildPyramid (see src/storage.rs).

CREATE TABLE IF NOT EXISTS geometry_pyramid_catalog (
    pyramid_name  text PRIMARY KEY,
    source_table  text NOT NULL,
    source_rel    regclass,
    storage_rel   regclass,
    geom_column   text NOT NULL,
    fid_column    text NOT NULL,
    config        jsonb NOT NULL DEFAULT '{}'::jsonb,
    built_at      timestamptz NOT NULL DEFAULT now()
);

COMMENT ON TABLE geometry_pyramid_catalog IS
    'Registry of vector pyramids managed by the geometry_pyramid extension.';

-- Per-zoom build statistics, maintained by the build/update paths.
CREATE TABLE IF NOT EXISTS geometry_pyramid_stats (
    pyramid_name   text     NOT NULL,
    z              smallint NOT NULL,
    tile_count     bigint   NOT NULL,
    total_bytes    bigint   NOT NULL,
    max_tile_bytes bigint   NOT NULL,
    build_ms       bigint   NOT NULL,
    attempts       integer  NOT NULL DEFAULT 1,
    effective_knobs jsonb,
    built_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (pyramid_name, z)
);

COMMENT ON TABLE geometry_pyramid_stats IS
    'Per-zoom build statistics for pyramids managed by geometry_pyramid.';

CREATE OR REPLACE FUNCTION public.ST_Pyramids()
RETURNS TABLE (name text, source_table text, built_at timestamptz, tile_count bigint)
LANGUAGE sql STABLE AS $$
    SELECT c.pyramid_name, c.source_table, c.built_at, COALESCE(sum(s.tile_count), 0)::bigint
    FROM public.geometry_pyramid_catalog c
    LEFT JOIN public.geometry_pyramid_stats s USING (pyramid_name)
    GROUP BY c.pyramid_name, c.source_table, c.built_at
    ORDER BY c.pyramid_name
$$;
