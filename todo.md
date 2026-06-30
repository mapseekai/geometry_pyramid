# TODO / Known issues

Tracked follow-ups after the `buildRules` (LOD) work. None block merge — the
feature is complete, the suite is green (90/90), and the whole-branch review
returned READY TO MERGE.

## Test coverage (follow-up, beyond spec §8)

These paths are correct by inspection (shared `rules_clause_for_zoom`, identical
pipeline placement) and indirectly exercised, but lack a dedicated end-to-end
regression test. Add when convenient:

- [ ] `buildRules` on the **on-demand read path** via `dynamic::generate_tile`:
      a `splitSize > 0` within-`maxZoom` tile, and a `z > maxZoom` tile with a
      covering rule — confirm they match the materialized tiles at covered zooms.
- [ ] **Multi-rule AND on the prepared path** e2e (current AND test runs the
      baseline source path; prepared AND is only string-unit-tested).
- [ ] **feature-major-points** and **feature-driven** paths built e2e with a rule
      (currently threaded + string-unit-tested, not built end-to-end).

## Code cleanup

- [ ] **`push_str(&format!(…))` → `write!(…)`** in the SQL projection helpers
      (`rule_projections_for`, `rules_clause_for_zoom`, and the sibling
      `attribute_projections_for`). Deferred intentionally: cold build-time path,
      and converting only the rule helpers would diverge from the sibling. Do all
      three in one sweep or not at all.

## Tooling

- [ ] Refresh the **GitNexus index** (stale since `9657a03`):
      `node .gitnexus/run.cjs analyze` from the project root.

## Future LOD enhancements (out of scope for the current `buildRules` spec)

The current `buildRules` only does per-level feature *filtering*. Possible
follow-on specs:

- [ ] Per-level **attribute-column subset** (drop heavy attributes at coarse zooms).
- [ ] Per-level **sampling / thinning** (keep a fraction of features, not a
      predicate) — generalizing `dropRate` (today points-only) to lines/areas.

## Notes on existing reduction knobs (context, not a task)

- `dropRate` — points only; no effect on line/area feature counts.
- `minFeatureSizePixels` — size-based, near-zero effect past ~z1 for long
  features; not importance-aware.
- `simplifyPixels` — reduces vertices per feature, not the feature count.
- `buildRules` is the only lever that reduces coarse-zoom feature *count* by
  attribute/importance.
