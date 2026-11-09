# Pyramid Lifecycle Workflows

## 1. Build or rebuild a pyramid

Call `ST_BuildPyramid(table, geom_column, fid_column, config_json)` after loading and indexing a PostGIS source table. `src/pyramid.rs` parses `PyramidConfig`, validates identifiers and invariants, upserts `geometry_pyramid_catalog`, and calls `build::build`.

Recommended source-table setup:

```sql
CREATE TABLE buildings (
  gid  serial PRIMARY KEY,
  name text,
  geom geometry(Polygon, 4326)
);
CREATE INDEX ON buildings USING gist (geom);

SELECT ST_BuildPyramid(
  'buildings', 'geom', 'gid',
  '{"minZoom":0,"maxZoom":14,"attributes":["name"]}'
);
```

Operational notes:

- Re-running `ST_BuildPyramid` for the same `name` replaces published tile rows in one transaction and preserves the tile table's OID and grants. Parallel builds use a staging table (`src/build.rs`).
- Empty or all-NULL geometry sources build a valid empty pyramid; tests cover this (`src/build.rs`, `src/lib.rs`).
- Identifier length is capped so generated names fit PostgreSQL's 63-byte identifier limit (`README.md`, `src/config.rs`, `src/pyramid.rs`).
- `tileSizeBudgetBytes` cannot be combined with `parallel > 1`; the code rejects it explicitly (`src/build.rs`).

## 2. Fetch tiles

Use `ST_Tile(name, z, x, y)`:

```sql
SELECT ST_Tile('buildings', 14, 13488, 6210);
```

`src/tile.rs` handles three read modes:

- `minZoom <= z <= maxZoom` and tile is present: return materialized MVT bytes from `_gp_tiles_<name>`.
- Materialized-range miss with `splitSize > 0`: dynamically re-slice from the source because sparse tiles may have been deferred.
- `z > maxZoom` with `dynamicBeyondMaxZoom:true`: dynamically slice from source to mirror Ganos `maxLevel` behavior.

`src/dynamic.rs` shares the build pipeline and LOD clauses, so dynamic reads should respect `buildRules.filter`, `attrFields`, and `sample` the same way stored tiles do. Dynamic slicing is not cached; repeated reads can be expensive and should have a source GiST index.

## 3. Inspect metadata and stats

```sql
SELECT ST_Pyramid('buildings');
SELECT * FROM ST_Pyramids();
```

`ST_Pyramid` returns metadata and per-zoom statistics. `ST_Pyramids()` is PL/pgSQL from `sql/bootstrap.sql` and counts rows in each `_gp_tiles_<name>` table, returning zero if storage is absent.

Stats are maintained by `src/stats.rs` and updated by both full and incremental build paths. Use them to reason about tile count, total bytes, max tile bytes, build duration, and retry attempts per zoom.

## 4. Refresh source changes

There are two update paths with different correctness tradeoffs.

### Predicate update

```sql
SELECT ST_UpdatePyramid('buildings', 'name = ''City Hall''');
SELECT ST_UpdatePyramid('buildings', ''); -- full rebuild
```

With a non-empty clause, `src/incremental.rs` computes the extent of currently matched rows, deletes tiles in the dirty rectangle for each zoom, and rebuilds just those tiles in the caller transaction. This is fast when the predicate truly covers the changed region.

Caveat: a predicate update cannot clean old locations for deleted rows or moved rows unless the predicate/region still covers those old tiles. `src/pyramid.rs` documents this explicitly. Use a full rebuild or the bbox path for delete/move-out cases.

### Bbox update

```sql
SELECT ST_UpdatePyramidByExtent('buildings', 116.30, 39.90, 116.42, 39.99, 4326);
```

`ST_UpdatePyramidByExtent` transforms the supplied bbox to tile SRS and re-slices all intersecting tiles from the current source table. This handles inserts, updates, deletes, and moves inside the region because it does not depend on rows matching a predicate after the fact (`src/incremental.rs`).

## 5. Drop a pyramid

```sql
SELECT ST_DropPyramid('buildings');
```

`ST_DropPyramid` deletes the catalog row, drops tile storage, deletes stats, and abandons shadow state (`src/pyramid.rs`, `src/storage.rs`, `src/stats.rs`, `src/shadow.rs`). It returns whether a catalog row was removed.

## 6. Run the local demo

The `demo/` directory is a local acceptance tool: a Python HTTP server serves
`ST_Tile` bytes and local MapLibre assets. Follow the current
[demo runbook](../../demo/README.md) for database setup, seeding, port
selection, and verification. Its optional online OSM basemap is off by default.

Current `demo/server.py` serves `GET /tiles/{name}/{z}/{x}/{y}.pbf`, validates the path with a simple identifier regex, calls `SELECT ST_Tile(%s, %s, %s, %s)`, and maps SQL NULL/empty bytes to HTTP 204. It is single-connection and synchronous; treat it as local acceptance/demo code, not production serving infrastructure.

## Workflow decision guide

- Need maximum read speed within `maxZoom`: keep `splitSize:0` so every non-empty tile is materialized.
- Need smaller/faster builds for sparse data and can tolerate live reads: use `splitSize > 0` and keep source indexes healthy.
- Need coarse-zoom simplification by business importance: use `buildRules.filter`, `attrFields`, and/or `sample` by zoom band.
- Need high-zoom sparse build speed: consider `preparedBuild:true` with `featureDrivenEnum:true`; for pure points, consider `featureMajorPoints:true` when `maxZoom` is high enough.
- Need safe local correction after deletes/moves: prefer `ST_UpdatePyramidByExtent` over predicate update.
