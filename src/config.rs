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
}
