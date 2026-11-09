# Use and operate geometry_pyramid

[简体中文](usage.zh-CN.md) · [README](../README.md)

This guide is for database owners who build, serve, refresh, and upgrade MVT
pyramids. Run examples in a database where both PostGIS and
`geometry_pyramid` are installed. For installation from source, start with the
[README](../README.md#try-one-tile).

## Prepare a source table

Enable the extensions once per database:

```sql
CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;
SELECT geometry_pyramid_version();
```

Use an ordinary table with a geometry column and a stable, unique feature ID.
Set `sourceSRS` to the geometry's SRID; its default is 4326. A geometry GiST
index helps builds, regional refreshes, and live tile reads. For example:

```sql
CREATE TABLE buildings (
    gid bigint PRIMARY KEY,
    name text,
    geom geometry(Polygon, 4326)
);
-- Load data before building the pyramid.
CREATE INDEX buildings_geom_gix ON buildings USING gist (geom);
```

Source table, column, and pyramid names must be bare ASCII identifiers of at
most 45 characters. Schema-qualified source names are not supported.
For other source projections, a functional GiST index on
`ST_Transform(geom, 3857)` can help source and live tile queries.

## Build and verify

```sql
SELECT ST_BuildPyramid(
    'buildings', 'geom', 'gid',
    '{"name":"buildings","sourceSRS":4326,"maxZoom":14,"attributes":["name"]}'
);
SELECT ST_Pyramid('buildings');
SELECT * FROM ST_Pyramids();
```

`ST_BuildPyramid` returns `true` on success. `ST_Pyramid` returns the
configuration and per-zoom tile counts, bytes, duration, and retry statistics.
`ST_Pyramids()` lists registered pyramids. The initial build binds the source
and output database objects; later reads do not depend on the caller's
`search_path`.

Building the same name and source again is a full rebuild. The builder replaces
published tile rows in one transaction, preserving the output table's OID,
grants, and dependent views. A failed transaction rolls back its tile changes.
For a bulk source load, complete and verify the load before rebuilding.

## Read and serve tiles

```sql
SELECT ST_Tile('buildings', 14, 13488, 6210);
SELECT octet_length(ST_Tile('buildings', 14, 13488, 6210));
```

`ST_Tile(name, z, x, y)` returns MVT bytes as `bytea`, or `NULL` for an
empty tile, an unknown pyramid, a zoom below `minZoom`, or valid zoom with
out-of-range x/y. Zoom outside 0 to 24 and malformed names raise errors.

Within `maxZoom`, a stored tile is returned first. With the default
`splitSize:0`, a storage miss is empty. With `splitSize > 0`, a miss may be a
deferred sparse tile and is sliced from the current source on every request.
Above `maxZoom`, live slicing is enabled by default; set
`dynamicBeyondMaxZoom:false` to return `NULL` instead. Live slicing has no
extension-level cache.

An HTTP service can map `GET /tiles/{name}/{z}/{x}/{y}.pbf` to a parameterized
`SELECT ST_Tile($1, $2, $3, $4)`. Return MVT for bytes and an empty response
for `NULL`. The [local demo](../demo/README.md) shows a small server and
MapLibre viewer; its synchronous single connection is for acceptance use.

## Refresh after source changes

Choose the refresh by the *affected old and new locations*:

| Change | Call | Coverage |
|---|---|---|
| Many changes or uncertain extent | `ST_UpdatePyramid('buildings', '')` | Full rebuild |
| Existing rows still selected by a predicate | `ST_UpdatePyramid('buildings', 'gid IN (1, 2)')` | Tiles intersecting currently matched rows |
| Local inserts, deletes, or moves | `ST_UpdatePyramidByExtent('buildings', xmin, ymin, xmax, ymax, srid)` | Every tile intersecting the supplied region |

For a move, include **both** the old and new footprint in the extent, or run a
full rebuild. Predicate refresh cannot discover a deleted row's old location.
The extent is expressed in the supplied SRID, for example:

```sql
SELECT ST_UpdatePyramidByExtent(
    'buildings', 116.30, 39.90, 116.42, 39.99, 4326
);
```

Regional refreshes replace dirty tile rows transactionally. Some projected or
date-line crossing extents fall back to a full local rebuild to preserve
coverage. Verify `ST_Pyramid('buildings')` and read representative affected
tiles afterward.

To remove a pyramid and its tile storage:

```sql
SELECT ST_DropPyramid('buildings');
```

Drop the pyramid **before** dropping its bound source table. This operation
removes tiles and statistics; rebuilding requires the source data.

## Configuration reference

`ST_BuildPyramid` accepts a JSON document; `'{}'` uses defaults.
`maxLevel` is an alias for `maxZoom`; supplying both is an error.

| Key | Default | Effect or constraint |
|---|---:|---|
| `name` | source table name | Pyramid and tile-table identity. |
| `sourceSRS` | `4326` | Source geometry SRID; set it to match the column. |
| `tileSRS` | `3857` | Only EPSG:3857 is accepted. |
| `minZoom`, `maxZoom` | `0`, `18` | Inclusive materialized range; `maxZoom <= 24`. |
| `tileSize`, `tileExtend` | `4096`, `8` | MVT extent (1 to 16384) and edge buffer (less than extent). |
| `attributes` | `[]` | Non-geometry source columns in MVT features. |
| `dynamicBeyondMaxZoom` | `true` | Live-slice valid zooms above `maxZoom`. |
| `splitSize` | `0` | Defer tiles below the feature-count threshold to live reads; 0 stores all nonempty tiles. |
| `parallel` | `1` | Build slots including coordinator; 0 selects auto, capped at 32. |
| `preparedBuild` | `false` | Pre-project source geometry before slicing. |
| `featureMajorPoints` | `false` | Sparse pure-POINT path; requires `preparedBuild:true`, `parallel:1`, `tileSRS:3857`. |
| `featureMajorMinZoom` | `13` | Minimum `maxZoom` for the point path; 0 removes the zoom gate. |
| `featureDrivenEnum` | `false` | Enumerate candidate tiles from feature envelopes; requires `preparedBuild:true` and `tileSRS:3857`. |
| `simplifyPixels` | `1.0` | Geometry simplification tolerance; 0 disables it. |
| `minFeatureSizePixels` | `0.0` | Filter small lines/polygons; 0 disables it. |
| `dropRate` | `1.0` | Point retention factor by zoom; 1 disables thinning. |
| `tileSizeBudgetBytes` | `0` | Retry oversized uncompressed MVT tiles with stronger reduction; 0 disables retries. Incompatible with `parallel > 1`. |
| `maxTileSizeAttempts` | `5` | Retry limit per zoom; the budget is a target, not a guaranteed cap. |
| `buildTimeoutMs` | `0` | Parallel worker/preparation wait limit in milliseconds; 0 is unbounded. |
| `buildRules` | `[]` | Per-zoom filters, output attributes, sampling, or point grids. |

The optional fast paths depend on data shape. Start with defaults and measure
on your own data. [Measured runs](../docs/user-data-build-performance.md) are
examples, not universal speed guarantees.

### Per-zoom detail with buildRules

Each rule needs `minZoom`, `maxZoom`, and at least one action:

| Action | Form | Behavior |
|---|---|---|
| Filter | `"filter":"importance >= 80"` | SQL boolean predicate over source columns. Overlapping filters combine with AND. |
| Attributes | `"attrFields":["name"]` | Override output properties at those zooms. |
| Sample | `"sample":{"rate":25,"key":["gid"]}` | Deterministically retain a percentage; `rate` is 0 to 100. `key` defaults to the feature ID. Optional `geometryTypes` limits affected types. Overlapping samples combine with AND. |
| Point grid | `"pointGrid":{"cellPixels":64,"keepField":"important"}` | Keep the lowest-ID point per occupied cell, or marked points when present. `cellPixels` is 1 through `tileSize`. |

Overlapping `attrFields` rules must agree. `keepField` must name a boolean
source column; the point grid leaves multipoints, lines, and polygons alone.

For example, keep major buildings and fewer fields at coarse zooms:

```json
{
  "name": "buildings",
  "maxZoom": 14,
  "attributes": ["name", "importance"],
  "buildRules": [
    {"minZoom": 0, "maxZoom": 5,
     "filter": "importance >= 80", "attrFields": ["name"]}
  ]
}
```

`buildRules.filter` and `ST_UpdatePyramid(where_clause)` are raw SQL
predicates evaluated with the caller's privileges. Treat them as
trusted-administrator input. The semicolon check on `filter` does not make
arbitrary input safe; do not pass user text through these fields or a
`SECURITY DEFINER` wrapper.

## Upgrade from 0.1.0

Install the 0.1.1 extension files on the PostgreSQL server first. Version
0.1.0 stored bare relation names, so identify the **actual schema-qualified**
source and tile table for every pyramid before binding them. Back up the
database and plan a maintenance window. The sample names below must be
replaced with inspected relations.

```sql
SELECT pyramid_name, source_table FROM public.geometry_pyramid_catalog;
-- Confirm each source and _gp_tiles_<name> table in pg_class before proceeding.
```

Run the upgrade and every binding in one transaction:

```sql
BEGIN;
ALTER EXTENSION geometry_pyramid UPDATE TO '0.1.1';
UPDATE public.geometry_pyramid_catalog
SET source_rel = 'data.buildings'::regclass,
    storage_rel = 'tiles._gp_tiles_buildings'::regclass
WHERE pyramid_name = 'buildings';
-- Repeat UPDATE for every verified pyramid.
DO $$ BEGIN
    IF EXISTS (SELECT FROM public.geometry_pyramid_catalog
               WHERE source_rel IS NULL OR storage_rel IS NULL) THEN
        RAISE EXCEPTION 'unbound pyramids remain';
    END IF;
END $$;
COMMIT;
```

On any error, `ROLLBACK` and correct the relation mapping before retrying.
Then inspect `ST_Pyramids()`, `ST_Pyramid(name)`, and a known tile. For legacy
pyramids that used a tile byte budget, run a full refresh before relying on
deferred dynamic reads; the refresh records the effective per-zoom settings.

## Troubleshoot

| Symptom | Check or action |
|---|---|
| `CREATE EXTENSION` fails | Verify PostgreSQL and PostGIS installation and that the extension package matches the server's major version. |
| Build rejects a name | Use a bare ASCII identifier of at most 45 characters; schema-qualified source names are unsupported. |
| Build rejects `tileSRS` | Set it to 3857; the current tile grid is Web Mercator only. |
| `ST_Tile` returns `NULL` | Check the name, tile coverage, `minZoom`, `maxZoom`, and `dynamicBeyondMaxZoom`; empty tiles return `NULL`. Invalid zoom outside 0 to 24 raises an error. |
| Old tile remains after delete or move | Refresh an extent covering its old location, or rebuild fully. |
| Live tile reads are slow | Check `splitSize` and requests above `maxZoom`; add a serving cache or materialize more tiles. |
| Build is slow | Inspect geometry indexes, source extent, zoom range, and per-zoom stats; benchmark `preparedBuild`, feature paths, or `buildRules` on target data. |
| Parallel build rejects tile budget | Use `parallel:1` for `tileSizeBudgetBytes`, or disable the byte budget. |
| Filter fails | Test the predicate against the source table using the same role; avoid untrusted input. |

For developer checks and benchmark scripts, see
[operations, testing, and benchmarks](operations/testing-and-benchmarks.md).
