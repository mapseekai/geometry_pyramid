# Ganos-style buildRules LOD Implementation Plan
> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement MAP-21, MAP-22, and MAP-39 by extending the existing `buildRules` object with Ganos-style `attrFields` and deterministic feature sampling while keeping one project-native configuration format.

**Architecture:** Keep `buildRules` as the only LOD surface. Add parsed configuration fields in `src/config.rs`, centralize zoom-effective LOD resolution in `src/build.rs`, and make source, prepared, optimized, and dynamic paths consume the same helpers. The implementation must keep existing filter-only rules compatible and must separate internal prepared-table columns from final MVT output attributes.

**Tech Stack:** Rust, serde, pgrx 0.18, PostgreSQL 17, PostGIS 3.6, `cargo test`, `cargo pgrx test pg17`, GitNexus, Multica CLI.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-06-ganos-lod-buildrules-design.md`.
- Keep exactly one input format: `buildRules[].minZoom/maxZoom/filter/attrFields/sample`.
- Do not add `attributesByZoom`, `samplingRules`, or Ganos `level/value` parsing.
- Do not implement Ganos `merge`, PNG rendering, 3D tiles, `ST_Tile` compatibility aliases, dynamic tile caching, or default-value changes.
- Existing filter-only `buildRules` configs must parse and behave as they do today.
- `filter` remains trusted-admin SQL evaluated with caller privileges; keep the existing semicolon guard.
- `sample.rate` is a percentage in `0..=100`.
- `sample.key` defaults to `["fid"]`; keys are identifiers or the reserved pseudo-key `fid`.
- `dropRate` remains a legacy POINT/MULTIPOINT filter and still applies when `sample` is also configured.
- Before editing any existing runtime symbol, run GitNexus impact analysis for that symbol and stop for user review if the risk is HIGH or CRITICAL.
- Before every implementation commit, run GitNexus `detect_changes` and confirm changed symbols and execution flows match the task.
- Preserve unrelated uncommitted work in the main checkout. Execute in an isolated worktree if the main checkout is dirty.

## File Structure

- Modify: `src/config.rs`
  - Owns JSON config structs and pure validation rules.
  - Add `BuildRuleSample`, optional `BuildRule.filter`, optional `BuildRule.attr_fields`, and sample validation.
- Modify: `src/build.rs`
  - Owns LOD SQL helper generation and build-path integration.
  - Add zoom-effective attribute helpers, sample predicate helpers, and prepared internal projection helpers.
- Modify: `src/prepared.rs`
  - Owns prepared-table creation.
  - Feed prepared source creation with internal projections that include output attributes, rule booleans, and sample key columns.
- Modify: `src/dynamic.rs`
  - Owns live tile slicing.
  - Use the same zoom-effective output attributes and source LOD clause as materialized builds.
- Modify: `src/lib.rs`
  - Existing pgrx integration tests.
  - Add small end-to-end fixtures for source, prepared, dynamic, feature-major-points, and feature-driven paths.
- Modify: `README.md`
  - Document the single `buildRules` LOD surface and Ganos-inspired fields.

No new dependencies are planned.

## Task 1: Config Model and Pure Validation

- Modify: `src/config.rs`
- Test: `cargo test config::tests::build_rules`

**Interfaces:**

- `BuildRule.filter: Option<String>`
- `BuildRule.attr_fields: Option<Vec<String>>`
- `BuildRule.sample: Option<BuildRuleSample>`
- `BuildRuleSample.rate: f64`
- `BuildRuleSample.key: Vec<String>`
- `BuildRuleSample.geometry_types: Vec<String>`

**Steps:**

- [ ] Run impact analysis before editing:

```bash
rtk git status --short
# GitNexus MCP:
# impact({repo:"geometry_pyramid", target:"BuildRule", direction:"upstream", file_path:"src/config.rs", summaryOnly:true})
# impact({repo:"geometry_pyramid", target:"validate", direction:"upstream", file_path:"src/config.rs", summaryOnly:true})
```

- [ ] Write failing config unit tests in `src/config.rs`:

```rust
#[test]
fn build_rules_accept_filter_only_legacy_shape() {
    let c = PyramidConfig::parse(
        r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"filter":"rank < 5"}]}"#,
    )
    .unwrap();
    c.validate().unwrap();
    assert_eq!(c.build_rules[0].filter.as_deref(), Some("rank < 5"));
}

#[test]
fn build_rules_accept_attr_fields_and_sample() {
    let c = PyramidConfig::parse(
        r#"{"buildRules":[{"minZoom":0,"maxZoom":4,
              "attrFields":["name","kind"],
              "sample":{"rate":25,"key":["fid"],"geometryTypes":["POINT"]}}]}"#,
    )
    .unwrap();
    c.validate().unwrap();
    let rule = &c.build_rules[0];
    assert_eq!(rule.attr_fields.as_deref().unwrap(), ["name", "kind"]);
    let sample = rule.sample.as_ref().unwrap();
    assert_eq!(sample.rate, 25.0);
    assert_eq!(sample.key, ["fid"]);
    assert_eq!(sample.geometry_types, ["POINT"]);
}

#[test]
fn build_rules_default_sample_key_is_fid() {
    let c = PyramidConfig::parse(
        r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"sample":{"rate":100}}]}"#,
    )
    .unwrap();
    c.validate().unwrap();
    assert_eq!(c.build_rules[0].sample.as_ref().unwrap().key, ["fid"]);
}

#[test]
fn build_rules_reject_noop_rule() {
    let c = PyramidConfig::parse(r#"{"buildRules":[{"minZoom":0,"maxZoom":4}]}"#).unwrap();
    assert!(c.validate().unwrap_err().contains("must set at least one"));
}

#[test]
fn build_rules_reject_invalid_attr_field() {
    let c = PyramidConfig::parse(
        r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"attrFields":["bad-name"]}]}"#,
    )
    .unwrap();
    assert!(c.validate().unwrap_err().contains("attrFields[0]"));
}

#[test]
fn build_rules_reject_sample_rate_out_of_range() {
    let c = PyramidConfig::parse(
        r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"sample":{"rate":101}}]}"#,
    )
    .unwrap();
    assert!(c.validate().unwrap_err().contains("sample.rate"));
}

#[test]
fn build_rules_reject_empty_sample_key() {
    let c = PyramidConfig::parse(
        r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"sample":{"rate":25,"key":[]}}]}"#,
    )
    .unwrap();
    assert!(c.validate().unwrap_err().contains("sample.key"));
}

#[test]
fn build_rules_reject_invalid_sample_key() {
    let c = PyramidConfig::parse(
        r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"sample":{"rate":25,"key":["bad-key"]}}]}"#,
    )
    .unwrap();
    assert!(c.validate().unwrap_err().contains("sample.key[0]"));
}

#[test]
fn build_rules_reject_conflicting_attr_fields_on_same_zoom() {
    let c = PyramidConfig::parse(
        r#"{"buildRules":[
             {"minZoom":0,"maxZoom":4,"attrFields":["name"]},
             {"minZoom":2,"maxZoom":6,"attrFields":["kind"]}
           ]}"#,
    )
    .unwrap();
    assert!(c.validate().unwrap_err().contains("conflicting attrFields"));
}

#[test]
fn build_rules_allow_identical_attr_fields_on_same_zoom() {
    let c = PyramidConfig::parse(
        r#"{"buildRules":[
             {"minZoom":0,"maxZoom":4,"attrFields":["name","kind"]},
             {"minZoom":2,"maxZoom":6,"attrFields":["name","kind"]}
           ]}"#,
    )
    .unwrap();
    c.validate().unwrap();
}
```

- [ ] Update `BuildRule` and add `BuildRuleSample`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRule {
    #[serde(rename = "minZoom")]
    pub min_zoom: u8,
    #[serde(rename = "maxZoom")]
    pub max_zoom: u8,
    #[serde(default)]
    pub filter: Option<String>,
    #[serde(default, rename = "attrFields")]
    pub attr_fields: Option<Vec<String>>,
    #[serde(default)]
    pub sample: Option<BuildRuleSample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRuleSample {
    pub rate: f64,
    #[serde(default = "default_sample_key")]
    pub key: Vec<String>,
    #[serde(default, rename = "geometryTypes")]
    pub geometry_types: Vec<String>,
}

fn default_sample_key() -> Vec<String> {
    vec!["fid".to_string()]
}
```

- [ ] Add pure validation helpers in `src/config.rs`:

```rust
fn valid_rule_identifier(ident: &str) -> bool {
    !ident.is_empty()
        && ident.len() <= 45
        && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn valid_geometry_type_name(name: &str) -> bool {
    matches!(
        name,
        "POINT"
            | "MULTIPOINT"
            | "LINESTRING"
            | "MULTILINESTRING"
            | "POLYGON"
            | "MULTIPOLYGON"
    )
}
```

- [ ] Update `PyramidConfig::validate()` rule loop:

```rust
for (k, rule) in self.build_rules.iter().enumerate() {
    if rule.min_zoom > rule.max_zoom {
        return Err(format!(
            "buildRules[{k}]: minZoom ({}) must be <= maxZoom ({})",
            rule.min_zoom, rule.max_zoom
        ));
    }
    if rule.filter.is_none() && rule.attr_fields.is_none() && rule.sample.is_none() {
        return Err(format!(
            "buildRules[{k}]: must set at least one of filter, attrFields, or sample"
        ));
    }
    if let Some(filter) = &rule.filter {
        let filter = filter.trim();
        if filter.is_empty() {
            return Err(format!("buildRules[{k}]: filter must not be empty"));
        }
        if filter.contains(';') {
            return Err(format!(
                "buildRules[{k}]: filter must not contain a statement separator (';')"
            ));
        }
    }
    if let Some(attrs) = &rule.attr_fields {
        for (i, attr) in attrs.iter().enumerate() {
            if !valid_rule_identifier(attr) {
                return Err(format!("buildRules[{k}].attrFields[{i}]: invalid identifier"));
            }
        }
    }
    if let Some(sample) = &rule.sample {
        if !(0.0..=100.0).contains(&sample.rate) {
            return Err(format!("buildRules[{k}].sample.rate must be between 0 and 100"));
        }
        if sample.key.is_empty() {
            return Err(format!("buildRules[{k}].sample.key must not be empty"));
        }
        for (i, key) in sample.key.iter().enumerate() {
            if key != "fid" && !valid_rule_identifier(key) {
                return Err(format!("buildRules[{k}].sample.key[{i}]: invalid identifier"));
            }
        }
        for (i, geom_type) in sample.geometry_types.iter().enumerate() {
            if !valid_geometry_type_name(geom_type) {
                return Err(format!(
                    "buildRules[{k}].sample.geometryTypes[{i}]: unsupported geometry type"
                ));
            }
        }
    }
}
for z in 0..=MAX_ZOOM_LIMIT {
    let mut seen: Option<&[String]> = None;
    for rule in &self.build_rules {
        if z >= rule.min_zoom && z <= rule.max_zoom {
            if let Some(attrs) = &rule.attr_fields {
                if let Some(prev) = seen {
                    if prev != attrs.as_slice() {
                        return Err(format!(
                            "buildRules: conflicting attrFields for zoom {z}"
                        ));
                    }
                } else {
                    seen = Some(attrs.as_slice());
                }
            }
        }
    }
}
```

- [ ] Run:

```bash
rtk cargo test config::tests::build_rules
```

Expected result after implementation: all `build_rules_*` config tests pass.

- [ ] Run GitNexus staged detection and commit:

```bash
rtk git add src/config.rs
# GitNexus MCP: detect_changes({repo:"geometry_pyramid", scope:"staged"})
rtk git commit -m "Accept Ganos-style fields in buildRules"
```

## Task 2: Central LOD SQL Helpers

- Modify: `src/build.rs`
- Test: `cargo test build::tests::effective_ build::tests::sample_ build::tests::rule_ build::tests::attribute_`

**Interfaces:**

- `effective_attributes_for_zoom(cfg, z) -> &[String]`
- `attribute_projections_for_zoom(cfg, z, inner_alias, outer_alias) -> (String, String)`
- `prepared_attribute_projections(cfg) -> String`
- `sample_key_projections_for(cfg) -> String`
- `rules_clause_for_zoom(cfg, z, target) -> String`
- `RuleTarget::Source { geom_column, fid_column }`
- `RuleTarget::Prepared`

**Steps:**

- [ ] Run impact analysis before editing:

```bash
# GitNexus MCP:
# impact({repo:"geometry_pyramid", target:"attribute_projections_for", direction:"upstream", file_path:"src/build.rs", summaryOnly:true})
# impact({repo:"geometry_pyramid", target:"rule_projections_for", direction:"upstream", file_path:"src/build.rs", summaryOnly:true})
# impact({repo:"geometry_pyramid", target:"rules_clause_for_zoom", direction:"upstream", file_path:"src/build.rs", summaryOnly:true})
```

- [ ] Add helper unit tests in `src/build.rs`:

```rust
#[test]
fn effective_attributes_fall_back_to_global_attributes() {
    let cfg = PyramidConfig::parse(
        r#"{"attributes":["name","kind"],
            "buildRules":[{"minZoom":0,"maxZoom":4,"filter":"rank < 5"}]}"#,
    )
    .unwrap();
    assert_eq!(effective_attributes_for_zoom(&cfg, 2), ["name", "kind"]);
}

#[test]
fn effective_attributes_use_rule_attr_fields() {
    let cfg = PyramidConfig::parse(
        r#"{"attributes":["name","kind","detail"],
            "buildRules":[{"minZoom":0,"maxZoom":4,"attrFields":["name"]}]}"#,
    )
    .unwrap();
    assert_eq!(effective_attributes_for_zoom(&cfg, 2), ["name"]);
    assert_eq!(
        effective_attributes_for_zoom(&cfg, 5),
        ["name", "kind", "detail"]
    );
}

#[test]
fn attribute_projections_can_use_zoom_effective_attrs() {
    let cfg = PyramidConfig::parse(
        r#"{"attributes":["detail"],
            "buildRules":[{"minZoom":0,"maxZoom":4,"attrFields":["kind"]}]}"#,
    )
    .unwrap();
    let (inner, outer) = attribute_projections_for_zoom(&cfg, 2, "src", "prep");
    assert!(inner.contains(r#"src."kind" AS "kind""#));
    assert!(outer.contains(r#"prep."kind""#));
    assert!(!inner.contains("detail"));
}

#[test]
fn prepared_attribute_projection_includes_global_and_rule_attrs() {
    let cfg = PyramidConfig::parse(
        r#"{"attributes":["detail"],
            "buildRules":[{"minZoom":0,"maxZoom":4,"attrFields":["kind"]}]}"#,
    )
    .unwrap();
    let inner = prepared_attribute_projections(&cfg, "src");
    assert!(inner.contains(r#"src."detail" AS "detail""#));
    assert!(inner.contains(r#"src."kind" AS "kind""#));
}

#[test]
fn sample_key_projection_includes_non_fid_keys() {
    let cfg = PyramidConfig::parse(
        r#"{"buildRules":[{"minZoom":0,"maxZoom":4,
            "sample":{"rate":25,"key":["fid","kind"]}}]}"#,
    )
    .unwrap();
    let inner = sample_key_projections_for(&cfg, "src");
    assert!(inner.contains(r#"src."kind"::text AS _gp_sample_0_1"#));
    assert!(!inner.contains(r#"src."fid"::text AS _gp_sample_0_0"#));
}

#[test]
fn source_rules_clause_inlines_filter_and_sample() {
    let cfg = PyramidConfig::parse(
        r#"{"buildRules":[{"minZoom":0,"maxZoom":4,
              "filter":"rank < 5",
              "sample":{"rate":25,"key":["fid"],"geometryTypes":["POINT"]}}]}"#,
    )
    .unwrap();
    let clause = rules_clause_for_zoom(
        &cfg,
        2,
        RuleTarget::Source {
            geom_column: "geom",
            fid_column: "gid",
        },
    );
    assert!(clause.contains("AND (rank < 5)"));
    assert!(clause.contains(r#"GeometryType(src."geom") NOT IN ('POINT')"#));
    assert!(clause.contains(r#"hashtextextended(src."gid"::text"#));
    assert!(clause.contains("< 0.25"));
}

#[test]
fn prepared_rules_clause_uses_rule_booleans_and_sample_keys() {
    let cfg = PyramidConfig::parse(
        r#"{"buildRules":[{"minZoom":0,"maxZoom":4,
              "filter":"rank < 5",
              "sample":{"rate":25,"key":["fid","kind"]}}]}"#,
    )
    .unwrap();
    let clause = rules_clause_for_zoom(&cfg, 2, RuleTarget::Prepared);
    assert!(clause.contains("AND prep_src._gp_rule_0"));
    assert!(clause.contains("prep_src.fid::text"));
    assert!(clause.contains("prep_src._gp_sample_0_1"));
}
```

- [ ] Change `RuleTarget`:

```rust
#[derive(Clone, Copy, Debug)]
pub(crate) enum RuleTarget<'a> {
    Prepared,
    Source {
        geom_column: &'a str,
        fid_column: &'a str,
    },
}
```

- [ ] Add attribute helpers:

```rust
pub(crate) fn effective_attributes_for_zoom<'a>(
    cfg: &'a PyramidConfig,
    z: u8,
) -> &'a [String] {
    for rule in &cfg.build_rules {
        if z >= rule.min_zoom && z <= rule.max_zoom {
            if let Some(attrs) = &rule.attr_fields {
                return attrs.as_slice();
            }
        }
    }
    cfg.attributes.as_slice()
}

fn append_attribute_projection(
    inner: &mut String,
    outer: &mut String,
    inner_alias: &str,
    outer_alias: &str,
    col: &str,
) {
    if col.is_empty() || !col.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        notice!(
            "geometry_pyramid: attribute column {:?} is empty or has invalid characters; skipped",
            col
        );
        return;
    }
    write!(inner, r#", {inner_alias}."{col}" AS "{col}""#).expect("write to String");
    write!(outer, r#", {outer_alias}."{col}""#).expect("write to String");
}

pub(crate) fn attribute_projections_for_zoom(
    cfg: &PyramidConfig,
    z: u8,
    inner_alias: &str,
    outer_alias: &str,
) -> (String, String) {
    let mut inner = String::new();
    let mut outer = String::new();
    for col in effective_attributes_for_zoom(cfg, z) {
        append_attribute_projection(&mut inner, &mut outer, inner_alias, outer_alias, col);
    }
    (inner, outer)
}
```

- [ ] Keep `attribute_projections_for(cfg, inner_alias, outer_alias)` as a compatibility wrapper for tests that need global attributes:

```rust
pub(crate) fn attribute_projections_for(
    cfg: &PyramidConfig,
    inner_alias: &str,
    outer_alias: &str,
) -> (String, String) {
    let mut inner = String::new();
    let mut outer = String::new();
    for col in &cfg.attributes {
        append_attribute_projection(&mut inner, &mut outer, inner_alias, outer_alias, col);
    }
    (inner, outer)
}
```

- [ ] Add prepared internal projection helpers:

```rust
pub(crate) fn prepared_attribute_projections(cfg: &PyramidConfig, src_alias: &str) -> String {
    let mut cols: Vec<&str> = Vec::new();
    for col in &cfg.attributes {
        if !cols.contains(&col.as_str()) {
            cols.push(col);
        }
    }
    for rule in &cfg.build_rules {
        if let Some(attrs) = &rule.attr_fields {
            for col in attrs {
                if !cols.contains(&col.as_str()) {
                    cols.push(col);
                }
            }
        }
    }
    let mut s = String::new();
    for col in cols {
        write!(&mut s, r#", {src_alias}."{col}" AS "{col}""#).expect("write to String");
    }
    s
}

pub(crate) fn sample_key_projections_for(cfg: &PyramidConfig, src_alias: &str) -> String {
    let mut s = String::new();
    for (rule_idx, rule) in cfg.build_rules.iter().enumerate() {
        if let Some(sample) = &rule.sample {
            for (key_idx, key) in sample.key.iter().enumerate() {
                if key == "fid" {
                    continue;
                }
                write!(
                    &mut s,
                    r#", {src_alias}."{key}"::text AS _gp_sample_{rule_idx}_{key_idx}"#
                )
                .expect("write to String");
            }
        }
    }
    s
}
```

- [ ] Update `rule_projections_for` to skip rules without filters:

```rust
pub(crate) fn rule_projections_for(cfg: &PyramidConfig) -> String {
    let mut s = String::new();
    for (k, rule) in cfg.build_rules.iter().enumerate() {
        if let Some(filter) = &rule.filter {
            write!(&mut s, r#", ({filter}) AS _gp_rule_{k}"#).expect("write to String");
        }
    }
    s
}
```

- [ ] Add sample clause helpers:

```rust
fn sample_key_expr(rule_idx: usize, key_idx: usize, key: &str, target: RuleTarget<'_>) -> String {
    match target {
        RuleTarget::Prepared => {
            if key == "fid" {
                "prep_src.fid::text".to_string()
            } else {
                format!("prep_src._gp_sample_{rule_idx}_{key_idx}")
            }
        }
        RuleTarget::Source { fid_column, .. } => {
            if key == "fid" {
                format!(r#"src."{fid_column}"::text"#)
            } else {
                format!(r#"src."{key}"::text"#)
            }
        }
    }
}

fn geometry_type_expr(target: RuleTarget<'_>) -> &'static str {
    match target {
        RuleTarget::Prepared => "prep_src.geom_type",
        RuleTarget::Source { .. } => "",
    }
}
```

Use an owned string for source geometry type because it includes the configured column:

```rust
fn sample_clause_for_rule(
    rule_idx: usize,
    sample: &crate::config::BuildRuleSample,
    target: RuleTarget<'_>,
) -> String {
    let threshold = sample.rate / 100.0;
    let keys = sample
        .key
        .iter()
        .enumerate()
        .map(|(key_idx, key)| sample_key_expr(rule_idx, key_idx, key, target))
        .collect::<Vec<_>>()
        .join(", ");
    let hash = format!(
        "(abs(hashtextextended(concat_ws('|', {keys}), 0::bigint)) % 1000000)::float8 / 1000000.0 < {threshold}"
    );
    if sample.geometry_types.is_empty() {
        return format!(" AND ({hash})");
    }
    let list = sample
        .geometry_types
        .iter()
        .map(|g| format!("'{g}'"))
        .collect::<Vec<_>>()
        .join(",");
    let geom_expr = match target {
        RuleTarget::Prepared => "prep_src.geom_type".to_string(),
        RuleTarget::Source { geom_column, .. } => format!(r#"GeometryType(src."{geom_column}")"#),
    };
    format!(" AND ({geom_expr} NOT IN ({list}) OR {hash})")
}
```

- [ ] Update `rules_clause_for_zoom`:

```rust
pub(crate) fn rules_clause_for_zoom(
    cfg: &PyramidConfig,
    z: u8,
    target: RuleTarget<'_>,
) -> String {
    let mut s = String::new();
    for (k, rule) in cfg.build_rules.iter().enumerate() {
        if z >= rule.min_zoom && z <= rule.max_zoom {
            if rule.filter.is_some() {
                match target {
                    RuleTarget::Prepared => {
                        write!(&mut s, " AND prep_src._gp_rule_{k}")
                            .expect("write to String");
                    }
                    RuleTarget::Source { .. } => {
                        let filter = rule.filter.as_ref().expect("checked is_some");
                        write!(&mut s, " AND ({filter})").expect("write to String");
                    }
                }
            }
            if let Some(sample) = &rule.sample {
                s.push_str(&sample_clause_for_rule(k, sample, target));
            }
        }
    }
    s
}
```

- [ ] Run:

```bash
rtk cargo test build::tests::effective_
rtk cargo test build::tests::sample_
rtk cargo test build::tests::rule_
rtk cargo test build::tests::attribute_
```

Expected result after implementation: new helper tests and existing projection/rule tests pass.

- [ ] Run GitNexus staged detection and commit:

```bash
rtk git add src/build.rs
# GitNexus MCP: detect_changes({repo:"geometry_pyramid", scope:"staged"})
rtk git commit -m "Centralize zoom-effective buildRules LOD helpers"
```

## Task 3: Source and Dynamic Path Integration

- Modify: `src/build.rs`, `src/dynamic.rs`, `src/lib.rs`
- Test: focused `cargo pgrx test pg17` tests named below.

**Interfaces:**

- Source materialized build passes `z`, `geom_column`, and `fid_column` into `attribute_projections_for_zoom` and `rules_clause_for_zoom`.
- Dynamic `generate_tile` uses the same source target helper.

**Steps:**

- [ ] Run impact analysis before editing:

```bash
# GitNexus MCP:
# impact({repo:"geometry_pyramid", target:"tile_pipeline_from_sql", direction:"upstream", file_path:"src/build.rs", summaryOnly:true})
# impact({repo:"geometry_pyramid", target:"build_zoom_level", direction:"upstream", file_path:"src/build.rs", summaryOnly:true})
# impact({repo:"geometry_pyramid", target:"generate_tile", direction:"upstream", file_path:"src/dynamic.rs", summaryOnly:true})
```

- [ ] Update source build in `build_zoom_level`:

```rust
let (attrs_inner, attrs_outer) =
    attribute_projections_for_zoom(src.cfg, rect.z, "src", "prep");
let rule_where = rules_clause_for_zoom(
    src.cfg,
    rect.z,
    RuleTarget::Source {
        geom_column: src.geom_column,
        fid_column: src.fid_column,
    },
);
```

- [ ] Update dynamic build in `dynamic.rs`:

```rust
let (attrs_inner, attrs_outer) =
    crate::build::attribute_projections_for_zoom(cfg, z, "src", "prep");
let rule_where = crate::build::rules_clause_for_zoom(
    cfg,
    z,
    crate::build::RuleTarget::Source {
        geom_column: src.geom_column,
        fid_column: src.fid_column,
    },
);
```

- [ ] Add test helpers in `src/lib.rs` test module:

```rust
fn bytea_contains(bytes: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    bytes.windows(needle.len()).any(|window| window == needle)
}

fn tile_bytes(sql: &str) -> Vec<u8> {
    Spi::get_one::<Vec<u8>>(sql)
        .unwrap()
        .expect("tile should exist")
}
```

- [ ] Add source materialized pg test:

```rust
#[pg_test]
fn build_rules_attr_fields_and_sample_source_tiles() {
    Spi::run("DROP TABLE IF EXISTS _gp_lod_src").unwrap();
    Spi::run(
        "CREATE TABLE _gp_lod_src(
            gid bigint PRIMARY KEY,
            geom geometry(Point, 4326),
            kind text,
            detail text,
            rank int
        )",
    )
    .unwrap();
    Spi::run(
        "INSERT INTO _gp_lod_src VALUES
         (1, ST_SetSRID(ST_Point(0, 0), 4326), 'keep', 'large-detail', 1),
         (2, ST_SetSRID(ST_Point(30, 0), 4326), 'drop', 'large-detail', 9)",
    )
    .unwrap();
    let ok = Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_lod_src','geom','gid',
         '{\"minZoom\":0,\"maxZoom\":1,\"attributes\":[\"kind\",\"detail\"],
            \"buildRules\":[
              {\"minZoom\":0,\"maxZoom\":0,
               \"filter\":\"rank < 5\",
               \"attrFields\":[\"kind\"],
               \"sample\":{\"rate\":100,\"key\":[\"fid\"]}}
            ]}')",
    )
    .unwrap();
    assert_eq!(ok, Some(true));

    let z0 = tile_bytes("SELECT ST_Tile('_gp_lod_src', 0, 0, 0)");
    assert!(bytea_contains(&z0, "kind"));
    assert!(!bytea_contains(&z0, "detail"));

    let z1 = tile_bytes("SELECT ST_Tile('_gp_lod_src', 1, 1, 1)");
    assert!(bytea_contains(&z1, "kind"));
    assert!(bytea_contains(&z1, "detail"));
}
```

- [ ] Add dynamic pg test:

```rust
#[pg_test]
fn build_rules_attr_fields_and_sample_dynamic_tiles() {
    Spi::run("DROP TABLE IF EXISTS _gp_lod_dyn").unwrap();
    Spi::run(
        "CREATE TABLE _gp_lod_dyn(
            gid bigint PRIMARY KEY,
            geom geometry(Point, 4326),
            kind text,
            detail text,
            rank int
        )",
    )
    .unwrap();
    Spi::run(
        "INSERT INTO _gp_lod_dyn VALUES
         (1, ST_SetSRID(ST_Point(0, 0), 4326), 'keep', 'large-detail', 1),
         (2, ST_SetSRID(ST_Point(30, 0), 4326), 'drop', 'large-detail', 9)",
    )
    .unwrap();
    let ok = Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_lod_dyn','geom','gid',
         '{\"minZoom\":0,\"maxZoom\":0,\"splitSize\":9999,
            \"dynamicBeyondMaxZoom\":true,
            \"attributes\":[\"kind\",\"detail\"],
            \"buildRules\":[
              {\"minZoom\":0,\"maxZoom\":2,
               \"filter\":\"rank < 5\",
               \"attrFields\":[\"kind\"],
               \"sample\":{\"rate\":100,\"key\":[\"fid\"]}}
            ]}')",
    )
    .unwrap();
    assert_eq!(ok, Some(true));

    let z0 = tile_bytes("SELECT ST_Tile('_gp_lod_dyn', 0, 0, 0)");
    assert!(bytea_contains(&z0, "kind"));
    assert!(!bytea_contains(&z0, "detail"));

    let z1 = tile_bytes("SELECT ST_Tile('_gp_lod_dyn', 1, 1, 1)");
    assert!(bytea_contains(&z1, "kind"));
    assert!(!bytea_contains(&z1, "detail"));
}
```

- [ ] Run:

```bash
rtk cargo pgrx test pg17 build_rules_attr_fields_and_sample_source_tiles
rtk cargo pgrx test pg17 build_rules_attr_fields_and_sample_dynamic_tiles
```

Expected result after implementation: both focused pg tests pass.

- [ ] Run GitNexus staged detection and commit:

```bash
rtk git add src/build.rs src/dynamic.rs src/lib.rs
# GitNexus MCP: detect_changes({repo:"geometry_pyramid", scope:"staged"})
rtk git commit -m "Apply buildRules LOD to source and dynamic tiles"
```

## Task 4: Prepared and Optimized Path Integration

- Modify: `src/prepared.rs`, `src/build.rs`, `src/lib.rs`
- Test: focused prepared, feature-major-points, and feature-driven pg tests.

**Interfaces:**

- Prepared source creation stores a superset of internal columns:
  - global `attributes`,
  - all `attrFields`,
  - `_gp_rule_k` booleans for rules with filters,
  - `_gp_sample_k_i` text columns for non-fid sample keys.
- Prepared build paths emit only `effective_attributes_for_zoom(cfg, z)`.

**Steps:**

- [ ] Run impact analysis before editing:

```bash
# GitNexus MCP:
# impact({repo:"geometry_pyramid", target:"prepare_source_for_build", direction:"upstream", file_path:"src/prepared.rs", summaryOnly:true})
# impact({repo:"geometry_pyramid", target:"build_zoom_level_prepared", direction:"upstream", file_path:"src/build.rs", summaryOnly:true})
# impact({repo:"geometry_pyramid", target:"build_zoom_level_prepared_points_feature_major", direction:"upstream", file_path:"src/build.rs", summaryOnly:true})
# impact({repo:"geometry_pyramid", target:"build_zoom_level_prepared_feature_driven", direction:"upstream", file_path:"src/build.rs", summaryOnly:true})
```

- [ ] Update `prepare_source_for_build`:

```rust
let attrs_inner = crate::build::prepared_attribute_projections(src.cfg, "src");
let sample_keys_inner = crate::build::sample_key_projections_for(src.cfg, "src");
let rules_inner = crate::build::rule_projections_for(src.cfg);
let internal_inner = format!("{attrs_inner}{sample_keys_inner}{rules_inner}");
```

Pass `&internal_inner` into `create_table_sql` in place of the old `attrs_inner + rules_inner` split. Keep the `create_table_sql` parameters as two fragments if that produces the smaller diff; the SQL output must include attributes, sample key columns, and rule columns.

- [ ] Update `prepared.rs` tests:

```rust
#[test]
fn create_table_sql_projects_sample_key_columns() {
    let sql = create_table_sql(
        "_gp_prep_7",
        "roads",
        "geom",
        "gid",
        r#", src."kind"::text AS _gp_sample_0_1"#,
        "",
    );
    assert!(sql.contains(r#"src."kind"::text AS _gp_sample_0_1"#));
}
```

- [ ] Update prepared tile-major build in `build.rs`:

```rust
let (attrs_inner, attrs_outer) =
    attribute_projections_for_zoom(src.cfg, rect.z, "prep_src", "prep");
let rule_where = rules_clause_for_zoom(src.cfg, rect.z, RuleTarget::Prepared);
```

- [ ] Update feature-major-points path:

```rust
let (attrs_inner, attrs_outer) =
    attribute_projections_for_zoom(src.cfg, rect.z, "prep_src", "prep");
let (_, attrs_mvt) = attribute_projections_for_zoom(src.cfg, rect.z, "prep_src", "f");
let rule_where = rules_clause_for_zoom(src.cfg, rect.z, RuleTarget::Prepared);
```

- [ ] Update feature-driven path:

```rust
let (attrs_inner, attrs_from_prepped) =
    attribute_projections_for_zoom(src.cfg, rect.z, "prep_src", "prepped");
let (_, attrs_from_kept) = attribute_projections_for_zoom(src.cfg, rect.z, "x", "kept");
let (_, attrs_mvt) = attribute_projections_for_zoom(src.cfg, rect.z, "x", "f");
let rule_where = rules_clause_for_zoom(src.cfg, rect.z, RuleTarget::Prepared);
```

- [ ] Add prepared pg test:

```rust
#[pg_test]
fn build_rules_attr_fields_and_sample_prepared_tiles() {
    Spi::run("DROP TABLE IF EXISTS _gp_lod_prep").unwrap();
    Spi::run(
        "CREATE TABLE _gp_lod_prep(
            gid bigint PRIMARY KEY,
            geom geometry(Point, 4326),
            kind text,
            detail text,
            rank int
        )",
    )
    .unwrap();
    Spi::run(
        "INSERT INTO _gp_lod_prep VALUES
         (1, ST_SetSRID(ST_Point(0, 0), 4326), 'keep', 'large-detail', 1),
         (2, ST_SetSRID(ST_Point(30, 0), 4326), 'drop', 'large-detail', 9)",
    )
    .unwrap();
    let ok = Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_lod_prep','geom','gid',
         '{\"minZoom\":0,\"maxZoom\":1,\"preparedBuild\":true,
            \"attributes\":[\"kind\",\"detail\"],
            \"buildRules\":[
              {\"minZoom\":0,\"maxZoom\":0,
               \"filter\":\"rank < 5\",
               \"attrFields\":[\"kind\"],
               \"sample\":{\"rate\":100,\"key\":[\"fid\",\"kind\"]}}
            ]}')",
    )
    .unwrap();
    assert_eq!(ok, Some(true));
    let z0 = tile_bytes("SELECT ST_Tile('_gp_lod_prep', 0, 0, 0)");
    assert!(bytea_contains(&z0, "kind"));
    assert!(!bytea_contains(&z0, "detail"));
}
```

- [ ] Add feature-major-points pg test by adapting existing `build_rules_feature_major_points_filters_output`:

```rust
#[pg_test]
fn build_rules_attr_fields_and_sample_feature_major_points() {
    Spi::run("DROP TABLE IF EXISTS _gp_lod_fm").unwrap();
    Spi::run(
        "CREATE TABLE _gp_lod_fm(
            gid bigint PRIMARY KEY,
            geom geometry(Point, 4326),
            kind text,
            detail text,
            rank int
        )",
    )
    .unwrap();
    Spi::run(
        "INSERT INTO _gp_lod_fm VALUES
         (1, ST_SetSRID(ST_Point(0, 0), 4326), 'keep', 'large-detail', 1),
         (2, ST_SetSRID(ST_Point(30, 0), 4326), 'drop', 'large-detail', 9)",
    )
    .unwrap();
    let ok = Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_lod_fm','geom','gid',
         '{\"minZoom\":0,\"maxZoom\":1,\"preparedBuild\":true,
            \"featureMajorPoints\":true,\"featureMajorMinZoom\":0,
            \"attributes\":[\"kind\",\"detail\"],
            \"buildRules\":[
              {\"minZoom\":0,\"maxZoom\":0,
               \"filter\":\"rank < 5\",
               \"attrFields\":[\"kind\"],
               \"sample\":{\"rate\":100,\"key\":[\"fid\"]}}
            ]}')",
    )
    .unwrap();
    assert_eq!(ok, Some(true));
    let z0 = tile_bytes("SELECT ST_Tile('_gp_lod_fm', 0, 0, 0)");
    assert!(bytea_contains(&z0, "kind"));
    assert!(!bytea_contains(&z0, "detail"));
}
```

- [ ] Add feature-driven pg test by adapting existing `build_rules_feature_driven_filters_output`:

```rust
#[pg_test]
fn build_rules_attr_fields_and_sample_feature_driven() {
    Spi::run("DROP TABLE IF EXISTS _gp_lod_fd").unwrap();
    Spi::run(
        "CREATE TABLE _gp_lod_fd(
            gid bigint PRIMARY KEY,
            geom geometry(LineString, 4326),
            kind text,
            detail text,
            rank int
        )",
    )
    .unwrap();
    Spi::run(
        "INSERT INTO _gp_lod_fd VALUES
         (1, ST_SetSRID(ST_MakeLine(ST_Point(0,0), ST_Point(0.01,0.01)), 4326),
          'keep', 'large-detail', 1),
         (2, ST_SetSRID(ST_MakeLine(ST_Point(30,0), ST_Point(30.01,0.01)), 4326),
          'drop', 'large-detail', 9)",
    )
    .unwrap();
    let ok = Spi::get_one::<bool>(
        "SELECT ST_BuildPyramid('_gp_lod_fd','geom','gid',
         '{\"minZoom\":0,\"maxZoom\":1,\"preparedBuild\":true,
            \"featureDrivenEnum\":true,
            \"attributes\":[\"kind\",\"detail\"],
            \"buildRules\":[
              {\"minZoom\":0,\"maxZoom\":0,
               \"filter\":\"rank < 5\",
               \"attrFields\":[\"kind\"],
               \"sample\":{\"rate\":100,\"key\":[\"fid\"],
                         \"geometryTypes\":[\"LINESTRING\"]}}
            ]}')",
    )
    .unwrap();
    assert_eq!(ok, Some(true));
    let z0 = tile_bytes("SELECT ST_Tile('_gp_lod_fd', 0, 0, 0)");
    assert!(bytea_contains(&z0, "kind"));
    assert!(!bytea_contains(&z0, "detail"));
}
```

- [ ] Run:

```bash
rtk cargo test prepared::tests::create_table_sql_projects_sample_key_columns
rtk cargo pgrx test pg17 build_rules_attr_fields_and_sample_prepared_tiles
rtk cargo pgrx test pg17 build_rules_attr_fields_and_sample_feature_major_points
rtk cargo pgrx test pg17 build_rules_attr_fields_and_sample_feature_driven
```

Expected result after implementation: all focused tests pass.

- [ ] Run GitNexus staged detection and commit:

```bash
rtk git add src/prepared.rs src/build.rs src/lib.rs
# GitNexus MCP: detect_changes({repo:"geometry_pyramid", scope:"staged"})
rtk git commit -m "Apply buildRules LOD to prepared fast paths"
```

## Task 5: Documentation, Multica Closure, and Full Verification

- Modify: `README.md`
- Use: Multica CLI
- Test: full cargo and pgrx verification.

**Interfaces:**

- README config table documents `buildRules` as `filter`, `attrFields`, and `sample`.
- Multica issues MAP-21, MAP-22, and MAP-39 are marked done only after tests pass.

**Steps:**

- [ ] Run README impact check only as a staged GitNexus `detect_changes` after editing because README has no code symbol blast radius.

- [ ] Update the README configuration table row for `buildRules` to mention:

```markdown
| `buildRules` | `[]` | Per-level LOD rules. Each rule uses this project shape: `{minZoom,maxZoom,filter?,attrFields?,sample?}`. `filter` is a raw SQL boolean predicate, `attrFields` overrides output attributes for the zoom band, and `sample` performs deterministic feature-level sampling with a percentage `rate`. Multiple covering filters and samples are ANDed; conflicting `attrFields` are rejected. |
```

- [ ] Update the `buildRules` note near the current example with this JSON:

```json
"buildRules": [
  {
    "minZoom": 0,
    "maxZoom": 5,
    "filter": "scalerank < 5",
    "attrFields": ["name", "kind"],
    "sample": {"rate": 25, "key": ["fid"]}
  }
]
```

- [ ] Add a short compatibility paragraph:

```markdown
This is intentionally a single project-native rule format. It incorporates the
Ganos `attrFields` and stable sampling ideas without accepting Ganos
`level/value` input. Existing filter-only `buildRules` remain valid.
```

- [ ] Run:

```bash
rtk cargo fmt --check
rtk cargo test
rtk cargo pgrx test pg17
```

Expected result after implementation:

```text
test result: ok
```

For `cargo pgrx test pg17`, expected summary is all tests pass with zero failures.

- [ ] Run GitNexus detect changes for the full branch:

```bash
# GitNexus MCP: detect_changes({repo:"geometry_pyramid", scope:"compare", base_ref:"main"})
```

If implementing in a branch based on current main, compare against the pre-implementation base commit instead of moving `main`.

- [ ] Mark Multica issues done:

```bash
multica issue update 3319c3ba-3b20-4b81-afe3-7a0262b7ce77 --status done --output json
multica issue update 6a82784f-7c4f-4c7f-b191-3bea33742ac7 --status done --output json
multica issue update 99ced561-0252-4bef-85d5-b40a2bef4fc4 --status done --output json
multica issue list --project 07ece43a-f577-4739-a33f-dbc7ad7afcdd --output json
```

Expected Multica status:

- MAP-21: `done`
- MAP-22: `done`
- MAP-39: `done`

- [ ] Run GitNexus staged detection and commit docs/status evidence:

```bash
rtk git add README.md
# GitNexus MCP: detect_changes({repo:"geometry_pyramid", scope:"staged"})
rtk git commit -m "Document Ganos-style buildRules LOD extensions"
```

## Self-Review

- Spec coverage:
  - MAP-21 per-level attributes: Task 1 validates `attrFields`, Task 2 resolves effective attributes, Tasks 3 and 4 prove output paths, Task 5 documents it.
  - MAP-22 deterministic sampling: Task 1 validates `sample`, Task 2 generates deterministic hash clauses, Tasks 3 and 4 prove source/dynamic/prepared/optimized paths, Task 5 documents it.
  - MAP-39 unified LOD direction: all tasks keep `buildRules` as the one surface and exclude separate formats.
- Placeholder scan:
  - No unresolved markers, no empty implementation hooks, and every code step names exact files and commands.
- Type consistency:
  - `BuildRuleSample`, `attr_fields`, `effective_attributes_for_zoom`, `attribute_projections_for_zoom`, `prepared_attribute_projections`, `sample_key_projections_for`, and `RuleTarget` names are used consistently across tasks.
- Risk notes:
  - The largest risk is prepared path internal projection: final MVT attributes must be per zoom, while prepared tables need a superset for all zooms.
  - The second risk is dirty main checkout state. Use an isolated worktree before implementation if the root checkout still has unrelated changes.
