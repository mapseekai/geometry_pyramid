//! On-demand single-tile slicing. When ST_Tile requests a zoom above the
//! pyramid's materialized ceiling (maxZoom), the tile is sliced live from the
//! source table using the *same* MVT pipeline as the build path
//! (crate::build::tile_pipeline_from_sql), so tiles are visually consistent
//! across the materialized/dynamic boundary. Read-only; nothing is cached.

use crate::source::PyramidSource;
use pgrx::prelude::*;

/// Slice a single (z, x, y) tile from the source table and return its MVT
/// bytes, or None when the source has no features there (or the source table
/// is missing). `src`'s identifiers must be validated (callers pass catalog
/// values re-validated upstream); they are re-validated here for defense.
pub(crate) fn generate_tile(
    src: PyramidSource,
    z: u8,
    x: i32,
    y: i32,
) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>> {
    let cfg = src.cfg;
    // Defense-in-depth: these identifiers are interpolated into dynamic SQL.
    // Callers pass catalog values (validated at build time), but this is a
    // pub(crate) entry point, so re-validate here rather than trust the caller.
    crate::pyramid::validate_identifier(src.source_table)?;
    crate::pyramid::validate_identifier(src.geom_column)?;
    crate::pyramid::validate_identifier(src.fid_column)?;

    // Guard against a stale catalog pointing at a dropped source table:
    // to_regclass returns NULL (not an error) for a missing relation.
    let reg: Option<String> = Spi::get_one_with_args(
        "SELECT to_regclass($1)::text",
        &[format!("\"{}\"", src.source_table).into()],
    )?;
    if reg.is_none() {
        return Ok(None);
    }

    let knobs = crate::build::ZoomKnobs::from_config(cfg, z).to_values();
    let (attrs_inner, attrs_outer) =
        crate::build::attribute_projections_for_zoom(cfg, z, "src", "prep");
    // buildRules applies on the on-demand read path too (RuleTarget::Source, the
    // raw src filter), so a tile served live — splitSize-deferred, or beyond
    // maxZoom — carries the same LOD filtering as its materialized peers. A zoom no
    // rule covers yields an empty clause, leaving unfiltered tiles unchanged.
    let rule_where = crate::build::rules_clause_for_zoom(
        cfg,
        z,
        crate::build::RuleTarget::Source {
            geom_column: src.geom_column,
            fid_column: src.fid_column,
        },
    );
    let pipeline = crate::build::tile_pipeline_from_sql(
        src.source_table,
        src.geom_column,
        src.fid_column,
        &attrs_inner,
        &attrs_outer,
        &rule_where,
    );

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

    let bytes: Option<Vec<u8>> = Spi::get_one_with_args(
        &sql,
        &[
            (z as i32).into(),
            src.name.into(),
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
    // ST_AsMVT on 0 input rows returns empty bytes (not NULL). Convert empty
    // MVT to None so callers see NULL for tiles with no features.
    Ok(bytes.filter(|b| !b.is_empty()))
}
