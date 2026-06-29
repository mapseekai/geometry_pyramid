# geometry_pyramid

A PostgreSQL extension that builds **vector tile pyramids** over geometry
tables, exposing a SQL surface compatible with Alibaba Cloud
[GanosBase `ganos_geometry_pyramid`][ganos]. Built with
[pgrx](https://github.com/pgcentralfoundation/pgrx).

## Status

Working build path: `ST_BuildPyramid` materializes MVT tiles via PostGIS
(`ST_TileEnvelope` + `ST_AsMVTGeom` + `ST_AsMVT`) into per-pyramid storage
tables with **atomic rebuild** — tiles are written into a shadow table and
swapped in a single transaction so readers never see a half-built pyramid.
**Optional parallel build** honours the `parallel` config key by fanning out
zoom-level work across PostgreSQL background workers. `ST_Pyramid()` returns
**per-zoom build statistics** (row count, tile count, byte size, build
duration). `ST_UpdatePyramid` supports **incremental refresh**: when a
`where_clause` is provided, only tiles intersecting the matched rows are
rebuilt; omitting the clause triggers a full rebuild. `ST_Tile` is now
**hybrid**: tiles within `maxZoom` are served from materialized storage, while
requests beyond `maxZoom` are sliced on-demand from the source table (when
`dynamicBeyondMaxZoom` is enabled, the default), mirroring Ganos's `maxLevel`
semantics; unknown pyramids return NULL (so do `ST_Pyramid` / `ST_UpdatePyramid`
on an unknown name — cleanly, never an error). By default (`splitSize` 0) every
non-empty tile within `maxZoom` is materialized; raising `splitSize` defers
sparse tiles (feature count below it) to on-demand slicing by `ST_Tile` instead
of materializing them. **Per-level feature filtering** (`buildRules`,
level-of-detail) restricts coarse zooms to features matching a SQL predicate.
PNG rendering and coalesce-densest remain on the roadmap.

## SQL surface

| Function | Purpose |
|---|---|
| `ST_BuildPyramid(table, geom, fid, config) -> bool` | Register / build a pyramid. |
| `ST_UpdatePyramid(name, where_clause) -> bool` | Refresh only the tiles intersecting rows matched by `where_clause` (full rebuild when the clause is empty). |
| `ST_UpdatePyramidByExtent(name, xmin, ymin, xmax, ymax, srid) -> bool` | Re-slice every tile intersecting the bbox (given in `srid`) from the current source — correctly handles inserts/updates/deletes/moves in that region, unlike the where_clause path. |
| `ST_DropPyramid(name) -> bool` | Remove a pyramid and its tile storage. |
| `ST_Pyramid(name) -> jsonb` | Inspect a pyramid's metadata and per-zoom build statistics. |
| `ST_Tile(name, z, x, y) -> bytea` | Fetch an MVT tile: materialized within maxZoom, sliced on-demand beyond it (NULL when absent or disabled). |
| `ST_Pyramids() -> SETOF (name, source_table, built_at, tile_count)` | List all pyramids. |
| `geometry_pyramid_version() -> text` | Extension version. |

For the tile envelope use PostGIS's built-in `ST_TileEnvelope(z, x, y)`.

The `config` argument of `ST_BuildPyramid` is a JSON document. Recognized
keys mirror Ganos:

| Key | Default | Notes |
|---|---|---|
| `name` | _table name_ | Allow multiple pyramids on the same table. |
| `sourceSRS` | `4326` | SRID of the source `geom` column. |
| `tileSRS` | `3857` | Tile coordinate system (Web Mercator). |
| `tileSize` | `4096` | MVT extent (must be > 0). |
| `tileExtend` | `8` | Tile-edge buffer to avoid clipping artefacts. |
| `minZoom` / `maxZoom` | `0` / `18` | Zoom range to materialize. |
| `dynamicBeyondMaxZoom` | `true` | When a tile request has z > maxZoom, slice it live from the source table instead of returning NULL (mirrors Ganos maxLevel). Set false for a strict pyramid that returns NULL beyond maxZoom. |
| `parallel` | `1` | Worker count for the build; values > 1 fan out zoom-level work across PostgreSQL background workers. |
| `preparedBuild` | `false` | Opt-in first full-build speed path for `parallel=1`: pre-project source geometries into a session-local indexed temp table before slicing. With `parallel>1`, it falls back to the existing worker path. |
| `featureMajorPoints` | `false` | Experimental sparse path for pure `POINT` sources. Requires `preparedBuild=true`, `parallel=1`, and `tileSRS=3857`; the builder computes tile ids per point instead of probing every bbox tile. Mixed, line, polygon, or non-Web-Mercator builds fall back to prepared tile-major. |
| `featureMajorMinZoom` | `13` | Minimum `maxZoom` at which `featureMajorPoints` engages — it only wins once the high zoom levels (where tile-major's empty-cell probing explodes) dominate the build, and slightly regresses shallow pyramids. Below this, eligible point builds fall back to prepared tile-major (with a `NOTICE`). Set `0` to always use feature-major when eligible. |
| `featureDrivenEnum` | `false` | All-geometry generalization of `featureMajorPoints`: derive candidate tiles from each feature's envelope instead of enumerating + GiST-probing every bbox cell. Requires `preparedBuild=true`, `parallel=1`, `tileSRS=3857`; output is identical to the tile-major path. Self-gating — a one-time prep check disables it for dense extents or a few bbox-spanning features, and per zoom it engages only above the `R(z) > 2·rows` crossover (sparse / high zoom), so turning it on is always safe. Measured ~65× at z14 on a sparse mixed dataset. |
| `attributes` | `[]` | Columns to include in MVT features. |
| `simplifyPixels` | `1.0` | DP tolerance in tile pixels (≈1 px ≈ tippecanoe default). 0 disables. |
| `minFeatureSizePixels` | `0.0` | Drop polygons whose area `<` and lines `<` this many tile pixels. 0 disables. |
| `dropRate` | `1.0` | Tippecanoe-style point retention: keep `dropRate^(maxZoom − z)`. 1.0 disables; 0.4 mimics `tippecanoe -r2.5`. |
| `tileSizeBudgetBytes` | `0` | Per-tile MVT byte cap (`--drop-densest-as-needed` analogue). When breached, the zoom is wiped and rebuilt with tighter knobs. 0 disables; `512000` mimics tippecanoe. |
| `maxTileSizeAttempts` | `5` | Cap on retry rounds per zoom when chasing the byte budget. |
| `buildTimeoutMs` | `0` | Per-zoom upper bound (ms) the coordinator waits for parallel workers; 0 = unbounded. Only relevant when `parallel` > 1. |
| `splitSize` | `0` | `0` (default) materializes every non-empty tile. A value > 0 is a density boundary: tiles with fewer than this many features are not materialized at build time; `ST_Tile` slices them on-demand from the source on every request. |
| `buildRules` | `[]` | Per-level feature filters (LOD). Each rule `{minZoom, maxZoom, filter}` restricts that zoom band to source features satisfying `filter` (a raw SQL boolean expression over source columns). Zooms not covered by any rule include all features; multiple rules covering a zoom are ANDed. Empty (default) = no filtering. |

## Creating a pyramid

A pyramid is built from an existing PostGIS table with a geometry column and a
stable feature-id column. The full lifecycle is build → fetch → inspect →
update → drop.

**1. Enable the extensions** (once per database):

```sql
CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;
```

**2. Build a pyramid** with `ST_BuildPyramid(table, geom, fid, config)`. The
`config` is a JSON document (pass `'{}'` for all defaults); the keys are listed
in the table above. Say you have:

```sql
CREATE TABLE buildings (
    gid  serial PRIMARY KEY,
    name text,
    geom geometry(Polygon, 4326)
);
-- ... load rows, then ideally index the geometry so the build's && prefilter is fast:
CREATE INDEX ON buildings USING gist (geom);
```

Build a pyramid over it:

```sql
SELECT ST_BuildPyramid(
    'buildings',          -- source table
    'geom',               -- geometry column
    'gid',                -- stable feature-id column
    '{
       "name":       "buildings",
       "sourceSRS":  4326,
       "tileSRS":    3857,
       "minZoom":    0,
       "maxZoom":    14,
       "attributes": ["name"]
     }'
);
```

`name` lets several pyramids share one source table; it defaults to the table
name. `sourceSRS` must match the `geom` column's SRID. `attributes` are the
non-geometry columns carried into each MVT feature. Building is transactional —
tiles are written into a shadow table and swapped in atomically, so readers
never see a half-built pyramid (re-running `ST_BuildPyramid` rebuilds in place).

**3. Fetch a tile** as MVT bytes with `ST_Tile(name, z, x, y)`:

```sql
-- arguments are (name, z, x, y); returns bytea (MVT), or NULL when the tile is empty
SELECT ST_Tile('buildings', 14, 13488, 6210);
```

Tiles within `maxZoom` come from materialized storage; requests beyond
`maxZoom` are sliced live from the source when `dynamicBeyondMaxZoom` is on
(the default). See `demo/` for an end-to-end HTTP tile server + MapLibre viewer.

**4. Inspect** metadata and per-zoom build statistics:

```sql
SELECT ST_Pyramid('buildings');     -- jsonb: config, tile_count, per-zoom stats
SELECT * FROM ST_Pyramids();        -- list every pyramid
```

**5. Update** after the source changes — two complementary paths:

```sql
-- By predicate: rebuild only tiles intersecting the matched rows.
SELECT ST_UpdatePyramid('buildings', 'name = ''City Hall''');
-- Full rebuild when the clause is empty:
SELECT ST_UpdatePyramid('buildings', '');

-- By bbox (in the given SRID): re-slice the whole region from the CURRENT
-- source. Unlike the predicate path this correctly handles deletes and
-- cross-tile moves, since it does not depend on which rows a clause matches.
SELECT ST_UpdatePyramidByExtent('buildings', 116.30, 39.90, 116.42, 39.99, 4326);
```

**6. Drop** the pyramid and its tile storage:

```sql
SELECT ST_DropPyramid('buildings');
```

## Notes

Pyramid, table, and column identifiers are capped at **45 characters** so
that derived names (e.g. the `_gp_tiles_<name>__shadow` shadow table used
during atomic rebuild) stay within PostgreSQL's 63-byte identifier limit.

Each pyramid's tiles live in a table `_gp_tiles_<name>` with columns
`id bigserial, z smallint, x integer, y integer, format text, srid integer,
mvt bytea` and `PRIMARY KEY (z, x, y)`. `format` is currently always `'pbf'`;
`srid` is the tile coordinate system (`tileSRS`). Consumers may read this table
directly; `ST_Tile` returns only the `mvt` column.

> **Note on `splitSize`:** the default `0` materializes every non-empty tile within `maxZoom`, so reads are pure storage lookups. If you set `splitSize` > 0, any tile request within `maxZoom` that misses the materialized store — including empty regions — is sliced live from the source table on *every* request. That trades a faster, smaller build for slower repeated reads, so size it to your density and read load (and keep a spatial index on the source) before raising it above 0.

> **Note on `buildRules`:** rules implement level-of-detail — e.g. `[{"minZoom":0,"maxZoom":4,"filter":"scalerank < 5"}]` keeps only major features at coarse zooms. `filter` is a raw SQL boolean expression evaluated with the privileges of the `ST_BuildPyramid` caller (trusted-admin input, like a view definition); it is checked against the source before any build work and must not contain a statement separator (`;`). The `;` rule is a defense-in-depth check, not single-expression enforcement — under this posture an arbitrary predicate is already as privileged as a view definition. This is a deliberate, documented exception to parameterized-query guidance — an arbitrary predicate cannot be parameterized.

## Project layout

```
.
├── Cargo.toml                   # crate config + pgXX feature flags
├── geometry_pyramid.control     # PG extension control file
├── rust-toolchain.toml          # pinned toolchain
├── .cargo/config.toml           # macOS dynamic_lookup linker flag
├── sql/bootstrap.sql            # catalog DDL + ST_Pyramids() (extension_sql_file!)
├── demo/                        # HTTP tile server + offline MapLibre viewer (see demo/README.md)
└── src/
    ├── lib.rs                   # pg_module_magic + module wiring + pg_test
    ├── config.rs                # PyramidConfig (JSON parsing)
    ├── storage.rs               # per-pyramid `_gp_tiles_<name>` table mgmt
    ├── build.rs                 # zoom-level INSERT pipeline (PostGIS via SPI)
    ├── pyramid.rs               # ST_BuildPyramid / ST_DropPyramid / ...
    ├── tile.rs                  # ST_Tile
    ├── tile_rect.rs             # tile-index rectangle math (bbox→tiles, strip split)
    ├── stats.rs                 # per-zoom build statistics persistence
    ├── shadow.rs                # shadow-table lifecycle for atomic rebuild
    ├── worker.rs                # background-worker control plane (spec files + entry)
    ├── incremental.rs           # where_clause-scoped incremental refresh
    └── dynamic.rs               # on-demand single-tile slicing (z > maxZoom)
```

## Prerequisites

- Rust stable (see `rust-toolchain.toml`)
- PostgreSQL 13–18 dev headers (PostGIS recommended; declared in `.control`)
- `cargo-pgrx`:

```bash
cargo install --locked cargo-pgrx --version 0.18.0
cargo pgrx init --pg17=$(which pg_config)   # or download
```

## Build / run

```bash
cargo check                 # fast compile verification (default feature: pg17)
cargo pgrx run pg17         # build, install, open psql
cargo pgrx test             # run #[test] + #[pg_test]
cargo pgrx package          # produce installable artefact
```

Inside psql:

```sql
CREATE EXTENSION postgis;
CREATE EXTENSION geometry_pyramid;
SELECT ST_BuildPyramid('buildings', 'geom', 'gid', '{"maxZoom":14}');
SELECT ST_Pyramid('buildings');
```

## Build-speed benchmark

`bench/build_speed.sql` builds a deterministic mixed point/line/polygon dataset
and runs a first full `ST_BuildPyramid`, printing the geometry distribution, a
per-zoom `R(z)` tile-cell diagnostic, and per-zoom build stats. It is manual by
design (it creates many geometries and records wall-clock timing) and is the
baseline the build-speed work compares against.

```bash
psql "$DATABASE_URL" \
  -v pyramid_name=gp_bench_baseline \
  -v max_zoom=8 \
  -v scale=1000000 \
  -v prepared_build=false \
  -f bench/build_speed.sql

psql "$DATABASE_URL" \
  -v pyramid_name=gp_bench_prepared \
  -v max_zoom=8 \
  -v scale=1000000 \
  -v prepared_build=true \
  -f bench/build_speed.sql

psql "$DATABASE_URL" \
  -v pyramid_name=gp_bench_points_fm \
  -v max_zoom=8 \
  -v scale=1000000 \
  -v point_only=true \
  -v prepared_build=true \
  -v feature_major_points=true \
  -f bench/build_speed.sql
```

`scale` is the dense-point count (lines ≈ `scale/3`, polygons ≈ `scale/4`); raise
it to `1e7`/`1e8` on target hardware. `featureMajorPoints` is useful only with
`point_only=true` or another pure `POINT` source; mixed data deliberately falls
back to the prepared tile-major path.

## Generation rules

For each zoom `z ∈ [minZoom, maxZoom]` the build pipeline does, in order:

1. **Bbox prefilter** in source SRS — `src.geom && ST_Transform(ST_TileEnvelope(z,x,y, margin => tileExtend/tileSize), sourceSRS)` — an index-only `&&` against the buffer-expanded tile envelope, so the source's GiST index drives the scan and buffer-band edge geometry is included (seamless tile borders).
2. **Project to tile SRS** — `ST_Transform(geom, tileSRS)` (Web Mercator by default).
3. **Per-zoom simplification** *(tippecanoe-style)* — `ST_SimplifyPreserveTopology(g, tol_m)` where `tol_m = simplifyPixels × (40 075 016.685 / (tileSize × 2^z))`. Points are skipped.
4. **Min-feature-size filter** — drop polygons with `ST_Area < (minFeatureSizePixels × pixel_m)²`, lines with `ST_Length < minFeatureSizePixels × pixel_m`.
5. **Deterministic point dropping** *(tippecanoe `-r`)* — `(abs(hashtextextended(fid, 0)) % 1 000 000) / 1 000 000 < dropRate^(maxZoom − z)`. Same fid → same decision across rebuilds.
6. **Quantize to MVT grid** — `ST_AsMVTGeom(g, env, tileSize, tileExtend, true)` (clipping + integer quantization).
7. **Encode + sparse-store** — `ST_AsMVT(...)` aggregated per `(x,y)`, only non-empty tiles inserted into `_gp_tiles_<name>`.
8. **Tile-size budget retry** *(opt-in via `tileSizeBudgetBytes`)* — `MAX(octet_length(mvt))` for the zoom is compared to the budget; on overshoot, all four knobs are tightened by `factor = √(max/budget)` (simplify tolerance and min-size scaled up, retention scaled down, knobs that were "off" bootstrap to a 1-pixel baseline) and the zoom is wiped + rebuilt. Bounded by `maxTileSizeAttempts`.

## Roadmap

1. ~~Bootstrap — metadata catalog, SQL surface, config parsing.~~ ✅
2. ~~Tile codec — MVT bytes via PostGIS `ST_AsMVTGeom` / `ST_AsMVT`.~~ ✅
3. ~~Build pipeline — zoom-major INSERT into per-pyramid storage tables.~~ ✅
4. ~~Catalog views — `ST_Pyramids()`.~~ ✅
5. ~~Tippecanoe-style strategies — per-zoom simplification, min feature size, point drop rate.~~ ✅
6. ~~Tile-size budget — `--drop-densest-as-needed` analogue with geometric tightening.~~ ✅
7. **Coalesce-densest** — merge adjacent small polygons at low zoom (`ST_Union` / `ST_Buffer(0)`).
8. ~~**Parallelism** — honour `parallel` config; partition tile range across workers using PG background workers.~~ ✅
9. ~~**Incremental updates** — `ST_UpdatePyramid(name, where_clause)` rebuilds tiles intersecting matched rows.~~ ✅
   - **Limitation:** incremental refresh rebuilds the tiles that intersect the rows matched by the `where_clause`. A feature that moved or was deleted leaves a stale tile at its **old** location unless the clause also covers that region. Pure-delete and move-out cases require either a clause that covers the old bounding area or a full rebuild. `ST_UpdatePyramidByExtent(name, xmin, ymin, xmax, ymax, srid)` covers these cases: it re-slices the given region from the current source regardless of which rows changed.
10. **Rendering** — optional `ST_AsPNG(name, tile, style_json)` for
    server-side raster output, matching Ganos.
11. ~~**Statistics** — per-zoom row counts, byte size, build duration in `ST_Pyramid()` output.~~ ✅
12. ~~**On-demand slicing beyond maxZoom** — `ST_Tile` serves materialized tiles within `maxZoom`; requests with z > maxZoom are sliced live from the source table when `dynamicBeyondMaxZoom` is true (default), mirroring Ganos `maxLevel` semantics.~~ ✅
13. ~~**splitSize** — density boundary: sub-threshold tiles deferred to on-demand slicing.~~ ✅
14. ~~**buildRules** — per-level feature filtering (LOD): each `{minZoom, maxZoom, filter}` restricts a zoom band to source features matching a raw SQL predicate; zooms covered by no rule keep all features, and multiple rules covering a zoom are ANDed.~~ ✅

[ganos]: https://www.alibabacloud.com/help/en/polardb/polardb-for-postgresql/vector-pyramid

## License

MIT (see `LICENSE`).
