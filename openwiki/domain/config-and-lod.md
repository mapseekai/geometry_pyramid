# Config and LOD Domain Concepts

## Configuration shape

`ST_BuildPyramid(table, geom, fid, config)` accepts a JSON document parsed into `PyramidConfig` (`src/config.rs`). Empty config means defaults. The public option table in `README.md` is the best user-facing reference; the source is the validation authority.

Core groups:

- **Identity and coordinate systems**: `name`, `sourceSRS`, `tileSRS`, `tileSize`, `tileExtend`, `minZoom`, `maxZoom`.
- **Read/build shape**: `dynamicBeyondMaxZoom`, `splitSize`, `parallel`, `buildTimeoutMs`.
- **Build-speed paths**: `preparedBuild`, `featureMajorPoints`, `featureMajorMinZoom`, `featureDrivenEnum`.
- **Generalization and size controls**: `attributes`, `simplifyPixels`, `minFeatureSizePixels`, `dropRate`, `tileSizeBudgetBytes`, `maxTileSizeAttempts`.
- **Level-of-detail rules**: `buildRules`.

Validation enforces `minZoom <= maxZoom`, `maxZoom <= 24`, `0 < tileSize <= 16384`, `tileExtend < tileSize`, valid rule identifiers, compatible rule attribute overrides, and sampling ranges (`src/config.rs`). Identifier-like config fields are deliberately bare identifiers; schema-qualified source names are not part of the current model.

## Build rules: one native LOD surface

`buildRules` is the repository's single level-of-detail surface. Each rule has the native shape:

```json
{
  "minZoom": 0,
  "maxZoom": 4,
  "filter": "scalerank < 5",
  "attrFields": ["name", "kind"],
  "sample": { "rate": 25, "key": ["fid"], "geometryTypes": ["POINT"] }
}
```

Semantics from `README.md`, `src/config.rs`, and `src/build.rs`:

- A rule covers the inclusive zoom band `[minZoom, maxZoom]`.
- `filter` is a raw SQL boolean expression evaluated against source columns.
- `attrFields` overrides the emitted MVT attributes for the covered zooms.
- `sample.rate` is a percentage from `0` to `100`; `sample.key` defaults to `["fid"]` and drives deterministic hashing; `sample.geometryTypes` can restrict sampling by geometry family.
- Multiple rules covering the same zoom are combined by AND for filtering/sampling. If multiple covering rules set `attrFields`, they must agree for that zoom.
- Zooms not covered by any rule use all source features and the global `attributes` list.

The extension intentionally does **not** implement a second Ganos-native `level` / `value` rule shape. Historical Ganos comparison docs may mention missing LOD features; prefer current `README.md` and `src/config.rs` when they conflict.

## Where LOD rules apply

Recent buildRules work made rule behavior consistent across build/read variants:

- Baseline tile-major builds apply zoom-effective rule clauses in `src/build.rs`.
- Prepared builds project needed rule data into prepared tables in `src/prepared.rs` and apply prepared-rule clauses from `src/build.rs`.
- `featureMajorPoints` and `featureDrivenEnum` have tests that verify filtered output (`src/lib.rs`).
- Dynamic reads (`splitSize` misses and `z > maxZoom`) call `dynamic::generate_tile`, which uses the same tile pipeline and zoom-effective `buildRules` clauses (`src/dynamic.rs`).
- Parallel worker jobs carry the full `PyramidConfig` and worker-visible prepared table information so worker strips use the same selected build path (`src/worker.rs`).

When changing this area, check both source and prepared rule targets in `src/build.rs`; a fix in only one path can make stored and dynamic or serial and parallel output diverge.

## Security posture for filters

`buildRules.filter` is trusted-admin SQL, analogous to a view definition. It is evaluated with the caller's privileges and cannot be parameterized as a normal value expression (`README.md`, `src/config.rs`). The code rejects semicolons as a single-statement guard and probes filters before substantial build work, but that is defense-in-depth, not a SQL sandbox (`src/build.rs`).

Do not expose filter-bearing build APIs through `SECURITY DEFINER` wrappers or to roles that should not be allowed to run arbitrary predicates over the source tables.

## Performance knobs and tradeoffs

- `splitSize:0` materializes every non-empty tile within `maxZoom`; reads are fastest and misses return `NULL` without live slicing.
- `splitSize > 0` skips sparse tiles during build, reducing build/storage work but making each later miss run a live source-table slice. Keep a GiST index on the source when using it (`src/tile.rs`, `src/dynamic.rs`).
- `preparedBuild:true` pre-transforms source geometry; in parallel builds the prepared table is committed and worker-visible (`src/prepared.rs`).
- `featureMajorPoints:true` is only for pure point sources, `tileSRS=3857`, `preparedBuild:true`, `parallel=1`, and sufficiently deep pyramids (`featureMajorMinZoom`).
- `featureDrivenEnum:true` is the all-geometry sparse/high-zoom path. It self-gates by comparing feature-envelope tile pairs and per-zoom lattice size, falling back when dense enumeration is cheaper (`src/build.rs`).
- `tileSizeBudgetBytes` is an output-protection loop that tightens simplification/min-size/drop thresholds and rebuilds a zoom. It is rejected with `parallel > 1` because worker-committed rows and coordinator retry deletes do not compose safely (`src/build.rs`).

`docs/user-data-build-performance.md` found `buildRules.sample` was the strongest coarse-zoom lever for complex LUCC polygons in the inspected benchmark set, while `tileSizeBudgetBytes` primarily protected tile size at significant build-time cost.

## Change guidance

- Start with `src/config.rs` for schema/validation changes, then inspect every caller in `src/build.rs`, `src/prepared.rs`, `src/dynamic.rs`, and `src/worker.rs`.
- Add or update pg tests in `src/lib.rs` for each build path touched: baseline, prepared, feature-major points, feature-driven, dynamic, and parallel when relevant.
- Keep README examples and this page aligned with the actual native JSON shape.
- Treat `docs/ganos-gap-analysis.md` as historical context; its header says some status rows are stale.
