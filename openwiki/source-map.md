# Source Map

Use this page for quick navigation before opening source files. For behavior details, prefer the dedicated pages linked from `quickstart.md`.

## Root metadata and docs

- `README.md` and `README.zh-CN.md` — user entry points with purpose, a runnable first tile, capabilities, and guide links.
- `openwiki/usage.md` and `openwiki/usage.zh-CN.md` — operator procedures and configuration reference.
- `Cargo.toml` — crate metadata, pgrx version, PostgreSQL feature flags, release profile.
- `geometry_pyramid.control` — PostgreSQL extension control metadata and PostGIS requirement.
- `sql/bootstrap.sql` — extension-owned catalog/stats DDL and `ST_Pyramids()`.
- `docs/build-speed-deep-dive.md` — performance rationale for prepared/feature-driven paths.
- `docs/user-data-build-performance.md` — benchmark summary for supplied user datasets.
- `docs/ganos-gap-analysis.md` — historical Ganos gap analysis; useful context, but its own header warns some status rows are stale.

## Rust extension modules

- `src/lib.rs` — pgrx module wiring, `geometry_pyramid_version()`, and most integration/pg tests.
- `src/config.rs` — `PyramidConfig`, defaults, validation, `buildRules`, and `parallel:0` auto-resolution.
- `src/pyramid.rs` — public lifecycle functions: build, update, bbox update, drop, inspect, catalog lookup, identifier validation.
- `src/source.rs` — source metadata passed between lifecycle/build/read paths.
- `src/build.rs` — full build coordinator, PostGIS tile SQL pipeline, LOD clauses, zoom stats, budget retry, and dispatch to build variants.
- `src/prepared.rs` — prepared source tables for serial and parallel-visible fast paths.
- `src/worker.rs` — PostgreSQL background worker job files, autonomous SQL execution, and parallel strip execution.
- `src/tile.rs` — `ST_Tile` lookup and dynamic fallback decision tree.
- `src/dynamic.rs` — uncached on-demand single-tile slicing for `splitSize` misses and beyond-`maxZoom` reads.
- `src/incremental.rs` — predicate and bbox dirty-region refresh.
- `src/storage.rs` — per-pyramid `_gp_tiles_<name>` storage table lifecycle.
- `src/shadow.rs` — ownership checks and cleanup for worker staging tables.
- `src/stats.rs` — per-zoom statistics persistence and retrieval.
- `src/tile_rect.rs` — tile rectangle math and partition helpers.

## Demo and benchmarks

- `demo/server.py` — local HTTP wrapper around `ST_Tile`; not production serving code.
- `demo/index.html` — MapLibre viewer with local vector tile layer and opt-in online basemap.
- `demo/test_server.py` — Python unit tests for parser, handler, static viewer expectations, and connection guard behavior.
- `demo/seed.sql`, `demo/README.md`, and `demo/README.zh-CN.md` — local demo database setup.
- `bench/*.sql` — manual performance scripts. Start with `bench/build_speed.sql` for synthetic baseline or `bench/user_data_perf.sql` for the current user-data matrix.

## Where to start for common changes

- Add or change config keys: `src/config.rs` → `src/build.rs` / `src/prepared.rs` / `src/dynamic.rs` → usage guide config table → tests in `src/lib.rs`.
- Change tile build output: `src/build.rs` first, then verify prepared, feature-major, feature-driven, dynamic, and parallel paths still match.
- Change tile reads: `src/tile.rs` and `src/dynamic.rs`; check split-size and beyond-maxZoom behavior.
- Change update semantics: `src/pyramid.rs` and `src/incremental.rs`; check stale-delete/move caveats.
- Change parallel behavior: `src/build.rs`, `src/prepared.rs`, `src/worker.rs`; watch transaction visibility and worker-committed rows.
- Change demo behavior: `demo/server.py`, `demo/index.html`, and `demo/test_server.py`.
