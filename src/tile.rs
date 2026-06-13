//! Tile fetch surface. Returns MVT bytes for a (z, x, y) tile, or NULL when
//! the pyramid does not have that tile materialized (or the request is outside
//! the configured range or the pyramid is unknown).
//!
//! Hybrid lookup strategy:
//!   • z < minZoom              → NULL
//!   • minZoom ≤ z ≤ maxZoom   → read materialized storage table (to_regclass
//!                                guard keeps an unbuilt pyramid as NULL, not error)
//!   • z > maxZoom              → on-demand slice via `crate::dynamic::generate_tile`
//!                                when `dynamicBeyondMaxZoom = true`, else NULL
//!   • unknown pyramid          → NULL (catalog-first; never touches storage)

use crate::storage;
use pgrx::prelude::*;

/// `ST_Tile(pyramid_name, z, x, y) -> bytea`
///
/// Returns the MVT-encoded tile for the given pyramid at zoom `z`, column
/// `x`, row `y`. Returns NULL for empty/sparse cells, tiles outside the
/// configured zoom range, or unknown pyramids. Slices tiles beyond
/// `maxZoom` on-demand from the source table when `dynamicBeyondMaxZoom`
/// is enabled (default: true).
#[pg_extern(name = "st_tile")]
fn st_tile(
    name: &str,
    z: i32,
    x: i32,
    y: i32,
) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>> {
    crate::pyramid::validate_identifier(name)?;
    if !(0..=24).contains(&z) {
        return Err(format!("zoom level {z} out of range").into());
    }
    let n = 1i32 << z;
    if !(0..n).contains(&x) || !(0..n).contains(&y) {
        return Ok(None);
    }

    // Catalog-first: an unknown pyramid yields NULL (never an error), and the
    // config tells us where the materialized/dynamic boundary is.
    // Wrap in a subquery with MAX so the outer SELECT always returns exactly
    // one row; pgrx 0.16's get_one_with_args panics on 0-row results from
    // pg_extern context when called nested inside an SPI session.
    let row: Option<String> = Spi::get_one_with_args(
        "SELECT MAX(json_build_object( \
            'source_table', source_table, \
            'geom_column',  geom_column, \
            'fid_column',   fid_column, \
            'config',       config \
         )::text) \
         FROM geometry_pyramid_catalog WHERE pyramid_name = $1",
        &[name.into()],
    )?;
    let Some(json) = row else { return Ok(None) };
    let parsed: serde_json::Value = serde_json::from_str(&json)?;
    let cfg: crate::config::PyramidConfig =
        serde_json::from_value(parsed["config"].clone())?;

    let zu = z as u8;
    if zu < cfg.min_zoom {
        return Ok(None);
    }

    if zu <= cfg.max_zoom {
        // Materialized range: read the storage table. Guard its existence so a
        // registered-but-unbuilt pyramid yields NULL instead of erroring.
        let table = storage::tile_table_name(name);
        let reg: Option<String> = Spi::get_one_with_args(
            "SELECT to_regclass($1)::text",
            &[format!("\"{table}\"").into()],
        )?;
        if reg.is_none() {
            return Ok(None);
        }
        let bytes: Option<Vec<u8>> = Spi::get_one_with_args(
            &format!(r#"SELECT mvt FROM "{table}" WHERE z = $1 AND x = $2 AND y = $3"#),
            &[(z as i16).into(), x.into(), y.into()],
        )?;
        return Ok(bytes);
    }

    // z > maxZoom: on-demand slice from the source, if enabled. generate_tile
    // re-validates the catalog identifiers and guards the source table itself.
    if !cfg.dynamic_beyond_max_zoom {
        return Ok(None);
    }
    let source_table = parsed["source_table"].as_str().unwrap_or("").to_string();
    let geom_column = parsed["geom_column"].as_str().unwrap_or("").to_string();
    let fid_column = parsed["fid_column"].as_str().unwrap_or("").to_string();
    crate::dynamic::generate_tile(name, &source_table, &geom_column, &fid_column, &cfg, zu, x, y)
}
