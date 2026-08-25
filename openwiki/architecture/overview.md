# Architecture Overview

## Runtime shape

`geometry_pyramid` is a PostgreSQL extension loaded through pgrx. `src/lib.rs` declares `pg_module_magic!`, wires module files, includes `sql/bootstrap.sql` with `extension_sql_file!`, and exposes `geometry_pyramid_version()`. Most user-facing SQL functions are implemented as `#[pg_extern]` functions in `src/pyramid.rs` and `src/tile.rs`.

The extension is intentionally a coordinator over PostGIS SQL:

1. pgrx exposes Rust functions as SQL functions.
2. Rust validates identifiers/configuration and builds SQL fragments.
3. SPI executes PostGIS queries for bboxing, transformation, simplification, clipping, MVT encoding, storage reads, and statistics.
4. PostgreSQL tables store catalog metadata, per-zoom stats, and per-pyramid MVT bytes.

Core source anchors:

- `src/lib.rs` — module wiring, SQL bootstrap, pg tests.
- `src/pyramid.rs` — lifecycle SQL functions and catalog lookup.
- `src/build.rs` — full build pipeline and zoom build orchestration.
- `src/tile.rs` / `src/dynamic.rs` — tile read and live slicing paths.
- `src/config.rs` — JSON config schema and validation.
- `sql/bootstrap.sql` — catalog/stats DDL and `ST_Pyramids()`.

## Persistent data model

`sql/bootstrap.sql` creates two shared extension tables:

- `geometry_pyramid_catalog(pyramid_name, source_table, geom_column, fid_column, config, built_at)` tracks registered pyramids.
- `geometry_pyramid_stats(pyramid_name, z, tile_count, total_bytes, max_tile_bytes, build_ms, attempts, built_at)` stores per-zoom build statistics.

`src/storage.rs` creates and drops per-pyramid tile tables named `_gp_tiles_<name>`. README documents columns as `id bigserial, z smallint, x integer, y integer, format text, srid integer, mvt bytea` with primary key `(z, x, y)`. `ST_Tile` returns only the `mvt` column.

A full rebuild uses `_gp_tiles_<name>__shadow` through `src/shadow.rs`. The build writes into shadow storage first, then swaps it into place; failed builds should leave the previous main table intact.

## Public SQL surface

Implemented in `src/pyramid.rs`, `src/tile.rs`, `src/lib.rs`, and `sql/bootstrap.sql`:

| Function | Implementation anchor | Notes |
|---|---|---|
| `ST_BuildPyramid(table, geom, fid, config)` | `src/pyramid.rs` | Parses and validates `PyramidConfig`, upserts catalog row, calls `build::build`. |
| `ST_UpdatePyramid(name, where_clause)` | `src/pyramid.rs`, `src/incremental.rs` | Empty clause = full rebuild; non-empty = dirty extent from matched rows. |
| `ST_UpdatePyramidByExtent(name, xmin, ymin, xmax, ymax, srid)` | `src/pyramid.rs`, `src/incremental.rs` | Bbox-defined refresh from current source data. |
| `ST_DropPyramid(name)` | `src/pyramid.rs`, `src/storage.rs`, `src/stats.rs`, `src/shadow.rs` | Deletes catalog/stat/storage/shadow state. |
| `ST_Pyramid(name)` | `src/pyramid.rs` | Metadata and per-zoom stats JSON. |
| `ST_Tile(name, z, x, y)` | `src/tile.rs`, `src/dynamic.rs` | Stored tile lookup plus dynamic fallback. |
| `ST_Pyramids()` | `sql/bootstrap.sql` | PL/pgSQL catalog listing with tile counts. |
| `geometry_pyramid_version()` | `src/lib.rs` | Returns Cargo package version. |

Unknown pyramids intentionally return `false` or `NULL` for several operations instead of raising user-facing storage errors (`README.md`, `src/pyramid.rs`, `src/tile.rs`).

## Build pipeline

`build::build(src)` is the full-build entrypoint (`src/build.rs`). Its major steps are:

1. Take a per-pyramid advisory transaction lock to serialize concurrent builders.
2. Reject unsupported `tileSizeBudgetBytes` + `parallel > 1` because budget retries wipe/rebuild zooms in ways that conflict with worker-committed strips.
3. Create a shadow table. In serial builds the shadow can live in the coordinator transaction; in parallel builds it must be created by an autonomous worker so background workers can see committed DDL.
4. Validate `buildRules` filters before substantial work.
5. Compute source bbox in tile SRS.
6. Optionally prepare a pre-transformed source table (`preparedBuild`) through `src/prepared.rs`.
7. Decide per build whether `featureMajorPoints` or `featureDrivenEnum` can engage.
8. For each zoom, derive a tile rectangle and call the zoom build path, optionally with budget retry.
9. Upsert per-zoom stats and swap the shadow table into place.

The tile SQL pipeline follows the README generation rules: bbox prefilter in source SRS, transform to tile SRS, per-zoom simplification, min-feature filtering, deterministic point dropping/sampling, `ST_AsMVTGeom`, `ST_AsMVT`, and sparse-store based on `splitSize`.

## Build path variants

The build system has several behavior-preserving paths selected by config and data shape:

- **Baseline tile-major**: enumerate tile cells in the source bbox and probe source geometries for each cell.
- **Prepared tile-major**: `preparedBuild:true` transforms/simplifies into a prepared table once, then runs tile-major against it (`src/prepared.rs`, `src/build.rs`).
- **Feature-major points**: `featureMajorPoints:true` for pure point sources with `preparedBuild`, `parallel=1`, `tileSRS=3857`, and sufficient `maxZoom`; computes tile ids arithmetically instead of probing empty cells.
- **Feature-driven enumeration**: `featureDrivenEnum:true` generalizes candidate tile derivation to all geometry envelopes and self-gates when it is not beneficial.
- **Parallel build**: `parallel > 1` fans tile strips to PostgreSQL background workers through files under `$PGDATA/gp_tmp/` (`src/worker.rs`). `parallel:0` auto-resolves to host parallelism capped in `src/config.rs`.

`docs/build-speed-deep-dive.md` explains why these paths exist: tile-major cost grows with the bbox tile lattice `R(z)`, while feature-derived paths win on sparse/high-zoom extents.

## Tile read pipeline

`ST_Tile(name, z, x, y)` in `src/tile.rs` is catalog-first:

1. Validate pyramid name and tile coordinate bounds.
2. Load catalog metadata and storage existence in one lookup via `pyramid::catalog_meta`.
3. Return `NULL` below `minZoom` or for invalid x/y.
4. Within `maxZoom`, read `_gp_tiles_<name>` when present.
5. If the stored read misses and `splitSize == 0`, return `NULL` because every non-empty tile should have been materialized.
6. If the stored read misses and `splitSize > 0`, call `dynamic::generate_tile` because sparse tiles may have been deferred.
7. Above `maxZoom`, call `dynamic::generate_tile` only when `dynamicBeyondMaxZoom` is true.

`src/dynamic.rs` uses the same `build::tile_pipeline_from_sql` machinery as full builds, including zoom-effective `buildRules`, so materialized and dynamic tiles should match LOD semantics. Dynamic tiles are read-only and uncached.

## Concurrency and transaction boundaries

Important design choices:

- Full serial builds rely on one transaction and shadow-table swap.
- Parallel workers are separate backends, so job specs and completion signals go through files in `$PGDATA/gp_tmp/`, not uncommitted catalog rows (`src/worker.rs`).
- Parallel shadow creation and committed prepared tables use autonomous worker execution because workers cannot see coordinator-uncommitted objects (`src/build.rs`, `src/prepared.rs`).
- Prepared committed tables are dropped in the coordinator transaction, not autonomously, to avoid a coordinator/worker lock wait deadlock described in `src/prepared.rs`.
- Incremental refresh runs in the caller transaction and rebuilds dirty rectangles in place, not through shadow swap (`src/incremental.rs`).

## Where the architecture is deliberately limited

- Source table, geometry column, fid column, and pyramid name are bare validated identifiers. Schema-qualified sources are not supported.
- Dynamic tile slicing is uncached.
- The SQL API is Ganos-inspired but not fully Ganos-compatible: current tile signature is `ST_Tile(name, z, x, y)`, not the documented Ganos `x,y,z` or key overload.
- Output format is 2D MVT only.
