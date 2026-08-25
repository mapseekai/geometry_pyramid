# geometry_pyramid Usage Guide

This guide explains how to use `geometry_pyramid` as a PostgreSQL/PostGIS extension: install or enable it, prepare source data, build vector tile pyramids, read tiles, refresh changes, and choose common configuration patterns.

For implementation details, see [Architecture overview](architecture/overview.md). For deeper workflow notes, see [Pyramid lifecycle workflows](workflows/pyramid-lifecycle.md) and [Config and LOD domain concepts](domain/config-and-lod.md).

## 1. What geometry_pyramid does

`geometry_pyramid` builds Mapbox Vector Tile (MVT) pyramids from PostGIS geometry tables. A source table is registered with `ST_BuildPyramid`, tiles are materialized into a per-pyramid storage table, and applications read tile bytes with `ST_Tile`.

The extension delegates geometry processing and MVT encoding to PostGIS (`ST_TileEnvelope`, `ST_AsMVTGeom`, `ST_AsMVT`) and coordinates the build/read lifecycle from Rust via pgrx.

## 2. Requirements

At runtime you need:

- PostgreSQL with the `geometry_pyramid` extension installed.
- PostGIS enabled in the target database.
- A source table with:
  - a geometry column,
  - a stable feature-id column,
  - a spatial index on the geometry column for practical build and dynamic-read performance.

For local development from this repository, common commands are:

```bash
cargo check
cargo pgrx run pg17
cargo pgrx test
cargo pgrx package
```

The crate defaults to PostgreSQL 17 features and targets pgrx `0.18.0` (`Cargo.toml`). README setup notes cover PostgreSQL 13-18 feature flags.

## 3. Enable the extensions

Run once per database:

```sql
CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;
```

Confirm the installed extension version:

```sql
SELECT geometry_pyramid_version();
```

## 4. Prepare source data

A typical source table has a primary key or stable id column and a PostGIS geometry column:

```sql
CREATE TABLE buildings (
  gid  serial PRIMARY KEY,
  name text,
  kind text,
  geom geometry(Polygon, 4326)
);

-- Load rows first, then add a spatial index.
CREATE INDEX buildings_geom_gix ON buildings USING gist (geom);
```

Guidelines:

- `sourceSRS` in the pyramid config must match the source geometry SRID.
- The feature id column should be stable across rebuilds and updates.
- Keep a GiST index on the geometry column, especially when using dynamic reads (`splitSize > 0` or beyond-`maxZoom` reads).
- Current identifier inputs are short bare identifiers. Schema-qualified source table names are not supported.

## 5. Build a pyramid

Use `ST_BuildPyramid(table, geom_column, fid_column, config_json)`:

```sql
SELECT ST_BuildPyramid(
  'buildings',
  'geom',
  'gid',
  '{
     "name": "buildings",
     "sourceSRS": 4326,
     "tileSRS": 3857,
     "minZoom": 0,
     "maxZoom": 14,
     "attributes": ["name", "kind"]
   }'
);
```

Important behavior:

- The `name` config key names the pyramid. It defaults to the source table name, but setting it explicitly is recommended.
- Re-running `ST_BuildPyramid` for the same pyramid name rebuilds it in place.
- Full rebuilds are atomic: tiles are written to shadow storage and swapped into place, so readers should not see a half-built pyramid.
- Empty or all-NULL geometry sources build a valid empty pyramid.

## 6. Read tiles

Use `ST_Tile(name, z, x, y)`:

```sql
SELECT ST_Tile('buildings', 14, 13488, 6210);
```

Return value:

- `bytea` containing MVT/PBF bytes when the tile exists.
- `NULL` when the tile is empty, outside the configured range, disabled, invalid, or the pyramid name is unknown.

Read modes:

- Within `maxZoom`, stored tiles are read from `_gp_tiles_<name>` when present.
- With `splitSize:0` (default), every non-empty tile within `maxZoom` is materialized, so misses return `NULL` without dynamic slicing.
- With `splitSize > 0`, sparse tiles may be skipped at build time; a stored miss within `maxZoom` is sliced live from the source table.
- Above `maxZoom`, `ST_Tile` slices live only when `dynamicBeyondMaxZoom` is `true`.

PostGIS provides tile envelopes when clients need them:

```sql
SELECT ST_TileEnvelope(14, 13488, 6210);
```

## 7. Inspect pyramids and build statistics

Inspect one pyramid:

```sql
SELECT ST_Pyramid('buildings');
```

List all pyramids:

```sql
SELECT * FROM ST_Pyramids();
```

The metadata includes configuration and per-zoom statistics such as tile count, total bytes, max tile bytes, build duration, and retry attempts.

Each pyramid stores tiles in `_gp_tiles_<name>` with MVT bytes in the `mvt` column. Prefer `ST_Tile` for application reads unless you deliberately need direct storage access.

## 8. Refresh after source changes

There are three common refresh patterns.

### Full rebuild

Use an empty where clause:

```sql
SELECT ST_UpdatePyramid('buildings', '');
```

Use this when many rows changed, configuration changed, or you need the safest simple correction.

### Predicate refresh

Refresh tiles intersecting currently matched rows:

```sql
SELECT ST_UpdatePyramid('buildings', 'name = ''City Hall''');
```

This is fast when the predicate truly covers the changed area. It is not sufficient for deletes or moved-out features when the old tile location is no longer covered by the predicate.

### Bbox refresh

Refresh every tile intersecting a region from the current source table:

```sql
SELECT ST_UpdatePyramidByExtent(
  'buildings',
  116.30, 39.90, 116.42, 39.99,
  4326
);
```

Prefer this for localized inserts, updates, deletes, and moves where you know the affected region.

## 9. Drop a pyramid

```sql
SELECT ST_DropPyramid('buildings');
```

This removes the catalog row, tile storage, statistics, and abandoned shadow state for the pyramid. It returns whether a catalog row was removed.

## 10. Configuration reference

`config_json` is a JSON document. Pass `'{}'` for defaults. Use this section when deciding what each parameter means and how to tune it.

### 10.1 Quick reference

| Key | Default | Primary purpose | Tune when... |
|---|---:|---|---|
| `name` | source table name | Names the pyramid and its storage. | You need multiple pyramids for one source table or a stable public tile name. |
| `sourceSRS` | `4326` | Declares the SRID of the source geometry column. | Your source data is not EPSG:4326. |
| `tileSRS` | `3857` | Declares the tile coordinate system. | You intentionally need non-Web-Mercator tiles. |
| `tileSize` | `4096` | MVT coordinate extent. | You need compatibility with a client/style expecting a different extent. |
| `tileExtend` | `8` | Geometry buffer around tile edges. | You see clipped lines/polygons or duplicated edge artifacts. |
| `minZoom` | `0` | Lowest zoom to materialize. | You do not serve global/coarse zooms. |
| `maxZoom` | `18` | Highest zoom to materialize. | You need more detail, smaller builds, or stricter serving bounds. |
| `attributes` | `[]` | Global MVT property columns. | You need feature properties in clients or want smaller tiles. |
| `dynamicBeyondMaxZoom` | `true` | Live-slice requests above `maxZoom`. | You want strict prebuilt-only serving or flexible high-zoom reads. |
| `splitSize` | `0` | Defer sparse tiles to dynamic slicing. | You want smaller/faster builds and can tolerate slower misses. |
| `parallel` | `1` | Number of build workers. | Builds are slow and the database host has spare CPU/I/O. |
| `preparedBuild` | `false` | Pre-transform source geometries before slicing. | Repeated transforms/simplification dominate build cost. |
| `featureMajorPoints` | `false` | Point-source sparse high-zoom path. | The source is pure points and high zoom tile-major probing is expensive. |
| `featureMajorMinZoom` | `13` | Gate for `featureMajorPoints`. | You want that path to engage earlier or later. |
| `featureDrivenEnum` | `false` | Sparse high-zoom path for all geometry types. | The source bbox covers many empty tiles. |
| `simplifyPixels` | `1.0` | Geometry simplification tolerance. | Tiles are large or geometry detail is excessive. |
| `minFeatureSizePixels` | `0.0` | Drop tiny lines/polygons. | Small features clutter coarse zooms or inflate tile size. |
| `dropRate` | `1.0` | Point retention by zoom. | Dense point layers need thinning at low zoom. |
| `tileSizeBudgetBytes` | `0` | Per-tile byte-size protection. | You must cap tile payload size. |
| `maxTileSizeAttempts` | `5` | Retry limit for tile-size budget. | Budget enforcement stops too early or spends too long retrying. |
| `buildTimeoutMs` | `0` | Parallel worker wait limit per zoom. | You need bounded parallel build runtime. |
| `buildRules` | `[]` | Per-zoom LOD filtering, attributes, and sampling. | Different zoom bands need different data density/properties. |

### 10.2 Identity and coordinate parameters

#### `name`

- **Meaning:** Logical pyramid name. It is used in SQL calls such as `ST_Tile('buildings', z, x, y)` and in derived storage names such as `_gp_tiles_<name>`.
- **Default:** The source table name.
- **When to change it:** Set it explicitly whenever the tile layer name is part of an API, when one source table needs multiple pyramids, or when you want a shorter/stable name independent of the table.
- **Adjustment guidance:** Choose a short bare identifier, for example `buildings_z14` or `roads_public`. Avoid schema prefixes, punctuation, spaces, and very long names.
- **Tradeoffs/watchpoints:** Reusing an existing name rebuilds that pyramid. Current identifier validation does not support schema-qualified names.

#### `sourceSRS`

- **Meaning:** SRID of the source geometry column. The build pipeline uses it to interpret source bboxes and transform geometries into the tile SRS.
- **Default:** `4326`.
- **When to change it:** Set it to the real SRID of the source column, for example `3857`, `4490`, or a local projected SRID.
- **Adjustment guidance:** Do not tune this for performance. It must match the data. If the source table mixes SRIDs, normalize the data before building.
- **Tradeoffs/watchpoints:** A wrong `sourceSRS` can produce misplaced or empty tiles.

#### `tileSRS`

- **Meaning:** SRID used for tile coordinates and tile envelopes. The default Web Mercator value matches standard web map tiles.
- **Default:** `3857`.
- **When to change it:** Only when your client and tile scheme intentionally use another tile coordinate system.
- **Adjustment guidance:** Keep `3857` for MapLibre, web maps, and most XYZ tile clients.
- **Tradeoffs/watchpoints:** Some accelerated paths require `tileSRS=3857`. Non-standard tile SRS choices may not work with typical web clients.

#### `tileSize`

- **Meaning:** MVT extent, i.e. the integer coordinate grid used inside each vector tile. It is not the HTTP tile pixel size.
- **Default:** `4096`.
- **When to change it:** Rarely. Change only for compatibility with a renderer/style that expects another extent.
- **Adjustment guidance:** Keep `4096` unless you have a specific reason. Larger values preserve more coordinate precision but can increase tile size. Smaller values reduce precision.
- **Tradeoffs/watchpoints:** Must be positive and at most `16384`. `tileExtend` must remain smaller than `tileSize`.

#### `tileExtend`

- **Meaning:** Buffer around tile edges, in MVT extent units, used to reduce clipping artifacts for features crossing tile boundaries.
- **Default:** `8`.
- **When to change it:** Increase if lines or polygons appear clipped at tile edges. Decrease if edge duplication or tile size is a concern.
- **Adjustment guidance:** Start with the default. Try small increases before large ones.
- **Tradeoffs/watchpoints:** Larger buffers include more geometry outside the tile boundary, which can increase tile size and duplicated edge work. Must be smaller than `tileSize`.

### 10.3 Zoom range and emitted attributes

#### `minZoom`

- **Meaning:** Lowest zoom level materialized by the build. Requests below this range return `NULL`.
- **Default:** `0`.
- **When to change it:** Raise it when you do not serve global/coarse zooms or when low zooms would be visually meaningless.
- **Adjustment guidance:** For city/building datasets, `10` or higher may be appropriate. For country/global context layers, keep it low.
- **Tradeoffs/watchpoints:** Raising `minZoom` reduces build work and storage, but clients must not expect tiles below that zoom.

#### `maxZoom`

- **Meaning:** Highest zoom level materialized by the build. Within this range, tiles are primarily served from storage.
- **Default:** `18`.
- **When to change it:** Raise it for more prebuilt detail. Lower it to reduce build time/storage or rely on live slicing above the materialized range.
- **Adjustment guidance:** Pick the highest zoom your application routinely serves from storage. If high zoom traffic is rare, use a lower `maxZoom` with `dynamicBeyondMaxZoom:true`; if high zoom traffic is frequent, materialize those zooms.
- **Tradeoffs/watchpoints:** Higher `maxZoom` can dramatically increase tile lattice size and build cost. It is capped at `24`.

#### `attributes`

- **Meaning:** Global list of non-geometry source columns emitted as MVT feature properties.
- **Default:** `[]`, meaning no extra properties.
- **When to change it:** Add fields required by styles, labels, filters, popups, or client-side logic. Remove fields that are not needed.
- **Adjustment guidance:** Keep the list minimal. Prefer short text/category fields and avoid large blobs or verbose JSON. Use `buildRules.attrFields` to emit fewer attributes at coarse zooms.
- **Tradeoffs/watchpoints:** More attributes increase tile size and encoding cost. Field names must be valid source columns/identifiers.

### 10.4 Read/build shape parameters

#### `dynamicBeyondMaxZoom`

- **Meaning:** Controls what happens for `ST_Tile(name, z, x, y)` when `z > maxZoom`. If true, the tile is sliced live from the source table; if false, it returns `NULL`.
- **Default:** `true`.
- **When to change it:** Set false for predictable prebuilt-only serving. Keep true when clients may zoom beyond `maxZoom` and occasional live slicing is acceptable.
- **Adjustment guidance:** For production high-traffic APIs, prefer `false` or add an external cache. For exploratory/internal maps, `true` is convenient.
- **Tradeoffs/watchpoints:** Dynamic slicing is not cached inside the extension and depends on source-table indexes.

#### `splitSize`

- **Meaning:** Density threshold for sparse storage. `0` materializes every non-empty tile. A value greater than `0` skips tiles whose feature count is below the threshold; later requests for those tiles are sliced live.
- **Default:** `0`.
- **When to change it:** Increase it when builds produce many low-density tiles and you prefer smaller/faster builds over fastest repeated reads.
- **Adjustment guidance:** Start with `0`. If build/storage is too large for sparse data, try small values such as `2`, `4`, or `8`, then measure read latency.
- **Tradeoffs/watchpoints:** Higher values defer more tiles, reducing build/storage work but increasing live read load. Even empty-region misses can trigger source slicing. Keep a GiST index and use external caching for repeated requests.

#### `parallel`

- **Meaning:** Number of build workers. Values greater than `1` distribute work across PostgreSQL background workers; `1` is serial. `0` auto-resolves to bounded host parallelism in config handling.
- **Default:** `1`.
- **When to change it:** Increase when builds are CPU-bound or have enough independent tile work and the database host has spare resources.
- **Adjustment guidance:** Try `2` or `4` before larger values. Benchmark on target data.
- **Tradeoffs/watchpoints:** More workers can increase CPU, I/O, and database contention. It may not help few-heavy-tile workloads. `tileSizeBudgetBytes` with `parallel > 1` is rejected.

#### `buildTimeoutMs`

- **Meaning:** Per-zoom upper bound, in milliseconds, for how long the coordinator waits for parallel workers. `0` means no timeout.
- **Default:** `0`.
- **When to change it:** Set it when operational jobs must fail rather than run indefinitely.
- **Adjustment guidance:** Leave `0` for manual builds and benchmarking. For scheduled jobs, set a value based on observed per-zoom build time plus safety margin.
- **Tradeoffs/watchpoints:** Too-low values can abort valid builds under normal load. Only relevant when `parallel > 1`.

### 10.5 Build-speed path parameters

#### `preparedBuild`

- **Meaning:** Pre-transforms and prepares source geometries so later tile slicing can reuse prepared data.
- **Default:** `false`.
- **When to change it:** Enable when repeated transform/simplification work is a major part of build time, especially for larger sources.
- **Adjustment guidance:** Try `true` and compare `ST_Pyramid` build stats or benchmark results. It is often a prerequisite for feature-derived sparse paths.
- **Tradeoffs/watchpoints:** Preparation itself costs time and temporary storage. It may be neutral or only mildly helpful for small/concentrated datasets.

#### `featureMajorPoints`

- **Meaning:** Specialized sparse high-zoom build path for pure point sources. Instead of probing every bbox tile, it computes tile ids from point coordinates.
- **Default:** `false`.
- **When to change it:** Enable for pure `POINT` datasets with high `maxZoom` where tile-major empty-cell probing dominates.
- **Adjustment guidance:** Requires `preparedBuild:true`, `parallel=1`, `tileSRS=3857`, and an eligible pure point source. Use with `featureMajorMinZoom` to avoid shallow-zoom regressions.
- **Tradeoffs/watchpoints:** Mixed geometry, lines, polygons, non-Web-Mercator, and parallel builds fall back or are not eligible.

#### `featureMajorMinZoom`

- **Meaning:** Minimum `maxZoom` at which `featureMajorPoints` engages.
- **Default:** `13`.
- **When to change it:** Lower it if benchmarks show point feature-major helps at shallower zooms; raise it if shallow builds regress.
- **Adjustment guidance:** Keep default unless you have measured data. Set `0` to always engage when otherwise eligible.
- **Tradeoffs/watchpoints:** This is a gate, not a zoom range to build. It controls whether the path engages for the whole eligible build.

#### `featureDrivenEnum`

- **Meaning:** General sparse/high-zoom path for all geometry types. It derives candidate tiles from feature envelopes instead of enumerating every tile in the bbox lattice.
- **Default:** `false`.
- **When to change it:** Enable for sparse datasets, high zooms, or large bboxes with many empty tiles.
- **Adjustment guidance:** Requires `preparedBuild:true`, `parallel=1`, and `tileSRS=3857`. It self-gates when dense enumeration is cheaper, so it is generally safe to test.
- **Tradeoffs/watchpoints:** Preparation and gating add overhead. Dense extents or features spanning many tiles may not benefit.

### 10.6 Generalization and tile-size controls

#### `simplifyPixels`

- **Meaning:** Douglas-Peucker simplification tolerance measured in tile pixels. Higher values simplify geometries more strongly.
- **Default:** `1.0`.
- **When to change it:** Increase when tiles are too large or geometry detail is unnecessary at target zooms. Decrease when geometry shape quality is too degraded. Set `0` to disable.
- **Adjustment guidance:** Try values such as `0.5`, `1.0`, `2.0`, then inspect visual quality and tile sizes.
- **Tradeoffs/watchpoints:** More simplification reduces bytes/build cost but can distort lines and polygons.

#### `minFeatureSizePixels`

- **Meaning:** Drops lines/polygons smaller than the configured size in tile pixels.
- **Default:** `0.0`, disabled.
- **When to change it:** Increase when tiny features clutter low zooms or inflate tile size without visual value.
- **Adjustment guidance:** Start with small non-zero values for coarse layers, then compare visual results. For business-critical small polygons, leave it at `0`.
- **Tradeoffs/watchpoints:** Dropped features are absent from tiles; use `buildRules` when you need zoom-specific behavior.

#### `dropRate`

- **Meaning:** Tippecanoe-style point retention factor by zoom: retained fraction is approximately `dropRate^(maxZoom - z)`.
- **Default:** `1.0`, disabled.
- **When to change it:** Use for dense point layers where low zooms need thinning.
- **Adjustment guidance:** Values below `1.0` thin more aggressively at lower zooms. For example, `0.4` is much stronger than `0.8`.
- **Tradeoffs/watchpoints:** This is a global point thinning knob. Prefer `buildRules.sample` when you need explicit per-zoom or per-geometry sampling.

#### `tileSizeBudgetBytes`

- **Meaning:** Per-tile MVT byte cap. When a tile exceeds the budget, the zoom is retried with tighter simplification/min-size/drop settings. `0` disables.
- **Default:** `0`.
- **When to change it:** Set it when clients, gateways, or storage policies require tile payloads below a hard limit.
- **Adjustment guidance:** Use it as output protection, not first-choice acceleration. Common starting points are application-specific; README mentions `512000` as a tippecanoe-like value.
- **Tradeoffs/watchpoints:** Budget retry can significantly increase build time and may reduce visual detail. It cannot be combined with `parallel > 1`.

#### `maxTileSizeAttempts`

- **Meaning:** Maximum retry rounds per zoom when enforcing `tileSizeBudgetBytes`.
- **Default:** `5`.
- **When to change it:** Increase when budget enforcement often stops before reaching acceptable sizes; decrease when builds spend too long retrying.
- **Adjustment guidance:** Keep default unless tile-size budget logs/stats show a problem.
- **Tradeoffs/watchpoints:** More attempts can improve compliance but extend build time. If tiles still exceed budget after retries, use LOD filters/sampling or reduce attributes.

### 10.7 `buildRules` and nested LOD parameters

`buildRules` is an array of per-zoom rules. It is the preferred way to express level-of-detail behavior that changes by zoom band.

Each rule can contain:

- **`minZoom` / `maxZoom`**: Inclusive zoom band where the rule applies. Use narrow bands for targeted changes and broad bands for simple coarse/mid/fine policies.
- **`filter`**: Raw SQL boolean expression evaluated against source columns. Use it to keep only important features at coarse zooms, such as `rank <= 3` or `importance >= 80`. More restrictive filters reduce tile size and build work but remove features from those zooms. Treat as trusted-admin SQL.
- **`attrFields`**: Attribute override for the covered zooms. Use fewer fields at low zooms to reduce bytes, then allow more fields at high zooms. If multiple rules cover the same zoom and set `attrFields`, they must agree.
- **`sample.rate`**: Deterministic sample percentage from `0` to `100`. Lower values thin more aggressively. `100` keeps everything selected by filters; `0` keeps nothing for the sampled geometry set.
- **`sample.key`**: Source columns used as stable hash keys for deterministic sampling. Defaults to `["fid"]`. Use stable unique ids; avoid fields that change often.
- **`sample.geometryTypes`**: Optional geometry families to sample, such as `["POINT"]`. Use this when you want to thin dense points without sampling polygons or lines.

Rule combination behavior:

- Rules covering the same zoom are combined by AND for filters and sampling.
- Zooms not covered by any rule use all source features and the global `attributes` list.
- Dynamic reads use the same zoom-effective rules as materialized builds, so stored and dynamic tiles should follow the same LOD semantics.

Example:

```json
{
  "attributes": ["class", "name", "importance", "area"],
  "buildRules": [
    {"minZoom": 0, "maxZoom": 4, "filter": "importance >= 80", "attrFields": ["class", "name"]},
    {"minZoom": 5, "maxZoom": 7, "sample": {"rate": 50, "key": ["gid"]}},
    {"minZoom": 8, "maxZoom": 12, "attrFields": ["class", "name", "importance"]}
  ]
}
```

### 10.8 Validation highlights

- `minZoom` must be less than or equal to `maxZoom`.
- `maxZoom` is capped at `24`.
- `tileSize` must be positive and at most `16384`.
- `tileExtend` must be smaller than `tileSize`.
- Identifier-like values must be valid bare identifiers.
- `buildRules.sample.rate` must be between `0` and `100`.
- `tileSizeBudgetBytes` cannot currently be combined with `parallel > 1`.

## 11. Level-of-detail examples

Use `buildRules` to change emitted features or attributes by zoom band.

### Filter coarse zooms

```sql
SELECT ST_BuildPyramid(
  'places', 'geom', 'gid',
  '{
     "name": "places",
     "minZoom": 0,
     "maxZoom": 12,
     "attributes": ["name", "rank", "kind"],
     "buildRules": [
       {"minZoom": 0, "maxZoom": 5, "filter": "rank <= 3"}
     ]
   }'
);
```

### Emit fewer attributes at low zoom

```json
{
  "minZoom": 0,
  "maxZoom": 6,
  "attrFields": ["name", "kind"]
}
```

### Deterministic sampling

```json
{
  "minZoom": 0,
  "maxZoom": 4,
  "sample": { "rate": 25, "key": ["gid"], "geometryTypes": ["POINT"] }
}
```

Rules covering the same zoom are combined by AND for filters and sampling. If multiple covering rules set `attrFields`, they must agree for that zoom.

Security note: `buildRules.filter` is raw SQL evaluated with the caller's privileges. Treat it as trusted-admin input, similar to a view definition. Do not expose filter-bearing build APIs through `SECURITY DEFINER` wrappers or to lower-privilege users.

## 12. Common configuration recipes

### Fast reads, predictable serving

Use the defaults for storage behavior:

```json
{
  "name": "buildings",
  "minZoom": 0,
  "maxZoom": 14,
  "splitSize": 0,
  "dynamicBeyondMaxZoom": false
}
```

This materializes every non-empty tile within `maxZoom` and returns `NULL` above `maxZoom`.

### Smaller builds for sparse data

```json
{
  "name": "sparse_points",
  "minZoom": 0,
  "maxZoom": 14,
  "splitSize": 4,
  "dynamicBeyondMaxZoom": true
}
```

This can reduce build/storage work but turns stored misses into live PostGIS slicing. Keep the source spatial index healthy and avoid this for high-traffic repeated reads unless you add an external cache.

### High-zoom sparse build acceleration

```json
{
  "name": "roads_sparse",
  "minZoom": 0,
  "maxZoom": 14,
  "preparedBuild": true,
  "featureDrivenEnum": true
}
```

`featureDrivenEnum` self-gates and is intended for sparse/high-zoom workloads. For pure point sources, consider `featureMajorPoints:true` when its eligibility requirements are met.

### Coarse-zoom thinning by business importance

```json
{
  "name": "lucc",
  "minZoom": 0,
  "maxZoom": 12,
  "attributes": ["class", "name", "importance"],
  "buildRules": [
    {"minZoom": 0, "maxZoom": 4, "filter": "importance >= 80", "attrFields": ["class", "name"]},
    {"minZoom": 5, "maxZoom": 7, "sample": {"rate": 50, "key": ["gid"]}}
  ]
}
```

## 13. Serving tiles to clients

`ST_Tile` returns raw MVT bytes. A tile server typically maps HTTP paths to SQL:

```text
GET /tiles/{name}/{z}/{x}/{y}.pbf
```

and executes:

```sql
SELECT ST_Tile($1, $2, $3, $4);
```

The repository's `demo/server.py` provides a local acceptance example with MapLibre assets. It is synchronous, single-connection demo code and should not be treated as production serving infrastructure.

For production, put a real HTTP service and cache in front of PostgreSQL, especially if you use dynamic slicing.

## 14. Operational limitations and watchpoints

- Names are restricted to short bare identifiers so generated PostgreSQL identifiers remain safe.
- Schema-qualified source tables are not currently supported.
- Output is 2D MVT/PBF only.
- Dynamic slicing is not cached inside the extension.
- Predicate refresh does not automatically clean old locations for deletes or moved features.
- Unknown pyramids return `NULL` or `false` for many operations instead of raising storage errors.
- `buildRules.filter` and `ST_UpdatePyramid(where_clause)` accept trusted raw SQL predicates.

## 15. Troubleshooting

| Symptom | Check |
|---|---|
| `ST_Tile` returns `NULL` | Verify pyramid name, zoom/x/y bounds, `minZoom`/`maxZoom`, `dynamicBeyondMaxZoom`, and whether the tile is actually empty. |
| Build is slow | Confirm a GiST index exists on the source geometry, reduce `maxZoom`, use LOD `buildRules`, or evaluate `preparedBuild` / `featureDrivenEnum`. |
| Repeated reads are slow | Check whether `splitSize > 0` or `dynamicBeyondMaxZoom` is causing live slicing. Add external caching or materialize more tiles. |
| Deletes or moves leave stale tiles | Use `ST_UpdatePyramidByExtent` over the old affected area or run a full rebuild. |
| Parallel build fails with tile budget | `tileSizeBudgetBytes` with `parallel > 1` is rejected; use serial budget enforcement or disable the byte budget. |
| Filter errors before build | Validate `buildRules.filter` against the source table and avoid semicolons or unsupported column references. |

## 16. Further reading

- [Quickstart](quickstart.md)
- [Pyramid lifecycle workflows](workflows/pyramid-lifecycle.md)
- [Config and LOD domain concepts](domain/config-and-lod.md)
- [Operations, testing, and benchmarks](operations/testing-and-benchmarks.md)
- [Source map](source-map.md)
