# Changelog

All notable changes to `geometry_pyramid` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project targets pgrx 0.18
/ PostgreSQL 17 / PostGIS 3.6.

## [Unreleased]

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
