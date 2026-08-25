# Operations, Testing, and Benchmarks

## Local build and pgrx checks

The crate targets pgrx `0.18.0` and defaults to PostgreSQL 17 via Cargo features (`Cargo.toml`). README setup notes cover PostgreSQL 13-18 feature flags and `cargo-pgrx` initialization.

Common commands:

```bash
cargo check
cargo pgrx run pg17
cargo pgrx test
cargo pgrx package
```

PostGIS is required before exercising the SQL API:

```sql
CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;
```

Use `cargo pgrx test` for both regular Rust tests and `#[pg_test]` coverage in `src/lib.rs`. Recent tests cover buildRules across dynamic reads, prepared multi-rule output, feature-major points, feature-driven builds, serial/parallel equivalence, and invalid-filter cleanup.

## Demo server and viewer

`demo/` is a local acceptance harness, not production infrastructure. `demo/server.py` serves:

- `/` and static MapLibre assets from `demo/`.
- `/tiles/{name}/{z}/{x}/{y}.pbf`, which calls `SELECT ST_Tile(%s, %s, %s, %s)` and returns HTTP 204 for SQL NULL or empty bytes.

The server is synchronous and keeps one module-level psycopg connection. Current code reconnects once after a psycopg error, rejects non-identifier tile names in the URL parser, catches common client-disconnect write errors, and requires the connection to be initialized before `_fetch_tile` runs (`demo/server.py`). Tests for these behaviors live in `demo/test_server.py`.

Typical local flow from `demo/README.md` and `workflows/pyramid-lifecycle.md`:

```bash
CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx install --release \
  --pg-config /opt/homebrew/opt/postgresql@17/bin/pg_config
CARGO_TARGET_DIR=/tmp/gp_target cargo pgrx start pg17

createdb -h localhost -p 28817 geometry_pyramid
psql -h localhost -p 28817 -d geometry_pyramid -f demo/seed.sql

DATABASE_URL="host=localhost port=28817 dbname=geometry_pyramid" python3 demo/server.py
```

The static viewer now includes an opt-in/toggleable online OpenStreetMap basemap in `demo/index.html`; tests assert it is not enabled by default (`demo/test_server.py`).

## Benchmark scripts

Manual SQL benchmarks live under `bench/`. They are intended for psql against a pgrx/PostGIS database and often create/drop benchmark schemas or pyramids.

Key files:

- `bench/build_speed.sql` — deterministic mixed point/line/polygon build-speed baseline described in README.
- `bench/user_data_perf.sql` and `bench/user_data_perf_followup.sql` — user-data import/performance matrix for points, building polygons, derived lines, LUCC, and mixed data.
- `bench/user_data_lucc_optimization.sql`, `bench/user_data_lucc_sampling.sql`, `bench/user_data_mixed_optimization.sql`, `bench/user_data_next_optimization.sql`, `bench/user_data_point_sampling.sql`, `bench/user_data_split_z8.sql`, `bench/user_data_budget_followup.sql`, and `bench/user_data_tile_profile_parallel.sql` — follow-up optimization and profiling scenarios.

`docs/user-data-build-performance.md` summarizes the inspected user-data runs. High-signal takeaways:

- Dense-looking source row counts are not the only driver; complex polygon clipping/MVT encoding can dominate even with few output tiles.
- `preparedBuild` was neutral to mildly helpful on the measured concentrated datasets.
- `featureMajorPoints` did not help shallow/concentrated z0-10 point data because only a small tile set was generated.
- `buildRules.sample` was the strongest LUCC coarse-zoom optimization in the measured set.
- `tileSizeBudgetBytes` is best viewed as output protection, not first-choice build acceleration.
- `parallel=4` did not help the inspected few-heavy-tile workload; mixed LOD regressed in that benchmark.

Use these findings as starting hypotheses, not universal performance guarantees. Re-run the relevant script on target data and hardware before recommending defaults.

## Operational watchpoints

- Full rebuilds are atomic through shadow table swap, but incremental refresh edits dirty regions in place (`src/build.rs`, `src/shadow.rs`, `src/incremental.rs`).
- `ST_UpdatePyramid(where_clause)` cannot clean stale old locations for deletes or move-outs unless the clause still covers them. Prefer `ST_UpdatePyramidByExtent` or full rebuild for those cases.
- Dynamic slicing is uncached; repeated `splitSize` or beyond-`maxZoom` reads can become live PostGIS workload (`src/dynamic.rs`).
- Parallel builds use background workers and files under `$PGDATA/gp_tmp/` for jobs/status because workers cannot see coordinator-uncommitted state (`src/worker.rs`).
- `tileSizeBudgetBytes` with `parallel > 1` is rejected by design (`src/build.rs`).

## Change guidance

- For SQL API or build-pipeline changes, run `cargo pgrx test` when possible and add `#[pg_test]` coverage in `src/lib.rs` for user-visible behavior.
- For demo changes, run `python3 demo/test_server.py`; these tests avoid requiring a live database for parser/static/handler checks.
- For performance changes, prefer adding a focused `bench/*.sql` script or extending an existing one, then summarize results in `docs/user-data-build-performance.md` rather than in the wiki.
