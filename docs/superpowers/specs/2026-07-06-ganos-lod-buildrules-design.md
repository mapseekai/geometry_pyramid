# Ganos-style LOD extensions for buildRules

Status: approved for implementation planning
Date: 2026-07-06
Related issues: MAP-21, MAP-22, MAP-39

## Summary

Extend the existing `buildRules` configuration in place so it remains the single
LOD rule surface. The current `filter`-only form stays valid, and the same rule
object gains Ganos-inspired `attrFields` and deterministic `sample` controls.

This deliberately does not add separate `attributesByZoom`, `samplingRules`, or
Ganos `level/value` parsing. There should be one project-native format:

```json
{
  "buildRules": [
    {
      "minZoom": 0,
      "maxZoom": 5,
      "filter": "rank < 5",
      "attrFields": ["name", "kind"],
      "sample": {
        "rate": 25,
        "key": ["fid"]
      }
    }
  ]
}
```

The goal is to close the remaining LOD design gap against Alibaba Ganos while
preserving the existing `geometry_pyramid` configuration shape and backward
compatibility.

## Scope

In scope:

- Per-zoom attribute field subsets through `buildRules[].attrFields`.
- Per-zoom deterministic feature-level sampling through `buildRules[].sample`.
- Shared semantics across source build, prepared build, feature-major-points,
  feature-driven, split-size dynamic reads, and beyond-maxZoom dynamic reads.
- A clear compatibility story for existing `attributes`, `dropRate`, and
  `buildRules[].filter`.

Out of scope:

- Ganos `level/value` input format.
- Ganos `merge` / coalesce behavior.
- PNG rendering, 3D tiles, `ST_Tile` signature compatibility, or dynamic tile
  caching.
- Changing default values for `splitSize`, `tileExtend`, or `parallel`.

## Configuration model

`BuildRule` remains zoom-band based:

- `minZoom`: inclusive minimum zoom.
- `maxZoom`: inclusive maximum zoom.
- `filter`: optional raw SQL boolean predicate over source columns.
- `attrFields`: optional list of MVT output attributes for this zoom band.
- `sample`: optional deterministic sampling rule.

`filter` becomes optional so a rule can exist only to change attributes or
sampling. A rule with none of `filter`, `attrFields`, or `sample` is invalid
because it has no effect.

`attrFields` follows the Ganos idea of rule-local attribute projection, but uses
the existing project rule shape. When no covering rule supplies `attrFields`,
the global `attributes` list remains the effective output attribute set.

`sample` has this shape:

```json
{
  "rate": 25,
  "key": ["fid"],
  "geometryTypes": ["POINT", "LINESTRING", "POLYGON"]
}
```

- `rate` is a percentage from `0` to `100`, matching the Ganos
  `ST_IsRandomSampled` mental model.
- `key` is optional and defaults to `["fid"]`. It selects stable values used by
  the hash expression. Values must be validated identifiers or the reserved
  pseudo-key `fid`.
- `geometryTypes` is optional. If omitted, sampling applies to all geometry
  types. If present, it limits sampling to those geometry families while other
  geometries pass through unchanged.

## Validation

Configuration parsing should reject:

- `minZoom > maxZoom`.
- Invalid attribute or sample key identifiers.
- `sample.rate < 0` or `sample.rate > 100`.
- Empty `sample.key`.
- Unsupported `geometryTypes` values.
- A no-op rule with no `filter`, `attrFields`, or `sample`.
- Overlapping rules whose effective `attrFields` differ on the same zoom.

Overlapping rules with identical `attrFields` are allowed. Overlapping rules
without `attrFields` do not conflict with a rule that sets `attrFields`; the
zoom uses the one explicitly supplied attribute set.

## Execution order

For each zoom, resolve all rules covering that zoom and apply the pipeline in a
fixed order:

1. Spatial bbox prefilter, unchanged from the current GiST-driven path.
2. `filter` predicates from covering rules, combined with `AND`.
3. Existing legacy `dropRate`, unchanged and still limited to POINT/MULTIPOINT.
4. `sample` predicates from covering rules, combined with `AND`.
5. Existing projection, simplification, min-size filtering, quantization, and
   MVT encoding.
6. Attribute projection using the zoom-effective `attrFields` or global
   `attributes` fallback.

This ordering keeps old behavior stable. Existing `filter` and `dropRate`
configs continue to mean what they mean today; `sample` only tightens output
when explicitly configured.

`sample.rate = 100` is equivalent to no sampling. `sample.rate = 0` excludes all
features covered by that sampling rule. Users may still use `filter: "1!=1"` to
make an intentionally empty zoom band explicit.

## Sampling semantics

Sampling is feature-level, not geometry-vertex thinning. It reduces the number
of features emitted into MVT; it does not simplify geometry internals and does
not preserve topology between retained and dropped features.

The sampling predicate should be deterministic across:

- repeated full builds,
- serial and parallel builds,
- prepared and source-table builds,
- feature-major-points and feature-driven paths,
- split-size dynamic reads,
- beyond-maxZoom dynamic reads.

The default key is the feature id because it is already required by the public
API and used by existing `dropRate` hashing. Explicit source-column keys are
allowed when they pass identifier validation, and every key must be projected or
otherwise available in prepared and optimized paths before sampling is applied.

`dropRate` stays as a legacy point-retention knob. If `dropRate` and `sample`
both cover the same point feature, both predicates apply. This is stricter but
backward-compatible because existing configs do not include `sample`.

## Attribute semantics

`attrFields` controls MVT output attributes only. It must not remove internal
fields needed for building, including:

- feature id,
- source geometry,
- prepared geometry,
- rule booleans,
- sample keys,
- any columns referenced by `filter`.

Prepared builds may need to project a superset internally while emitting only
the zoom-effective `attrFields` into the final MVT row. This distinction is
intentional: internal availability and public output are separate concerns.

When multiple covering rules define `attrFields`, the sets must match exactly.
The spec rejects union or last-wins behavior because either would make coarse
zoom output depend on rule ordering and would be hard to explain.

## Path integration

The implementation should avoid per-path rule logic. Add shared helpers that
resolve zoom-effective LOD state once and feed the existing build paths.

Expected helper responsibilities:

- Resolve covering rules for a zoom.
- Build the `AND` filter clause for source and prepared targets.
- Build deterministic sampling predicates for source and prepared targets.
- Resolve effective output attributes for a zoom.
- Identify the internal columns required by filters, sample keys, and emitted
  attributes for prepared projections.

All paths must use those helpers:

- source-table tile-major build,
- prepared tile-major build,
- prepared feature-major-points build,
- prepared feature-driven build,
- dynamic `ST_Tile` reads for `splitSize`,
- dynamic `ST_Tile` reads for `z > maxZoom`.

If a path cannot access the required sample key or filter column, the build
should fail during validation or preparation, not silently widen or drop output.

## Documentation

README should describe `buildRules` as the single LOD rule surface:

- `filter`: per-band feature predicate.
- `attrFields`: per-band output attribute subset, inspired by Ganos.
- `sample`: per-band deterministic feature-level sampling.

The docs should state that this is intentionally not Ganos `level/value`
syntax. It is a project-native rule format that incorporates the Ganos LOD
capabilities relevant to MAP-21, MAP-22, and MAP-39.

The existing raw SQL trust boundary for `filter` remains unchanged: predicates
are trusted-admin input evaluated with the caller's privileges, like a view
definition.

## Test matrix

Config tests:

- Existing `filter`-only `buildRules` parses unchanged.
- `attrFields` parses and rejects invalid identifiers.
- `sample.rate` accepts `0`, `25`, `100` and rejects out-of-range values.
- `sample.key` defaults to `fid` and rejects invalid or empty keys.
- Overlapping conflicting `attrFields` are rejected.
- Overlapping identical `attrFields` are allowed.

Helper tests:

- Multiple filters covering a zoom are `AND`ed.
- Multiple sample rules covering a zoom are `AND`ed.
- Effective attributes fall back to global `attributes`.
- Effective attributes use `attrFields` when one covering rule supplies it.
- Prepared output uses prepared aliases while source output uses source aliases.

End-to-end tests:

- Source-table build: covered zoom emits only `attrFields` and sampled features.
- Prepared build: internal prepared projection keeps rule/sample needs while MVT
  output is cropped by `attrFields`.
- Dynamic split-size read: live tile output matches materialized output for the
  same zoom-effective LOD state.
- Beyond-maxZoom read: live tile applies the same `filter`, `attrFields`, and
  `sample` semantics.
- Feature-major-points: optimized point path applies sample and attr fields.
- Feature-driven: optimized sparse path applies sample and attr fields.
- Regression: no `attrFields` or `sample` means existing output semantics stay
  unchanged.

## Multica closure criteria

This spec closes the design requirements for:

- MAP-21: it defines per-level attribute subsets and their conflict rules.
- MAP-22: it defines deterministic per-level feature sampling and compatibility
  with existing `dropRate`.
- MAP-39: it states the unified LOD direction, the Ganos-inspired scope, and the
  excluded future work (`merge`, rendering, 3D, compatibility aliases).

Implementation remains a separate phase.
