# Changelog

All notable changes to `geometry_pyramid` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project targets pgrx 0.18
/ PostgreSQL 13–18 / PostGIS 3.1+ with MVT support.

## [0.1.2] - 2026-11-13

- Publish 30 native PostgreSQL 13–18 packages through GitHub Actions: Ubuntu 24.04 x86_64/ARM64, macOS 15 Intel/Apple Silicon, and Windows Server 2022 x86_64. PostgreSQL 13 and 14 packages are legacy compatibility builds; those server versions are end-of-life upstream.
- Include platform-aware installers, per-file and archive SHA-256 checksums, and a complete release manifest. Re-download published assets and verify they match the tested release set byte-for-byte.
- Test every final archive with fresh installation, real `0.1.0 → 0.1.2` and `0.1.1 → 0.1.2` upgrades, tile/grant preservation, and separate-session regression checks.
- Keep the SQL API and catalog unchanged from 0.1.1, with a no-op upgrade script for existing 0.1.1 databases. Correct the documented PostGIS minimum to 3.1 because tile buffering uses `ST_TileEnvelope`'s `margin` parameter.

## [0.1.1] - 2026-11-09

- Bind source and output relations by OID. Full rebuilds preserve the published table and its grants. Existing 0.1.0 installations require explicit relation bindings during upgrade (see README).
- Keep session-sensitive, uncommitted and small jobs local. Parallel builds check ownership before using shared scratch tables and publish complete worker status messages atomically.
- Align geographic and projected candidate filters, outer tile buffers and effective zoom budgets across full builds, refreshes and dynamic reads. Upsert statistics on the first refresh after an empty build.
- Prune regions below `splitSize` before clipping in tile-major builds. Accept the Ganos key `maxLevel` as an alias for `maxZoom`.
- Skip empty regions in default builds of sufficiently sparse indexed tables. Add opt-in `buildRules.pointGrid` for coarse point layers and warn when the byte budget remains exceeded after retries.
- Reuse simplification for complex features shared by a few tiles in opt-in serial builds. Skip the intermediate clipped rows for typed point tables using `pointGrid`.
- Run the requested benchmark case for `--small-parallel`. The synthetic `--sparse` suite runs independently of the imported fixtures.
- Verify the Ubuntu PG17 package on PRs, manual runs, and release tags. Test upgrades from `v0.1.0` and publish the verified archive on release tags.

## [0.1.0] - 2026-09-08

### Added

- **`buildRules` — per-level feature filtering (level-of-detail).** Each rule
  `{ minZoom, maxZoom, filter }` restricts a zoom band to source features
  satisfying `filter`, a raw SQL boolean expression over source columns. Zooms
  covered by no rule keep every feature; multiple rules covering a zoom are
  AND-ed. Empty (the default) is a no-op, byte-for-byte identical to prior
  output. Each filter is precomputed once into a boolean column in the prepared
  table and validated against the source before any build work, so an invalid
  predicate fails cleanly with no partial pyramid. Applies on all four build
  paths (baseline tile-major, prepared tile-major, point feature-major,
  feature-driven) and on the on-demand `ST_Tile` read path, keeping
  splitSize-deferred and beyond-`maxZoom` tiles consistent with materialized
  tiles. This is the lever for coarse-zoom build speed and storage: on a
  483k-feature line layer a graduated LOD cut a z0–12 build 2.4× (4m33s → 1m55s)
  and storage 83 → 60 MB, leaving deep-zoom detail unchanged.
- **Parallel build** across PostgreSQL background workers on Morton spatial
  blocks (`parallel` config key); `parallel: 0` auto-resolves to the host core
  count (capped). Over-requesting is safe — unspawnable slots run inline.
- **Opt-in fast build paths:** `preparedBuild` (pre-project the source into an
  indexed temporary table), `featureMajorPoints` (group points by tile id for
  pure-POINT sources), and feature-driven tile enumeration for all geometry
  types (`featureDrivenEnum`).

### Changed

- Build and dynamic tile SQL keep projected/simplified and clipped geometries
  in separate evaluation stages, avoiding repeated geometry work for null
  filtering and MVT aggregation and sorting smaller clipped records.
- Full builds use SPI's processed INSERT row counts instead of scanning the
  entire zoom before and after every block. Prepared builds derive the extent
  from the already-projected source; one-cell point zooms avoid point binning.
- Feature-driven strategy estimates now include the actual buffer and safety
  padding, clamp to the build rectangle, and ignore empty geometries.
- Builds whose maximum-zoom rectangle contains at most 64 cells use local
  preparation before any worker setup, since all their zooms already run inline.
  The requested catalog configuration and budget/parallel validation stay intact.

- **`splitSize` now defaults to `0`** (materialize every non-empty tile within
  `maxZoom`). Set `splitSize > 0` to defer sub-threshold tiles to on-demand
  slicing by `ST_Tile`. Reads are pure storage lookups at the default.

### Fixed

- Point feature-major builds include neighbouring tile buffers and exact
  boundary points, matching the normal prepared path's feature membership.
  Empty points are excluded before candidate enumeration.
- `ST_DropPyramid` no longer emits a spurious "does not exist, skipping" NOTICE.
