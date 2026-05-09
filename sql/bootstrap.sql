-- geometry_pyramid bootstrap: catalog table that tracks every pyramid
-- registered in the database. Tile data is stored in per-pyramid tables
-- created lazily by ST_BuildPyramid (not yet implemented).

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
