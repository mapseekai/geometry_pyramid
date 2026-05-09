//! Pyramid build configuration parsed from the JSON `config` argument of
//! `ST_BuildPyramid`. Mirrors the option surface of GanosBase.

use serde::{Deserialize, Serialize};

pub const DEFAULT_TILE_SIZE: u32 = 4096;
pub const DEFAULT_TILE_EXTEND: u32 = 8;
pub const DEFAULT_MIN_ZOOM: u8 = 0;
pub const DEFAULT_MAX_ZOOM: u8 = 18;
pub const DEFAULT_PARALLEL: u32 = 1;
pub const DEFAULT_SOURCE_SRS: i32 = 4326;
pub const DEFAULT_TILE_SRS: i32 = 3857;

// Tippecanoe-inspired generation knobs.
pub const DEFAULT_SIMPLIFY_PIXELS: f64 = 1.0;
pub const DEFAULT_MIN_FEATURE_PIXELS: f64 = 0.0;
pub const DEFAULT_DROP_RATE: f64 = 1.0;

/// Web Mercator full span (m). Used to convert a zoom level + tile extent
/// into a per-pixel ground distance for simplification / size filters.
pub const WEBMERC_FULL_SPAN_M: f64 = 40_075_016.685_578_488;

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

    /// Maximum zoom level to materialize.
    #[serde(default = "default_max_zoom", rename = "maxZoom")]
    pub max_zoom: u8,

    /// Number of parallel workers used to build the pyramid.
    #[serde(default = "default_parallel")]
    pub parallel: u32,

    /// When true, only generate tiles for non-empty regions (sparse pyramid).
    #[serde(default = "default_sparse")]
    pub sparse: bool,

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
    #[serde(default = "default_min_feature_pixels", rename = "minFeatureSizePixels")]
    pub min_feature_size_px: f64,

    /// Tippecanoe-style point drop rate: at zoom z, retain
    /// `dropRate^(maxZoom - z)` of POINT/MULTIPOINT features. 1.0 disables;
    /// tippecanoe's default is 0.4 (≡ `-r2.5`).
    #[serde(default = "default_drop_rate", rename = "dropRate")]
    pub drop_rate: f64,
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
            sparse: true,
            attributes: Vec::new(),
            simplify_pixels: DEFAULT_SIMPLIFY_PIXELS,
            min_feature_size_px: DEFAULT_MIN_FEATURE_PIXELS,
            drop_rate: DEFAULT_DROP_RATE,
        }
    }
}

impl PyramidConfig {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        if json.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(json)
    }
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
fn default_sparse() -> bool {
    true
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_uses_defaults() {
        let cfg = PyramidConfig::parse("").unwrap();
        assert_eq!(cfg.tile_size, DEFAULT_TILE_SIZE);
        assert_eq!(cfg.source_srs, 4326);
        assert_eq!(cfg.tile_srs, 3857);
        assert!(cfg.sparse);
    }

    #[test]
    fn parses_known_keys() {
        let cfg = PyramidConfig::parse(
            r#"{"sourceSRS":4490,"tileSize":512,"maxZoom":14,"parallel":4}"#,
        )
        .unwrap();
        assert_eq!(cfg.source_srs, 4490);
        assert_eq!(cfg.tile_size, 512);
        assert_eq!(cfg.max_zoom, 14);
        assert_eq!(cfg.parallel, 4);
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
}
