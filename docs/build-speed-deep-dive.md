# Build-speed deep dive

Measured analysis of the three pyramid build paths and the optimizations behind
them. All numbers from **PostgreSQL 17.10 / PostGIS 3.6.4** on Apple aarch64,
`geometry_pyramid` 0.1.0.

## The three build paths

| Path | Config | What it does |
|------|--------|--------------|
| **baseline (tile-major)** | default | Per zoom, enumerate every tile cell in the data bbox (`generate_series` x/y lattice) and `LATERAL`-join the source geometry intersecting each, transforming + simplifying on the fly. |
| **prepared-build** | `preparedBuild:true` | Transform + simplify the source geometry **once** into a GiST-indexed prepared table, then run the same tile-major loop against it (no per-zoom re-transform). Requires `parallel=1`. |
| **feature-major points** | `preparedBuild:true, featureMajorPoints:true, tileSRS:3857` | For **pure POINT** sources, compute each point's tile `(x,y)` by arithmetic (`floor((X+offset)/(extent/2^z))`) instead of enumerating bbox cells. No empty-cell probing. |

These compose: feature-major builds on top of prepared. They are **orthogonal
savings** — prepared removes repeated transform/simplify (all geometry, all
zooms); feature-major removes empty-tile-cell enumeration (points, high zoom).

## Headline numbers

100k-point dataset (+ scattered outliers widening the bbox), mixed = points +
lines + polygons over the same area. `splitSize=1`, `tileSRS=3857`. **All paths
produced byte-identical tile counts** (337 tiles @maxZoom=12, 855 @maxZoom=14) —
the optimizations are behavior-preserving.

**Points, `maxZoom=14`:**

| Path | Total wall-clock | vs best |
|------|------------------|---------|
| feature-major points | **2.9 s** | 1.00x |
| prepared-build | 8.8 s | 3.05x |
| baseline (tile-major) | 17.1 s | **5.92x** |

**Mixed, `maxZoom=14`** (feature-major N/A — has lines/polygons):

| Path | Total wall-clock | vs best |
|------|------------------|---------|
| prepared-build | **14.7 s** | 1.00x |
| baseline (tile-major) | 20.9 s | 1.42x |

## Why: per-zoom cost shape (points)

The total hides the real story. Per-zoom `build_ms`:

```
 z   baseline   prepared   feature-major
 7      175        121        147
10      225        270        359     <- crossover region
12      943        420        201
13     2820       1339        221
14    10801       5187        181     <- feature-major ~60x faster than baseline
```

- **feature-major is ~flat across zoom** (~150-360 ms/zoom): it is **O(features)**.
  Each point is binned to its tile by arithmetic; no dependence on zoom.
- **both tile-major paths grow ~4x per zoom** above ~z11: they are **O(R(z))**,
  where `R(z)` = number of tile cells in the data bbox ~ `4^z`. Each zoom
  enumerates the full lattice and GiST-probes every cell, **most of them empty**.
  At z14 the baseline burns 10.8 s on that single level.
- **Crossover is ~z10-11.** Below it, feature-major is slightly *slower* — it
  pays a fixed per-feature binning cost at every zoom even when all points fall
  in one tile, while tile-major's `R(z)` is still tiny.

`prepared` beats `baseline` at **every** zoom (e.g. z14: 5187 vs 10801 ms) — it
still pays `R(z)`, but each per-cell op works on pre-transformed, pre-simplified
geometry. Its relative win grows with zoom (1.12x @z12 -> 1.42x @z14 on mixed).

### The total-vs-per-zoom trap

At `maxZoom=12` feature-major does **not** win the total (FM 2.65 s vs prepared
2.30 s) — the many cheap shallow levels, where FM is the slower path, dominate
the sum. Only at `maxZoom>=~13` do the explosive top levels dominate and FM pull
ahead. Judging by total alone would mis-rank them; the per-zoom curve is what
reveals that FM's entire value lives in the last few zoom levels.

## `featureMajorMinZoom` gate

Because feature-major regresses shallow pyramids, it is gated by a config knob:

```jsonc
{ "featureMajorPoints": true, "featureMajorMinZoom": 13 }  // default 13
```

The default 13 sits just past the measured break-even (FM *lost* at maxZoom=12 by
~15% and won from z13 up) — and that break-even was measured on an FM-favorable
wide-bbox dataset, so 13 deliberately errs toward enabling.

When `maxZoom < featureMajorMinZoom`, eligible point builds fall back to prepared
tile-major (with a `NOTICE`), so turning `featureMajorPoints` on is safe
regardless of depth. Set `featureMajorMinZoom:0` to force feature-major on at any
depth (used by the test suite to exercise the path cheaply).

> **Heuristic caveat.** The real predictor is `R(z)` vs feature count, i.e. how
> *spread out* the data is relative to how many features it has — not `maxZoom`
> alone. A tight-bbox city extent keeps `R(z)` small and pushes the crossover to
> higher zoom; a sparse continent-wide set explodes `R(z)` early. `maxZoom` is a
> cheap proxy, not a precise one. A future refinement could estimate `R(maxZoom)`
> from the source bbox and compare it to the row count.

## Feature-driven enumeration generalizes feature-major to all geometries

Feature-major only fixes **points**. The same O(R(z)) → O(features) win is available
for lines and polygons by deriving candidate tiles from each feature's **envelope**
tile-range (a superset) and reusing the *identical* `ST_AsMVTGeom(...) IS NOT NULL`
clip — so the output is provably unchanged.

The opportunity is bounded by the bbox **fill ratio**, measured as
`candidate tiles / R(z)`:

| bbox shape | z14 R(z) | candidate tiles | non-empty | ceiling |
|------------|----------|-----------------|-----------|---------|
| tight (dense city extent) | 266 | 266 | 100% | **1.0×** |
| wide (dense core + outliers) | 1,403,272 | 316 | 0.02% | **4441×** |

On a **dense** extent every cell is occupied — feature-driven enumeration produces
the identical set and buys nothing. The win exists only when the bbox is sparse
(the same regime feature-major already exploits for points, and the shape
`bench/build_speed.sql` deliberately creates with its scattered `sparse_point`s).

**Measured A/B** (z14, wide bbox, 125k mixed feats, prepared+GiST, `margin=0`,
`splitSize=1`):

| form | cells enumerated | wall (run 2) |
|------|------------------|--------------|
| full-lattice (current) | 1,403,272 | 4,780 ms |
| feature-driven | 127,882 pairs (≈1.02/feat) | **74 ms** |

**~65× faster, byte-identical output** (316 = 316 tiles, set-diff 0 both ways on
tile set *and* per-tile feature_count). The win is removing the ~1.4M empty GiST
probes (~4.7 s); the surviving 74 ms is the real `O(features)` clip+group that
both forms must pay. So the saving is roughly a **fixed** cost (empty-probe count ×
per-probe time) — its *relative* size shrinks as feature density rises and grows
with the `R(z)/features` ratio (sparser data, higher zoom).

> **Worst case.** Candidate pairs = Σ per-feature envelope tile-span, so a geometry
> whose envelope covers the whole bbox (a bbox-spanning line) regenerates `R(z)`
> by itself. A real implementation should cap per-feature span and fall back to the
> `&&`-join for over-large features. For typical small features (the bench: short
> lines, small polys) the span is ~1 tile, so pairs ≈ feature count.

**Implemented** as `featureDrivenEnum` (`preparedBuild=true`, `parallel=1`,
`tileSRS=3857`). It is self-gating, so turning it on is always safe:

- A one-time prep scan computes the row count and the feature-tile pair count the
  explode would enumerate at `maxZoom`. If that is **not** below `R(maxZoom)` the
  path is disabled — this single check rules out both dense extents (every cell
  occupied) and a handful of bbox-spanning features (whose explode alone rivals
  the lattice), falling back to prepared tile-major with a `NOTICE`.
- Per zoom it engages only above the `R(z) > 2·rows` crossover, so the cheap
  shallow levels still run tile-major and only the sparse/high-zoom levels — where
  empty-cell enumeration explodes — take the feature-driven path.

Equivalence is covered by a `pg_test` at the default (nonzero) `tileExtend`, with
mixed geometry types, `simplifyPixels>0` and attributes, asserting an identical
tile set and encoded size against the tile-major path across the crossover.

## Parallel builds run the fast path on Morton blocks

`parallel>1` farms each zoom to background workers. Originally workers ran the
**baseline** tile-major path (re-transforming the source per strip per zoom),
because the prepared table was a session-local `TEMP` table invisible to other
backends. The parallel build now materializes the prepared table **committed**
(via an autonomous worker, deterministic per-pyramid name, self-healing
`DROP IF EXISTS` like the shadow table), so workers run the same prepared /
feature-driven fast path the coordinator would.

**Partition.** The fast path partitions each zoom rect into **Morton (Z-order)
blocks** — a 2-D grid (≈4 blocks per worker) ordered by the Morton code of the
block's grid position — assigned to workers **interleaved** (block *i* → worker
*i* mod *W*). Blocks are a disjoint partition that exactly covers the rect, so
every `(z,x,y)` is owned by exactly one worker (no overlap → no PK collision, no
gap → no missing tile). Interleaving matters: a contiguous Morton run covering a
dense region is split across workers rather than dumped on one. The pure-baseline
parallel path keeps its proven x-strips (it re-scans source per tile, so 2-D
blocking buys it little).

**The read-filter is load-bearing.** The Morton bounds clamp the candidate-tile
*explode*, not the *scan*: without a filter the `prepped` CTE reads **every**
feature in every worker (`W×` the scan), so the clip splits `W` ways but the scan
doesn't — near-zero speedup. Each worker therefore prefilters with
`geom_tile && <block envelope>` (the block's tile span in metres, padded by the
margin + 2 tiles — a provable superset of the explode, so it can't drop a tile),
turning the per-worker scan into a GiST index-scan of its block. `EXPLAIN ANALYZE`
confirms: an empty block reads 0 rows via the index instead of seq-scanning all.

**Measured** (200k points over a country bbox, `parallel=4` = coordinator + 3
workers, 8-core Apple aarch64, `featureDrivenEnum`, release build, one warm
session):

| dataset shape | serial | parallel=4 | speedup |
|---------------|--------|-----------|---------|
| distributed, `maxZoom=13` | 6.59 s | **3.95 s** | **1.67×** |
| distributed, `maxZoom=16` | 9.56 s | **5.18 s** | **1.85×** |
| concentrated core + far outliers, `maxZoom=13` | 7.35 s | 6.27 s | 1.17× |

All builds are **byte-identical** to serial (FULL JOIN on `(z,x,y)` by encoded
size, 0 mismatches at every depth). Per-zoom the expensive deep levels (z6–z15)
hit **2.1–4.3×** with 4 workers — the parallelism is efficient *where it engages*.

But the **overall** speedup is capped well below linear (~1.7–1.85×) and climbs
only *slowly* with depth — it is **not** near-linear. The per-zoom curve shows
why, and it is honest to state it plainly:

- **A fixed serial floor dominates.** The shallow zooms z0–z5 run inline on the
  coordinator (each rect is below `PARALLEL_MIN_CELLS=64`, so farming them costs
  more than it saves — measured 0.78–0.95×, i.e. *slower* in parallel). Together
  with the one-shot transform/index prefix (the committed prepared table is built
  single-threaded before any worker starts), that floor is a constant ~1.4 s that
  does not parallelize. Going `maxZoom` 13→16 only shifts the ratio 1.67→1.85×
  because the deep parallel levels grow while the floor stays fixed (Amdahl). To
  push past ~1.85× you must attack the floor itself — parallelize the transform
  prefix, or cut the ~230 ms/zoom fixed cost of the shallow inline levels — which
  is separate work from this partition scheme.
- **A concentrated core can't be tile-partitioned.** When a dense cluster fits in
  one Morton block (sparse data over a huge bbox), one worker carries it — no
  uniform grid resolution can split it without exploding the block count over the
  wide bbox. True balance there needs **work-aware** partitioning (quadtree on
  occupied tiles, or per-block feature counts) — a follow-up. Tile-exact ownership
  forbids partitioning by feature (two workers could emit the same tile → PK
  collision), so the partition must stay tile-based.

> **Caveat that bit once.** The committed prepared table must be dropped in the
> *coordinator's* transaction, not via an autonomous worker: the coordinator holds
> `ACCESS SHARE` on it for the whole build, so an autonomous `DROP`'s
> `ACCESS EXCLUSIVE` deadlocks against it — and because the coordinator waits on a
> status *file*, not a lock, Postgres can't detect it; it silently burned the full
> worker timeout (~60 s) every parallel build until fixed.

## The single-pass MVT refactor was a no-op

Commit `4cc10ff` ("single-pass MVT aggregation") moved `ST_AsMVT` out of a
`SELECT`+`HAVING` form into a `WITH grouped` CTE, claiming the old form "encoded
every tile twice." **Measured false.** PostgreSQL collapses `equal()` `Aggref`
nodes to a single per-group computation, and computes all aggregates before
`HAVING` filters — so the old form already encoded each tile once. A/B
`EXPLAIN ANALYZE` over identical input (see below): OLD ~= NEW in both regimes
(100k feats / 4 dense groups: 52.7 vs 56.7 ms; 790k feats / 85 groups: 582 vs
597 ms) — the CTE form is even marginally slower. The refactor is fine as
**cleanup** (explicit, doesn't lean on planner dedup) but is not a speedup.

## Reproduce

**A/B encode-cost** (settles the single-pass question) — built into the bench:

```bash
psql "$DATABASE_URL" -v scale=1000000 -v ab_zoom=8 -f bench/build_speed.sql
# read the "A/B encode-cost" section; compare the 2nd run of OLD vs NEW.
```

**Path comparison** — build the same dataset once and time each path. ST_BuildPyramid
returns a boolean, so wrap it to time it:

```sql
CREATE FUNCTION _timed_build(src text, gc text, fc text, cfg jsonb)
RETURNS numeric LANGUAGE plpgsql AS $$
DECLARE t0 timestamptz; ok boolean;
BEGIN t0 := clock_timestamp();
      ok := ST_BuildPyramid(src, gc, fc, cfg::text);
      RETURN round(EXTRACT(epoch FROM (clock_timestamp()-t0))*1000::numeric, 1);
END $$;

-- baseline / prepared / feature-major over the SAME point table, then compare
-- geometry_pyramid_stats.build_ms per zoom. Equivalence check:
--   SELECT count(DISTINCT s.tiles) -- must be 1 across paths within a dataset.
```

Per-zoom timings live in `geometry_pyramid_stats (pyramid_name, z, build_ms, tile_count)`.
