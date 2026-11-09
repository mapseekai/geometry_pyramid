//! Pyramid build configuration parsed from the JSON `config` argument of
//! `ST_BuildPyramid`. Mirrors the option surface of GanosBase.

use serde::{Deserialize, Serialize};
use std::thread;

pub const DEFAULT_TILE_SIZE: u32 = 4096;
pub const DEFAULT_TILE_EXTEND: u32 = 8;
pub const DEFAULT_MIN_ZOOM: u8 = 0;
pub const DEFAULT_MAX_ZOOM: u8 = 18;
pub const DEFAULT_PARALLEL: u32 = 1;
/// Upper bound on the auto-resolved worker count (`parallel = 0`). Keeps a very
/// large host from spawning an absurd number of per-zoom job files; the build
/// degrades gracefully below this anyway (unspawnable slots run inline).
pub const PARALLEL_AUTO_CAP: u32 = 32;
pub const DEFAULT_PREPARED_BUILD: bool = false;
pub const DEFAULT_FEATURE_MAJOR_POINTS: bool = false;
/// Minimum `maxZoom` at which `featureMajorPoints` engages (see the field doc).
pub const DEFAULT_FEATURE_MAJOR_MIN_ZOOM: u8 = 13;
pub const DEFAULT_FEATURE_DRIVEN_ENUM: bool = false;
pub const DEFAULT_SOURCE_SRS: i32 = 4326;
pub const DEFAULT_TILE_SRS: i32 = 3857;

// Tippecanoe-inspired generation knobs.
pub const DEFAULT_SIMPLIFY_PIXELS: f64 = 1.0;
pub const DEFAULT_MIN_FEATURE_PIXELS: f64 = 0.0;
pub const DEFAULT_DROP_RATE: f64 = 1.0;
/// Default per-tile budget. 0 = disabled (no retry loop). Set to e.g.
/// `512000` to mimic tippecanoe's default ~500 KiB cap.
pub const DEFAULT_TILE_SIZE_BUDGET_BYTES: i64 = 0;
pub const DEFAULT_MAX_TILE_SIZE_ATTEMPTS: u32 = 5;

/// Web Mercator full span (m). Used to convert a zoom level + tile extent
/// into a per-pixel ground distance for simplification / size filters.
pub const WEBMERC_FULL_SPAN_M: f64 = 40_075_016.685_578_49;

/// Largest accepted `maxZoom`. Tile indices stay well within i32 and the
/// build cost stays bounded.
pub const MAX_ZOOM_LIMIT: u8 = 24;

/// Upper bound for `tileSize` (MVT extent). Well above the common 4096 while
/// staying clear of the `i32` wrap that the `as i32` bind would otherwise hit.
pub const MAX_TILE_SIZE: u32 = 16384;

/// A `geometry_pyramid_catalog` row joined with its parsed `config`, as
/// produced by the `json_build_object(...)` projection the lookup queries use.
/// Deserializing straight into this (rather than a `serde_json::Value` plus
/// per-field string indexing and a `config` deep-clone) keeps the hot
/// `ST_Tile` path allocation-light and the parsing in one typed place.
///
/// A NULL `source_table` / `geom_column` / `fid_column` fails deserialization
/// with a clean serde error — the same outcome as the previous `.ok_or` guards.
#[derive(Debug, Deserialize)]
pub struct CatalogRow {
    pub source_table: String,
    pub geom_column: String,
    pub fid_column: String,
    pub config: PyramidConfig,
    #[serde(flatten)]
    pub bindings: crate::storage::Bindings,
}

/// One level-of-detail rule. Within `[minZoom, maxZoom]`, a rule can filter
/// source features, override output attributes, or thin points. `filter` is
/// raw SQL over source columns, evaluated with the
/// **caller's own privileges** (trusted-admin input, like a view definition) and
/// validated against the source before any build work; it must not contain a
/// statement separator (`;`). See docs/superpowers/specs/*buildrules* and the
/// README note.
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
    #[serde(default, rename = "pointGrid", skip_serializing_if = "Option::is_none")]
    pub point_grid: Option<BuildRulePointGrid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRuleSample {
    pub rate: f64,
    #[serde(default = "default_sample_key")]
    pub key: Vec<String>,
    #[serde(default, rename = "geometryTypes")]
    pub geometry_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildRulePointGrid {
    #[serde(rename = "cellPixels")]
    pub cell_pixels: u32,
    #[serde(default, rename = "keepField", skip_serializing_if = "Option::is_none")]
    pub keep_field: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PyramidConfig {
    /// Optional pyramid name. Defaults to the source table name.
    #[serde(default)]
    pub name: Option<String>,

    /// SRID of the source geometry column.
    #[serde(default = "default_source_srs", rename = "sourceSRS")]
    pub source_srs: i32,

    /// SRID used for tile coordinates. Web Mercator by default.
    #[serde(default = "default_tile_srs", rename = "tileSRS")]
    pub tile_srs: i32,

    /// MVT extent in tile-local coordinates. Standard MVT default is 4096.
    #[serde(default = "default_tile_size", rename = "tileSize")]
    pub tile_size: u32,

    /// Buffer (in tile-local units) included around the tile to avoid edge
    /// artefacts when geometries straddle tile boundaries.
    #[serde(default = "default_tile_extend", rename = "tileExtend")]
    pub tile_extend: u32,

    /// Minimum zoom level to materialize.
    #[serde(default = "default_min_zoom", rename = "minZoom")]
    pub min_zoom: u8,

    /// Maximum zoom level to materialize. Ganos also calls this `maxLevel`.
    #[serde(default = "default_max_zoom", rename = "maxZoom", alias = "maxLevel")]
    pub max_zoom: u8,

    /// Number of parallel worker slots (coordinator + background workers) used to
    /// build the pyramid. `0` means **auto** — the host's available parallelism,
    /// capped at [`PARALLEL_AUTO_CAP`] (matching Ganos, where `0` = maximum
    /// allowed). `1` (the default) builds serially. Resolve via
    /// [`PyramidConfig::effective_parallel`]; over-requesting is safe (slots that
    /// can't spawn a worker run inline on the coordinator).
    #[serde(default = "default_parallel")]
    pub parallel: u32,

    /// Opt-in first-full-build speed path. When true and parallel=1, the build
    /// pre-projects source geometries into a temporary indexed table before
    /// slicing tiles. With parallel>1 the first implementation falls back to
    /// the existing worker path.
    #[serde(default = "default_prepared_build", rename = "preparedBuild")]
    pub prepared_build: bool,

    /// Experimental Ganos-like sparse path for pure POINT sources. When enabled
    /// together with preparedBuild and parallel=1, the builder groups points by
    /// their computed tile id instead of probing every tile in the source bbox.
    #[serde(
        default = "default_feature_major_points",
        rename = "featureMajorPoints"
    )]
    pub feature_major_points: bool,

    /// Minimum `maxZoom` at which `featureMajorPoints` actually engages.
    /// Feature-major cost is ~flat per zoom (O(features)) while tile-major cost
    /// grows ~4x per zoom (O(R(z)) bbox-cell enumerate + probe), so feature-major
    /// only wins for deep pyramids — benchmarks (PG17/PostGIS3.6) put the total
    /// break-even between z12 and z13 even on an FM-favorable wide-bbox dataset,
    /// so the default (13) errs toward enabling. Below this, eligible point builds
    /// fall back to prepared tile-major so shallow pyramids don't regress. Set 0
    /// to always use feature-major when otherwise eligible. NOTE: the true driver
    /// is bbox-spread vs feature-count; this maxZoom proxy is a coarse, cheap
    /// heuristic — tight-bbox sources may want it higher.
    #[serde(
        default = "default_feature_major_min_zoom",
        rename = "featureMajorMinZoom"
    )]
    pub feature_major_min_zoom: u8,

    /// Generalizes the `featureMajorPoints` idea to **all** geometry types: per
    /// zoom, candidate tiles are derived from each feature's envelope tile-range
    /// (arithmetic) instead of enumerating + GiST-probing every cell in the source
    /// bbox. Requires preparedBuild, parallel=1 and tileSRS=3857. Output is
    /// identical to the tile-major path (same `ST_AsMVTGeom` clip). It only
    /// engages where it pays off — a one-time prep check disables it when the
    /// per-feature explode would enumerate more than the full lattice (dense
    /// extents, or a few bbox-spanning features), and per zoom it is used only
    /// when R(z) exceeds a small multiple of the row count (sparse / high zoom).
    /// Below that crossover the dense tile-major path runs instead.
    #[serde(default = "default_feature_driven_enum", rename = "featureDrivenEnum")]
    pub feature_driven_enum: bool,

    /// Optional list of attribute columns to include in MVT features.
    #[serde(default)]
    pub attributes: Vec<String>,

    /// Simplification tolerance expressed in tile pixels (one MVT extent
    /// unit = one pixel). Geometries are reduced via
    /// `ST_SimplifyPreserveTopology` before MVT encoding. Tippecanoe's
    /// default is ~1 px; set 0 to disable.
    #[serde(default = "default_simplify_pixels", rename = "simplifyPixels")]
    pub simplify_pixels: f64,

    /// Minimum visual feature size in tile pixels. At each zoom, polygons
    /// whose area `< (size_px)²` and lines whose length `< size_px` are
    /// filtered out before MVT encoding. 0 disables.
    #[serde(
        default = "default_min_feature_pixels",
        rename = "minFeatureSizePixels"
    )]
    pub min_feature_size_px: f64,

    /// Tippecanoe-style point drop rate: at zoom z, retain
    /// `dropRate^(maxZoom - z)` of POINT/MULTIPOINT features. 1.0 disables;
    /// tippecanoe's default is 0.4 (≡ `-r2.5`).
    #[serde(default = "default_drop_rate", rename = "dropRate")]
    pub drop_rate: f64,

    /// Per-tile MVT byte budget (`--drop-densest-as-needed` analogue). When
    /// any tile at a zoom exceeds this size after the initial INSERT, the
    /// zoom is wiped and rebuilt with tighter simplify / min-size / drop
    /// knobs. 0 disables the retry loop.
    #[serde(
        default = "default_tile_size_budget_bytes",
        rename = "tileSizeBudgetBytes"
    )]
    pub tile_size_budget_bytes: i64,

    /// Cap on retry rounds per zoom when chasing the byte budget. The build
    /// gives up and emits whatever it has after this many attempts.
    #[serde(
        default = "default_max_tile_size_attempts",
        rename = "maxTileSizeAttempts"
    )]
    pub max_tile_size_attempts: u32,

    /// Density boundary. The default 0 materializes every non-empty tile.
    /// With splitSize > 0, tiles whose feature count is below it are NOT
    /// materialized at build time; ST_Tile slices them on-demand from the
    /// source on every request (mirrors Ganos's density-based dynamic build).
    #[serde(default = "default_split_size", rename = "splitSize")]
    pub split_size: u32,

    /// Upper bound (ms) the coordinator waits for parallel workers per zoom
    /// round. 0 = wait indefinitely.
    #[serde(default, rename = "buildTimeoutMs")]
    pub build_timeout_ms: i64,

    /// When a requested tile has z > maxZoom, slice it live from the source
    /// table instead of returning NULL. Default true (mirrors Ganos maxLevel).
    #[serde(
        default = "default_dynamic_beyond_max_zoom",
        rename = "dynamicBeyondMaxZoom"
    )]
    pub dynamic_beyond_max_zoom: bool,

    /// Per-level feature filters (level-of-detail). Empty = no filtering (default).
    /// Each rule's `filter` is raw SQL run with the **caller's own privileges**
    /// (trusted-admin input, like a view definition) — see [`BuildRule`] and the
    /// README security note.
    #[serde(default, rename = "buildRules")]
    pub build_rules: Vec<BuildRule>,
}

impl PyramidConfig {
    /// Ground distance covered by one MVT pixel at zoom `z`, in metres.
    /// Math is correct only when `tile_srs` is metre-based (Web Mercator).
    pub fn pixel_size_m(&self, z: u8) -> f64 {
        WEBMERC_FULL_SPAN_M / (self.tile_size as f64 * (1u64 << z) as f64)
    }

    /// Douglas–Peucker tolerance in metres at zoom `z`.
    pub fn simplify_tol_m(&self, z: u8) -> f64 {
        if self.simplify_pixels <= 0.0 {
            0.0
        } else {
            self.simplify_pixels * self.pixel_size_m(z)
        }
    }

    /// Minimum permissible polygon area (m²) and line length (m) at z.
    /// Both 0 mean "no filter".
    pub fn min_size_thresholds_m(&self, z: u8) -> (f64, f64) {
        if self.min_feature_size_px <= 0.0 {
            (0.0, 0.0)
        } else {
            let side = self.min_feature_size_px * self.pixel_size_m(z);
            (side * side, side)
        }
    }

    /// Retention probability for POINT/MULTIPOINT features at zoom `z`.
    /// Returns 1.0 when dropping is disabled or `z >= max_zoom`.
    pub fn drop_threshold_at(&self, z: u8) -> f64 {
        if self.drop_rate >= 1.0 || z >= self.max_zoom {
            1.0
        } else {
            self.drop_rate.powi((self.max_zoom - z) as i32)
        }
    }
}

impl Default for PyramidConfig {
    fn default() -> Self {
        Self {
            name: None,
            source_srs: DEFAULT_SOURCE_SRS,
            tile_srs: DEFAULT_TILE_SRS,
            tile_size: DEFAULT_TILE_SIZE,
            tile_extend: DEFAULT_TILE_EXTEND,
            min_zoom: DEFAULT_MIN_ZOOM,
            max_zoom: DEFAULT_MAX_ZOOM,
            parallel: DEFAULT_PARALLEL,
            prepared_build: DEFAULT_PREPARED_BUILD,
            feature_major_points: DEFAULT_FEATURE_MAJOR_POINTS,
            feature_major_min_zoom: DEFAULT_FEATURE_MAJOR_MIN_ZOOM,
            feature_driven_enum: DEFAULT_FEATURE_DRIVEN_ENUM,
            attributes: Vec::new(),
            simplify_pixels: DEFAULT_SIMPLIFY_PIXELS,
            min_feature_size_px: DEFAULT_MIN_FEATURE_PIXELS,
            drop_rate: DEFAULT_DROP_RATE,
            tile_size_budget_bytes: DEFAULT_TILE_SIZE_BUDGET_BYTES,
            max_tile_size_attempts: DEFAULT_MAX_TILE_SIZE_ATTEMPTS,
            split_size: 0,
            build_timeout_ms: 0,
            dynamic_beyond_max_zoom: true,
            build_rules: Vec::new(),
        }
    }
}

fn default_sample_key() -> Vec<String> {
    vec!["fid".to_string()]
}

fn valid_rule_identifier(ident: &str) -> bool {
    !ident.is_empty()
        && ident.len() <= 45
        && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn valid_geometry_type_name(name: &str) -> bool {
    matches!(
        name,
        "POINT" | "MULTIPOINT" | "LINESTRING" | "MULTILINESTRING" | "POLYGON" | "MULTIPOLYGON"
    )
}

impl PyramidConfig {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        if json.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(json)
    }

    /// Check the cross-field invariants a build relies on. Pure (no SPI), so it
    /// is unit-testable and is the single source of truth for these constraints
    /// — `ST_BuildPyramid` calls it once instead of scattering the checks.
    ///
    /// # Errors
    /// Returns a message describing the first violated invariant:
    /// `minZoom <= maxZoom`, `maxZoom <= MAX_ZOOM_LIMIT`, `0 < tileSize <=
    /// MAX_TILE_SIZE`, and `tileExtend < tileSize`.
    pub fn validate(&self) -> Result<(), String> {
        if self.tile_srs != 3857 {
            return Err(
                "tileSRS must be 3857; other tile coordinate systems are not supported".into(),
            );
        }
        if self.min_zoom > self.max_zoom {
            return Err(format!(
                "minZoom ({}) must be <= maxZoom ({})",
                self.min_zoom, self.max_zoom
            ));
        }
        if self.max_zoom > MAX_ZOOM_LIMIT {
            return Err(format!(
                "maxZoom ({}) exceeds the supported limit of {MAX_ZOOM_LIMIT}",
                self.max_zoom
            ));
        }
        // tile_size is the MVT extent and the divisor in pixel_size_m and in the
        // candidate-filter margin (tileExtend/tileSize); zero would divide by
        // zero, and a value past i32::MAX wraps negative when bound as i32.
        if self.tile_size == 0 {
            return Err("tileSize must be > 0".into());
        }
        if self.tile_size > MAX_TILE_SIZE {
            return Err(format!(
                "tileSize ({}) exceeds the supported limit of {MAX_TILE_SIZE}",
                self.tile_size
            ));
        }
        if self.tile_extend >= self.tile_size {
            return Err(format!(
                "tileExtend ({}) must be < tileSize ({})",
                self.tile_extend, self.tile_size
            ));
        }
        for (k, rule) in self.build_rules.iter().enumerate() {
            if rule.min_zoom > rule.max_zoom {
                return Err(format!(
                    "buildRules[{k}]: minZoom ({}) must be <= maxZoom ({})",
                    rule.min_zoom, rule.max_zoom
                ));
            }
            if rule.filter.is_none()
                && rule.attr_fields.is_none()
                && rule.sample.is_none()
                && rule.point_grid.is_none()
            {
                return Err(format!(
                    "buildRules[{k}]: must set at least one of filter, attrFields, sample, or pointGrid"
                ));
            }
            if let Some(filter) = &rule.filter {
                let filter = filter.trim();
                if filter.is_empty() {
                    return Err(format!("buildRules[{k}]: filter must not be empty"));
                }
                // Single-statement guard, not a sanitizer: the filter is evaluated
                // with the caller's own privileges (trusted-admin posture, like a
                // view definition — see the README note), so this only forbids
                // chaining a second statement. A malformed or unknown-column
                // expression is caught later by the build-time validation probe.
                if filter.contains(';') {
                    return Err(format!(
                        "buildRules[{k}]: filter must not contain a statement separator (';')"
                    ));
                }
            }
            if let Some(attrs) = &rule.attr_fields {
                for (i, attr) in attrs.iter().enumerate() {
                    if !valid_rule_identifier(attr) {
                        return Err(format!(
                            "buildRules[{k}].attrFields[{i}]: invalid identifier"
                        ));
                    }
                }
            }
            if let Some(sample) = &rule.sample {
                if !(0.0..=100.0).contains(&sample.rate) {
                    return Err(format!(
                        "buildRules[{k}].sample.rate must be between 0 and 100"
                    ));
                }
                if sample.key.is_empty() {
                    return Err(format!("buildRules[{k}].sample.key must not be empty"));
                }
                for (i, key) in sample.key.iter().enumerate() {
                    if key != "fid" && !valid_rule_identifier(key) {
                        return Err(format!(
                            "buildRules[{k}].sample.key[{i}]: invalid identifier"
                        ));
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
            if let Some(grid) = &rule.point_grid {
                if !(1..=self.tile_size).contains(&grid.cell_pixels) {
                    return Err(format!(
                        "buildRules[{k}].pointGrid.cellPixels must be between 1 and tileSize"
                    ));
                }
                if grid
                    .keep_field
                    .as_deref()
                    .is_some_and(|f| !valid_rule_identifier(f))
                {
                    return Err(format!(
                        "buildRules[{k}].pointGrid.keepField: invalid identifier"
                    ));
                }
            }
        }
        for z in 0..=MAX_ZOOM_LIMIT {
            let mut seen: Option<&[String]> = None;
            let mut seen_grid = None;
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
                    if let Some(grid) = &rule.point_grid {
                        if seen_grid.is_some_and(|prev| prev != grid) {
                            return Err(format!("buildRules: conflicting pointGrid for zoom {z}"));
                        }
                        seen_grid = Some(grid);
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn point_grid_at(&self, z: u8) -> Option<(usize, &BuildRulePointGrid)> {
        self.build_rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| (rule.min_zoom..=rule.max_zoom).contains(&z))
            .find_map(|(i, rule)| rule.point_grid.as_ref().map(|grid| (i, grid)))
    }

    /// Effective worker-slot count (coordinator + workers) for the build.
    ///
    /// `parallel = 0` resolves to the host's available parallelism, capped at
    /// [`PARALLEL_AUTO_CAP`] (Ganos's `0` = maximum-allowed semantics).
    /// Explicit requests use the same cap. The result is always `>= 1`, so the
    /// build's `effective_parallel() - 1` worker count never underflows.
    pub fn effective_parallel(&self) -> u32 {
        match self.parallel {
            0 => auto_parallelism(),
            n => n.min(PARALLEL_AUTO_CAP),
        }
    }
}

/// Available parallelism for `parallel = 0` (auto), clamped to
/// `[1, PARALLEL_AUTO_CAP]`. Falls back to 1 when the platform can't report it.
fn auto_parallelism() -> u32 {
    thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
        .clamp(1, PARALLEL_AUTO_CAP)
}

fn default_source_srs() -> i32 {
    DEFAULT_SOURCE_SRS
}
fn default_tile_srs() -> i32 {
    DEFAULT_TILE_SRS
}
fn default_tile_size() -> u32 {
    DEFAULT_TILE_SIZE
}
fn default_tile_extend() -> u32 {
    DEFAULT_TILE_EXTEND
}
fn default_min_zoom() -> u8 {
    DEFAULT_MIN_ZOOM
}
fn default_max_zoom() -> u8 {
    DEFAULT_MAX_ZOOM
}
fn default_parallel() -> u32 {
    DEFAULT_PARALLEL
}
fn default_prepared_build() -> bool {
    DEFAULT_PREPARED_BUILD
}
fn default_feature_major_points() -> bool {
    DEFAULT_FEATURE_MAJOR_POINTS
}
fn default_feature_major_min_zoom() -> u8 {
    DEFAULT_FEATURE_MAJOR_MIN_ZOOM
}
fn default_feature_driven_enum() -> bool {
    DEFAULT_FEATURE_DRIVEN_ENUM
}
fn default_simplify_pixels() -> f64 {
    DEFAULT_SIMPLIFY_PIXELS
}
fn default_min_feature_pixels() -> f64 {
    DEFAULT_MIN_FEATURE_PIXELS
}
fn default_drop_rate() -> f64 {
    DEFAULT_DROP_RATE
}
fn default_tile_size_budget_bytes() -> i64 {
    DEFAULT_TILE_SIZE_BUDGET_BYTES
}
fn default_max_tile_size_attempts() -> u32 {
    DEFAULT_MAX_TILE_SIZE_ATTEMPTS
}
fn default_dynamic_beyond_max_zoom() -> bool {
    true
}
fn default_split_size() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_uses_defaults() {
        let cfg = PyramidConfig::parse("").unwrap();
        assert_eq!(cfg.tile_size, DEFAULT_TILE_SIZE);
        assert_eq!(cfg.source_srs, 4326);
        assert_eq!(cfg.tile_srs, 3857);
        assert!(!cfg.feature_major_points);
    }

    #[test]
    fn validate_accepts_defaults() {
        assert!(PyramidConfig::default().validate().is_ok());
    }

    #[test]
    fn effective_parallel_resolves_auto_and_honours_explicit() {
        assert_eq!(
            PyramidConfig::parse(r#"{"parallel":1000000}"#)
                .unwrap()
                .effective_parallel(),
            PARALLEL_AUTO_CAP
        );
        assert_eq!(PyramidConfig::parse("").unwrap().effective_parallel(), 1);
        assert_eq!(
            PyramidConfig::parse(r#"{"parallel":4}"#)
                .unwrap()
                .effective_parallel(),
            4
        );
        // 0 = auto: resolves to a real worker count in [1, PARALLEL_AUTO_CAP],
        // never 0 (so `effective_parallel() - 1` can't underflow).
        let auto = PyramidConfig::parse(r#"{"parallel":0}"#)
            .unwrap()
            .effective_parallel();
        assert!((1..=PARALLEL_AUTO_CAP).contains(&auto));
    }

    #[test]
    fn validate_rejects_broken_invariants() {
        let cases = [
            PyramidConfig {
                min_zoom: 5,
                max_zoom: 3,
                ..Default::default()
            },
            PyramidConfig {
                max_zoom: 30,
                ..Default::default()
            },
            PyramidConfig {
                tile_size: 0,
                ..Default::default()
            },
            PyramidConfig {
                tile_size: 40000,
                ..Default::default()
            },
            PyramidConfig {
                tile_size: 256,
                tile_extend: 256,
                ..Default::default()
            },
        ];
        for cfg in cases {
            assert!(cfg.validate().is_err(), "expected rejection for {cfg:?}");
        }
    }

    #[test]
    fn parses_known_keys() {
        let cfg = PyramidConfig::parse(
            r#"{"sourceSRS":4490,"tileSize":512,"maxZoom":14,"parallel":4,"featureMajorPoints":true}"#,
        )
        .unwrap();
        assert_eq!(cfg.source_srs, 4490);
        assert_eq!(cfg.tile_size, 512);
        assert_eq!(cfg.max_zoom, 14);
        assert_eq!(cfg.parallel, 4);
        assert!(cfg.feature_major_points);
    }

    #[test]
    fn max_level_alias_roundtrips_and_rejects_ambiguous_bounds() {
        let cfg = PyramidConfig::parse(r#"{"maxLevel":1}"#).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.max_zoom, 1);
        let saved = serde_json::to_value(&cfg).unwrap();
        assert_eq!(saved["maxZoom"], 1);
        assert!(saved.get("maxLevel").is_none());
        assert_eq!(
            serde_json::from_value::<PyramidConfig>(saved)
                .unwrap()
                .max_zoom,
            1
        );
        assert!(PyramidConfig::parse(r#"{"maxZoom":1,"maxLevel":2}"#).is_err());
        assert!(PyramidConfig::parse(r#"{"maxLevel":25}"#)
            .unwrap()
            .validate()
            .is_err());
        assert!(PyramidConfig::parse(r#"{"minZoom":2,"maxLevel":1}"#)
            .unwrap()
            .validate()
            .is_err());
    }

    #[test]
    fn pixel_size_matches_web_mercator_math() {
        // At z=10 with tileSize=4096 the ground sample distance should be
        // ~9.55 m/px (40075016.685 / (4096 * 1024)).
        let cfg = PyramidConfig::default();
        let px = cfg.pixel_size_m(10);
        assert!((px - 9.5546).abs() < 1e-3, "got {px}");
    }

    #[test]
    fn drop_threshold_decays_per_zoom() {
        let cfg = PyramidConfig {
            drop_rate: 0.4,
            max_zoom: 10,
            ..PyramidConfig::default()
        };
        // maxZoom keeps everything; one step below halves to drop_rate.
        assert!((cfg.drop_threshold_at(10) - 1.0).abs() < 1e-9);
        assert!((cfg.drop_threshold_at(9) - 0.4).abs() < 1e-9);
        assert!((cfg.drop_threshold_at(8) - 0.16).abs() < 1e-9);
    }

    #[test]
    fn drop_threshold_disabled_when_rate_one() {
        let cfg = PyramidConfig::default(); // drop_rate = 1.0
        assert!((cfg.drop_threshold_at(0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn min_size_thresholds_zero_when_disabled() {
        let cfg = PyramidConfig::default(); // min_feature_size_px = 0
        let (a, l) = cfg.min_size_thresholds_m(12);
        assert_eq!(a, 0.0);
        assert_eq!(l, 0.0);
    }

    #[test]
    fn dynamic_beyond_max_zoom_defaults_true_and_parses() {
        let def = PyramidConfig::default();
        assert!(def.dynamic_beyond_max_zoom);
        let off = PyramidConfig::parse(r#"{"dynamicBeyondMaxZoom":false}"#).unwrap();
        assert!(!off.dynamic_beyond_max_zoom);
    }

    #[test]
    fn split_size_defaults_zero_and_parses() {
        assert_eq!(PyramidConfig::default().split_size, 0);
        let c = PyramidConfig::parse(r#"{"splitSize":10000}"#).unwrap();
        assert_eq!(c.split_size, 10000);
        let c2 = PyramidConfig::parse(r#"{"splitSize":250}"#).unwrap();
        assert_eq!(c2.split_size, 250);
    }

    #[test]
    fn prepared_build_defaults_false_and_parses() {
        assert!(!PyramidConfig::default().prepared_build);
        let cfg = PyramidConfig::parse(r#"{"preparedBuild":true}"#).unwrap();
        assert!(cfg.prepared_build);
    }

    #[test]
    fn point_grid_rejects_invalid_sizes_fields_and_conflicts() {
        for grid in [
            r#"{"cellPixels":0}"#,
            r#"{"cellPixels":4097}"#,
            r#"{"cellPixels":64,"keepField":"bad.name"}"#,
        ] {
            assert!(PyramidConfig::parse(&format!(
                r#"{{"buildRules":[{{"minZoom":0,"maxZoom":2,"pointGrid":{grid}}}]}}"#
            ))
            .unwrap()
            .validate()
            .is_err());
        }
        let err = PyramidConfig::parse(
            r#"{"buildRules":[
                {"minZoom":0,"maxZoom":2,"pointGrid":{"cellPixels":64}},
                {"minZoom":2,"maxZoom":3,"pointGrid":{"cellPixels":32}}
            ]}"#,
        )
        .unwrap()
        .validate()
        .unwrap_err();
        assert!(err.contains("conflicting pointGrid for zoom 2"));
    }

    #[test]
    fn build_rules_default_empty_and_parses() {
        assert!(PyramidConfig::default().build_rules.is_empty());
        let c = PyramidConfig::parse(
            r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"filter":"scalerank < 5"}]}"#,
        )
        .unwrap();
        assert_eq!(c.build_rules.len(), 1);
        assert_eq!(c.build_rules[0].min_zoom, 0);
        assert_eq!(c.build_rules[0].max_zoom, 4);
        assert_eq!(c.build_rules[0].filter.as_deref(), Some("scalerank < 5"));
        assert!(c.validate().is_ok());
    }

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
    fn build_rules_reject_inverted_zoom() {
        let c =
            PyramidConfig::parse(r#"{"buildRules":[{"minZoom":5,"maxZoom":2,"filter":"true"}]}"#)
                .unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn build_rules_reject_empty_filter() {
        let c =
            PyramidConfig::parse(r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"filter":"   "}]}"#)
                .unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn build_rules_reject_semicolon() {
        let c = PyramidConfig::parse(
            r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"filter":"true; DROP TABLE x"}]}"#,
        )
        .unwrap();
        assert!(c.validate().is_err());
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
}
