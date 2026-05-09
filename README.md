# geometry_pyramid

A PostgreSQL extension that builds **vector tile pyramids** over geometry
tables, exposing a SQL surface compatible with Alibaba Cloud
[GanosBase `ganos_geometry_pyramid`][ganos]. Built with
[pgrx](https://github.com/pgcentralfoundation/pgrx).

## Status

Working single-threaded build path: `ST_BuildPyramid` materializes MVT
tiles via PostGIS (`ST_TileEnvelope` + `ST_AsMVTGeom` + `ST_AsMVT`) into
per-pyramid storage tables, and `ST_Tile` serves them. Parallel build,
incremental updates, and PNG rendering are still on the roadmap.

## SQL surface

| Function | Purpose |
|---|---|
| `ST_BuildPyramid(table, geom, fid, config) -> bool` | Register / build a pyramid. |
| `ST_UpdatePyramid(name, where_clause) -> bool` | Refresh tiles for changed rows. |
| `ST_DropPyramid(name) -> bool` | Remove a pyramid and its tile storage. |
| `ST_Pyramid(name) -> jsonb` | Inspect a pyramid's metadata. |
| `ST_Tile(name, z, x, y) -> bytea` | Fetch an MVT tile (NULL on miss). |
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
| `tileSize` | `4096` | MVT extent. |
| `tileExtend` | `8` | Tile-edge buffer to avoid clipping artefacts. |
| `minZoom` / `maxZoom` | `0` / `18` | Zoom range to materialize. |
| `parallel` | `1` | Worker count for the build. |
| `sparse` | `true` | Skip empty tiles. |
| `attributes` | `[]` | Columns to include in MVT features. |
| `simplifyPixels` | `1.0` | DP tolerance in tile pixels (≈1 px ≈ tippecanoe default). 0 disables. |
| `minFeatureSizePixels` | `0.0` | Drop polygons whose area `<` and lines `<` this many tile pixels. 0 disables. |
| `dropRate` | `1.0` | Tippecanoe-style point retention: keep `dropRate^(maxZoom − z)`. 1.0 disables; 0.4 mimics `tippecanoe -r2.5`. |

## Project layout

```
.
├── Cargo.toml                   # crate config + pgXX feature flags
├── geometry_pyramid.control     # PG extension control file
├── rust-toolchain.toml          # pinned toolchain
├── .cargo/config.toml           # macOS dynamic_lookup linker flag
├── sql/bootstrap.sql            # catalog DDL + ST_Pyramids() (extension_sql_file!)
└── src/
    ├── lib.rs                   # pg_module_magic + module wiring + pg_test
    ├── config.rs                # PyramidConfig (JSON parsing)
    ├── storage.rs               # per-pyramid `_gp_tiles_<name>` table mgmt
    ├── build.rs                 # zoom-level INSERT pipeline (PostGIS via SPI)
    ├── pyramid.rs               # ST_BuildPyramid / ST_DropPyramid / ...
    └── tile.rs                  # ST_Tile
```

## Prerequisites

- Rust stable (see `rust-toolchain.toml`)
- PostgreSQL 13–18 dev headers (PostGIS recommended; declared in `.control`)
- `cargo-pgrx`:

```bash
cargo install --locked cargo-pgrx --version 0.16.0
cargo pgrx init --pg16=$(which pg_config)   # or download
```

## Build / run

```bash
cargo check                 # fast compile verification
cargo pgrx run pg16         # build, install, open psql
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

## Generation rules

For each zoom `z ∈ [minZoom, maxZoom]` the build pipeline does, in order:

1. **Bbox prefilter** in source SRS — `ST_Intersects(src.geom, ST_Transform(ST_TileEnvelope(z,x,y), sourceSRS))` so the source's GiST index drives the scan.
2. **Project to tile SRS** — `ST_Transform(geom, tileSRS)` (Web Mercator by default).
3. **Per-zoom simplification** *(tippecanoe-style)* — `ST_SimplifyPreserveTopology(g, tol_m)` where `tol_m = simplifyPixels × (40 075 016.685 / (tileSize × 2^z))`. Points are skipped.
4. **Min-feature-size filter** — drop polygons with `ST_Area < (minFeatureSizePixels × pixel_m)²`, lines with `ST_Length < minFeatureSizePixels × pixel_m`.
5. **Deterministic point dropping** *(tippecanoe `-r`)* — `(abs(hashtextextended(fid, 0)) % 1 000 000) / 1 000 000 < dropRate^(maxZoom − z)`. Same fid → same decision across rebuilds.
6. **Quantize to MVT grid** — `ST_AsMVTGeom(g, env, tileSize, tileExtend, true)` (clipping + integer quantization).
7. **Encode + sparse-store** — `ST_AsMVT(...)` aggregated per `(x,y)`, only non-empty tiles inserted into `_gp_tiles_<name>`.

## Roadmap

1. ~~Bootstrap — metadata catalog, SQL surface, config parsing.~~ ✅
2. ~~Tile codec — MVT bytes via PostGIS `ST_AsMVTGeom` / `ST_AsMVT`.~~ ✅
3. ~~Build pipeline — zoom-major INSERT into per-pyramid storage tables.~~ ✅
4. ~~Catalog views — `ST_Pyramids()`.~~ ✅
5. ~~Tippecanoe-style strategies — per-zoom simplification, min feature size, point drop rate.~~ ✅
6. **Tile-size budget** — `--drop-densest-as-needed` analogue: re-run a tile when `octet_length(mvt) > 500 KiB` with stricter thresholds.
7. **Coalesce-densest** — merge adjacent small polygons at low zoom (`ST_Union` / `ST_Buffer(0)`).
8. **Parallelism** — honor `parallel` config; partition tile range across
   workers using PG bgworkers or client-side parallel `ST_BuildPyramid`
   invocations.
9. **Incremental updates** — make `ST_UpdatePyramid(name, where_clause)`
   refresh only tiles whose source rows changed.
10. **Rendering** — optional `ST_AsPNG(name, tile, style_json)` for
    server-side raster output, matching Ganos.
11. **Statistics** — per-zoom row counts, byte size, build duration in
    `ST_Pyramid()` output.

[ganos]: https://www.alibabacloud.com/help/en/polardb/polardb-for-postgresql/vector-pyramid

## License

MIT (see `LICENSE`).
