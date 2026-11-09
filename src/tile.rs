//! Tile fetch surface. Returns MVT bytes for a (z, x, y) tile, or NULL when
//! the pyramid does not have that tile materialized (or the request is outside
//! the configured range or the pyramid is unknown).
//!
//! Hybrid lookup strategy:
//!   • z < minZoom              → NULL
//!   • minZoom ≤ z ≤ maxZoom   → read the registered materialized table
//!   • z > maxZoom              → on-demand slice via `crate::dynamic::generate_tile`
//!                                when `dynamicBeyondMaxZoom = true`, else NULL
//!   • unknown pyramid          → NULL (catalog-first; never touches storage)

use pgrx::prelude::*;
use std::error::Error;

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
) -> Result<Option<Vec<u8>>, Box<dyn Error + Send + Sync>> {
    crate::pyramid::validate_identifier(name)?;
    if !(0..=24).contains(&z) {
        return Err(format!("zoom level {z} out of range").into());
    }
    let n = 1i32 << z;
    if !(0..n).contains(&x) || !(0..n).contains(&y) {
        return Ok(None);
    }

    // Catalog-first: an unknown pyramid yields NULL (never an error), and the
    // config tells us where the materialized/dynamic boundary is. catalog_meta
    // deserializes the row once (no intermediate Value clone) and re-validates
    // the source identifiers it carries.
    let Some(meta) = crate::pyramid::catalog_meta(name)? else {
        return Ok(None);
    };
    let cfg = &meta.config;
    let zu = z as u8;
    if zu < cfg.min_zoom {
        return Ok(None);
    }

    if zu <= cfg.max_zoom {
        // The scalar subquery also returns one row on a storage miss.
        let hit: Option<Vec<u8>> = if let Some(table) = &meta.bindings.storage {
            table.lock()?;
            Spi::get_one_with_args(
                &format!(
                    "SELECT (SELECT mvt FROM ONLY {} WHERE z=$1 AND x=$2 AND y=$3 AND tableoid=$4)",
                    table.sql()
                ),
                &[
                    (z as i16).into(),
                    x.into(),
                    y.into(),
                    pg_sys::Oid::from(table.oid).into(),
                ],
            )?
        } else {
            None
        };
        if hit.is_some() {
            return Ok(hit);
        }
        // Storage miss within the materialized range. With splitSize > 0 the tile
        // may be a deferred sparse tile (or genuinely empty) — slice on-demand;
        // an empty region yields None. With splitSize == 0 a miss means empty.
        if cfg.split_size == 0 {
            return Ok(None);
        }
    } else if !cfg.dynamic_beyond_max_zoom {
        return Ok(None);
    }
    let Some(source) = &meta.bindings.source else {
        return Ok(None);
    };
    source.lock()?;
    let source_sql = source.sql();
    let src = meta.for_source(name, &source_sql);
    crate::dynamic::generate_tile(src, zu, x, y)
}
