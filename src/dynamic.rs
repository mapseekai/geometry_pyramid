//! On-demand single-tile slicing. When ST_Tile requests a zoom above the
//! pyramid's materialized ceiling (maxZoom), the tile is sliced live from the
//! source table using the *same* MVT pipeline as the build path
//! (crate::build::tile_pipeline_from_sql), so tiles are visually consistent
//! across the materialized/dynamic boundary. Read-only; nothing is cached.

use crate::source::PyramidSource;
use pgrx::prelude::*;
use std::error::Error;

/// Slice a single (z, x, y) tile from the source table and return its MVT
/// bytes, or None when the source has no features there. The caller locks the registered source before passing its qualified SQL name.
pub(crate) fn generate_tile(
    src: PyramidSource,
    z: u8,
    x: i32,
    y: i32,
) -> Result<Option<Vec<u8>>, Box<dyn Error + Send + Sync>> {
    let cfg = src.cfg;
    let (saved, attempts) = crate::stats::effective_knobs(src, z)?;
    if saved.is_none() && attempts > 1 {
        return Err(
            "legacy budget settings are unavailable; run ST_UpdatePyramid(name, '')".into(),
        );
    }
    let knobs = saved.unwrap_or_else(|| crate::build::ZoomKnobs::from_config(cfg, z));
    let pipeline = crate::build::tile_pipeline_from_sql(src, z, 0, false);

    // Single tile = degenerate 1x1 range: xmin=xmax=x ($4,$5), ymin=ymax=y
    // ($6,$7). Same 14-param binding as build_zoom_level.
    //
    // Drop the GROUP BY: for a 1x1 tile range, ST_AsMVT without GROUP BY is a
    // global aggregate that always returns exactly 1 row — even when 0 features
    // intersect the tile (in that case it returns empty bytes). This avoids the
    // pgrx 0.16 SpiError::InvalidPosition panic that occurs when a query with
    // GROUP BY returns 0 rows inside a nested SPI call (pg_extern called via SPI).
    let sql = format!(
        r#"SELECT ST_AsMVT(features.*, $2, $3, 'geom', 'fid')
           {pipeline}"#
    );

    let rect = crate::tile_rect::TileRect {
        z,
        xmin: x.into(),
        xmax: x.into(),
        ymin: y.into(),
        ymax: y.into(),
    };
    let bytes: Option<Vec<u8>> =
        Spi::get_one_with_args(&sql, &crate::build::tile_args(src, &rect, &knobs))?;
    // ST_AsMVT on 0 input rows returns empty bytes (not NULL). Convert empty
    // MVT to None so callers see NULL for tiles with no features.
    Ok(bytes.filter(|b| !b.is_empty()))
}
