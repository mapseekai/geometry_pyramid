# geometry_pyramid OpenWiki Quickstart

## What this repository is

`geometry_pyramid` is a Rust/pgrx PostgreSQL extension that builds and serves PostGIS-backed vector tile pyramids. Its public API is modeled on Alibaba Cloud GanosBase `ganos_geometry_pyramid`: source geometry tables are registered with `ST_BuildPyramid`, materialized MVT tiles are stored in per-pyramid tables, and `ST_Tile` fetches stored or dynamically sliced tiles. The extension delegates geometry and MVT work to PostGIS via SPI (`ST_TileEnvelope`, `ST_AsMVTGeom`, `ST_AsMVT`) rather than implementing a tile codec in Rust.

Primary evidence: `README.md`, `Cargo.toml`, `src/lib.rs`, `src/pyramid.rs`, `src/build.rs`, `src/tile.rs`, `sql/bootstrap.sql`.

## Current implementation at a glance

- SQL lifecycle: `ST_BuildPyramid`, `ST_UpdatePyramid`, `ST_UpdatePyramidByExtent`, `ST_DropPyramid`, `ST_Pyramid`, `ST_Tile`, `ST_Pyramids`, and `geometry_pyramid_version` (`README.md`, `src/pyramid.rs`, `src/tile.rs`, `src/lib.rs`, `sql/bootstrap.sql`).
- Build output: per-pyramid `_gp_tiles_<name>` tables with MVT `bytea`, plus `geometry_pyramid_catalog` and per-zoom `geometry_pyramid_stats` (`sql/bootstrap.sql`, `src/storage.rs`, `src/stats.rs`).
- Atomic full rebuilds: builds replace published tile rows in one transaction, preserving the table OID and grants. Parallel builds use a staging table (`src/build.rs`).
- Hybrid tile reads: `ST_Tile` uses materialized storage within `maxZoom`, slices sparse `splitSize` misses dynamically, and can slice beyond `maxZoom` when `dynamicBeyondMaxZoom` is enabled (`src/tile.rs`, `src/dynamic.rs`).
- Performance paths: baseline tile-major, `preparedBuild`, pure-point `featureMajorPoints`, all-geometry `featureDrivenEnum`, and background-worker parallel builds (`src/build.rs`, `src/prepared.rs`, `src/worker.rs`, `docs/build-speed-deep-dive.md`).
- LOD/build rules: recent work added one project-native `buildRules` surface with per-zoom `filter`, `attrFields`, and deterministic `sample`; overlapping rules are ANDed (`src/config.rs`, `src/build.rs`, `CHANGELOG.md`, recent git log).

## Start here as an engineer

### Build and test locally

```bash
cargo check
cargo pgrx run pg17
cargo pgrx test pg17
cargo pgrx package --pg-config /path/to/postgresql-17/bin/pg_config --out-dir package
```

The crate defaults to PostgreSQL 17 through Cargo features (`Cargo.toml`) and targets pgrx `0.18.0`. Initialize `cargo-pgrx` with a PostgreSQL 17 `pg_config` and install PostGIS first; see the [README](../README.md#try-one-tile).

### Minimal SQL lifecycle

Run the [README's self-contained SQL example](../README.md#try-one-tile)
for a first build and tile read, then use the [usage guide](usage.md) for
refresh and removal.

Identifier inputs are intentionally restricted to short bare identifiers because many paths interpolate validated identifiers into dynamic SQL (`src/pyramid.rs`, `src/config.rs`). Schema-qualified source tables are not yet supported.

## Documentation map

- [中文快速开始](quickstart.zh-CN.md) — Chinese entrypoint for the same repository wiki.
- [Usage guide](usage.md) / [中文](usage.zh-CN.md) — standard user guide for enabling the extension, preparing data, building, reading, refreshing, configuring, and troubleshooting pyramids.
- [Architecture overview](architecture/overview.md) / [中文](architecture/overview.zh-CN.md) — module responsibilities, SQL entrypoints, build/read pipelines, storage, and concurrency model.
- [Pyramid lifecycle workflows](workflows/pyramid-lifecycle.md) / [中文](workflows/pyramid-lifecycle.zh-CN.md) — build, tile fetch, update, drop, and demo workflows with caveats.
- [Config and LOD domain concepts](domain/config-and-lod.md) / [中文](domain/config-and-lod.zh-CN.md) — `PyramidConfig`, `buildRules`, sampling, dynamic tiles, and performance knobs.
- [Operations, testing, and benchmarks](operations/testing-and-benchmarks.md) / [中文](operations/testing-and-benchmarks.zh-CN.md) — pgrx commands, demo server, benchmark SQL, user-data findings, and OpenWiki workflow.
- [Source map](source-map.md) / [中文](source-map.zh-CN.md) — practical file-by-file navigation for future agents.

## Safety and security posture

- `buildRules.filter` and `ST_UpdatePyramid(where_clause)` accept raw SQL predicates by design. The code treats them as trusted-admin input evaluated with the caller's privileges, analogous to a view definition. Do not wrap these functions in `SECURITY DEFINER` wrappers or expose them to lower-privilege roles (`README.md`, `src/pyramid.rs`, `src/config.rs`).
- The semicolon check in `buildRules.filter` is defense-in-depth, not full SQL sanitization (`src/config.rs`). Invalid predicates are probed before build work so failures should not leave partial pyramids (`src/build.rs`).
- Dynamic SQL identifiers are guarded by validation (`src/pyramid.rs`, `src/config.rs`, `src/dynamic.rs`). Keep this invariant when adding new SQL-generating code.

## Backlog

- Coalesce/merge LOD — source anchor: `docs/ganos-gap-analysis.md`; no current implementation was found.
- Ganos compatibility aliases/signatures — source anchor: `docs/ganos-gap-analysis.md`; `ST_Tile(name,'z_x_y')`, Ganos `x,y,z` order, and `ST_DeletePyramid` remain compatibility gaps.
- PNG and 3D tile outputs — source anchor: `docs/ganos-gap-analysis.md`; repository currently focuses on 2D MVT.
- Production tile serving/caching — source anchor: `demo/server.py`, `src/dynamic.rs`; demo server is explicitly local/acceptance only and dynamic slicing is uncached.
- Schema-qualified source tables — source anchor: `src/pyramid.rs` identifier validation and gap-analysis docs; only bare identifiers are currently accepted.
