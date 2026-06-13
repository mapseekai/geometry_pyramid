//! On-demand single-tile slicing. When ST_Tile requests a zoom above the
//! pyramid's materialized ceiling (maxZoom), the tile is sliced live from the
//! source table using the *same* MVT pipeline as the build path
//! (crate::build::tile_pipeline_from_sql), so tiles are visually consistent
//! across the materialized/dynamic boundary. Read-only; nothing is cached.

use crate::config::PyramidConfig;
use pgrx::prelude::*;

/// Slice a single (z, x, y) tile from the source table and return its MVT
/// bytes, or None when the source has no features there (or the source table
/// is missing). `source_table` / `geom_column` / `fid_column` must be
/// validated identifiers (callers pass catalog values re-validated upstream).
#[allow(clippy::too_many_arguments)]
pub(crate) fn generate_tile(
    pyramid_name: &str,
    source_table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
    z: u8,
    x: i32,
    y: i32,
) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>> {
    // Guard against a stale catalog pointing at a dropped source table:
    // to_regclass returns NULL (not an error) for a missing relation.
    let reg: Option<String> = Spi::get_one_with_args(
        "SELECT to_regclass($1)::text",
        &[format!("\"{source_table}\"").into()],
    )?;
    if reg.is_none() {
        return Ok(None);
    }

    let knobs = crate::build::ZoomKnobs::from_config(cfg, z).to_values();
    let (attrs_inner, attrs_outer) = crate::build::attribute_projections(cfg);
    let pipeline = crate::build::tile_pipeline_from_sql(
        source_table,
        geom_column,
        fid_column,
        &attrs_inner,
        &attrs_outer,
    );

    // Single tile = degenerate 1x1 range: xmin=xmax=x ($4,$5), ymin=ymax=y
    // ($6,$7). Same 14-param binding as build_zoom_level.
    let sql = format!(
        r#"SELECT ST_AsMVT(features.*, $2, $3, 'geom', 'fid')
           {pipeline}
           GROUP BY t.x, t.y"#
    );

    let bytes: Option<Vec<u8>> = Spi::get_one_with_args(
        &sql,
        &[
            (z as i32).into(),
            pyramid_name.into(),
            (cfg.tile_size as i32).into(),
            x.into(),
            x.into(),
            y.into(),
            y.into(),
            cfg.tile_srs.into(),
            (cfg.tile_extend as i32).into(),
            cfg.source_srs.into(),
            knobs.simplify_tol_m.into(),
            knobs.min_area_m2.into(),
            knobs.min_len_m.into(),
            knobs.drop_threshold.into(),
        ],
    )?;
    Ok(bytes)
}
