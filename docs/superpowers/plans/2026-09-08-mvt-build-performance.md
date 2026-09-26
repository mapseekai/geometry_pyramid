# MVT build performance implementation plan

> **For agentic workers:** Use superpowers:subagent-driven-development for isolated test/review work; execute the shared SQL edits serially.

**Goal:** Improve construction speed without silently dropping features or changing the configured LOD.

**Architecture:** Retain the four existing build paths and their shared dynamic/incremental SQL. Add bounded evaluation barriers around expensive geometry expressions, remove redundant scans, and make candidate generation/selection honor buffer coverage. Existing opt-in LOD remains the quality-control surface.

**Tech Stack:** Rust, pgrx 0.18.0, PostgreSQL 17 test target, PostGIS.

**Spec:** `docs/mvt-performance-review-2026-09-08.md`, approved by the user in this task.

## Constraints

- Preserve fid, selected attributes, geometry and buffer membership across build paths; aggregate order need not be byte-identical.
- Preserve dynamic reads, incremental updates, splitSize, budget semantics and worker visibility.
- No new dependency or default lossy sampling/generalization.
- Run GitNexus impact before changing existing symbols. Test baseline, regressions and full extension suite; measure release builds.

## Execution

- [x] Capture existing test and benchmark baseline; add boundary/geometry regressions that fail first.
- [x] Keep source/prepared tile-major geometry evaluation once per candidate, with bounded per-tile materialization. Keep feature-driven prepped and clipped records in separate OFFSET 0 evaluation stages to avoid repeated simplify/clip after fanout.
- [x] Fix point candidate enumeration for buffer and exact boundaries; keep arithmetic feature-major execution and no bbox lattice.
- [x] Replace INSERT-before/after full-zoom counts with SPI processed rows; perform final zoom stats only once.
- [x] Build prepared source before computing its extent, avoiding a second source projection pass and retaining empty-source cleanup.
- [x] Account for real buffer/padding in feature-driven candidate estimates and test strategy selection.
- [x] Evaluate per-zoom source reuse/strategy and parallel setup overhead; keep only measured improvements consistent with existing output semantics.
- [x] Run mixed geometry/LOD/dynamic/incremental/parallel regressions, release benchmarks, review, and document measured changes plus remaining experimental directions.

## Validation details

Root owns `src/build.rs`, any necessary `src/prepared.rs` changes, benchmark evidence and docs. Test worker owns additions to `src/lib.rs` only. Baseline command: `rtk proxy cargo pgrx test pg17 --release`. Run named regressions first, then the full suite. Performance checks use the same source snapshot and config before/after, warm-up plus at least three timed runs, compare decoded MVT feature content, and report preprocessing separately from per-zoom time.

Ruling: use the current checkout on branch `zwishing/mvt-build-performance` so the user's analysis artifact and existing pgrx build cache remain directly usable; no merge/push or deployment is part of this task.

Ruling: persistent LOD caches, topology-changing aggregation, partial-tile budget quality policies, and a worker-pool redesign are conditional follow-ups, not default behavior changes. First eliminate confirmed waste, then use measured results to choose further changes.

Completed: 124 release PG17 tests passed; independent core review clean. Twelve immutable-library benchmark cases retained exact decoded before/after content. Measured results and limitations: `docs/mvt-performance-results-2026-09-08.md`. Normal release extension installed locally; no merge or push.
