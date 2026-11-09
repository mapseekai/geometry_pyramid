-- Historical rows contain bare names. The owner binds their verified relations
-- in the same transaction as ALTER EXTENSION UPDATE (see README).
ALTER TABLE public.geometry_pyramid_catalog
    ADD COLUMN source_rel regclass,
    ADD COLUMN storage_rel regclass;
ALTER TABLE public.geometry_pyramid_stats ADD COLUMN effective_knobs jsonb;

CREATE OR REPLACE FUNCTION public.ST_Pyramids()
RETURNS TABLE (name text, source_table text, built_at timestamptz, tile_count bigint)
LANGUAGE sql STABLE AS $$
    SELECT c.pyramid_name, c.source_table, c.built_at, COALESCE(sum(s.tile_count), 0)::bigint
    FROM public.geometry_pyramid_catalog c
    LEFT JOIN public.geometry_pyramid_stats s USING (pyramid_name)
    GROUP BY c.pyramid_name, c.source_table, c.built_at
    ORDER BY c.pyramid_name
$$;
