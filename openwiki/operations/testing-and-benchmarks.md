# Develop, test, and benchmark

[简体中文](testing-and-benchmarks.zh-CN.md) · [Usage guide](../usage.md)

Run these commands from the repository root. The default Cargo feature is
PostgreSQL 17 and the pgrx version is 0.18.0. Use a PostgreSQL 17 `pg_config`;
another PostgreSQL version on `PATH` is not a substitute.

## Check a source change

```bash
cargo install --locked cargo-pgrx --version 0.18.0
cargo pgrx init --pg17 /path/to/postgresql-17/bin/pg_config
cargo check
cargo pgrx test pg17
python3 demo/test_server.py
```

`cargo pgrx test pg17` runs Rust and PostgreSQL integration tests, including
PostGIS-backed `#[pg_test]` cases. The Python tests exercise demo parsing and
HTTP behavior without a live database; install `psycopg` to run them.

For an interactive SQL check, `cargo pgrx run pg17` builds and installs the
extension, starts managed PostgreSQL, and opens `psql`. Enable PostGIS before
`geometry_pyramid` in the target database. The [README example](../../README.md#try-one-tile)
builds and checks one tile.

## Build a package

```bash
cargo pgrx package --pg-config /path/to/postgresql-17/bin/pg_config --out-dir package
```

The repository's [Verify and release workflow](../../.github/workflows/release.yml)
tests PostgreSQL 17 on Ubuntu 24.04, builds this package, and checks fresh
installation and upgrade from 0.1.0. Cargo has features for PostgreSQL
13 through 18; this workflow verifies the PostgreSQL 17 package.

## Run the viewer

Follow the [demo runbook](../../demo/README.md) to seed a dedicated database,
start the HTTP server, inspect tile responses, and open the MapLibre viewer.
The local server uses one synchronous database connection. Its default port
is 8989; `PORT` changes it. The OSM basemap is optional and off by default.

## Measure build speed

`bench/build_speed.sql` creates and drops `_gp_bench_mixed` and the selected
pyramid, so use a **disposable benchmark database**. Set `DATABASE_URL` to
that database. Start with a small scale to check setup, then choose a
representative workload and repeat measurements on the target hardware:

```bash
psql "$DATABASE_URL" \
  -v pyramid_name=gp_bench_baseline \
  -v max_zoom=8 \
  -v scale=1000 \
  -v prepared_build=false \
  -f bench/build_speed.sql
```

The script reports source geometry distribution, tile-cell counts by zoom,
wall time, and persisted per-zoom statistics. For a comparable prepared run,
change the pyramid name and set `-v prepared_build=true`; keep the other
inputs and environment fixed. The larger `bench/user_data_*.sql` scripts
and [measured results](../../docs/user-data-build-performance.md) cover real
source shapes. Do not transfer a speedup from one dataset to another without
rerunning the relevant case.

## Operational checks

- Full rebuilds replace rows in the published tile table transactionally.
  Readers do not see partial results; the table keeps its grants and views.
- Predicate refresh uses currently matched rows. For deletes or moves,
  refresh an extent covering the old and new locations, or rebuild fully.
- Deferred or above-`maxZoom` tiles are sliced live without an extension
  cache. Monitor source query load and add a serving cache when needed.
- Parallel builds use PostgreSQL background workers and files under
  `$PGDATA/gp_tmp/`. `tileSizeBudgetBytes` with `parallel > 1` is rejected.

For API behavior, configuration, upgrade, and symptom-based troubleshooting,
see the [usage guide](../usage.md).
