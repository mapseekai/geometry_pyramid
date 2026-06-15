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

use crate::source::PyramidSource;
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
    // config tells us where the materialized/dynamic boundary is. catalog_meta
    // deserializes the row once (no intermediate Value clone) and re-validates
    // the source identifiers it carries.
    let Some(meta) = crate::pyramid::catalog_meta(name)? else {
        return Ok(None);
    };
    let src = PyramidSource {
        name,
        source_table: &meta.source_table,
        geom_column: &meta.geom_column,
        fid_column: &meta.fid_column,
        cfg: &meta.config,
    };
    let cfg = src.cfg;

    let zu = z as u8;
    if zu < cfg.min_zoom {
        return Ok(None);
    }

    if zu <= cfg.max_zoom {
        // Materialized range: read the storage table. catalog_meta already told
        // us whether it exists (folded into its lookup via to_regclass), so a
        // registered-but-unbuilt pyramid is a miss, not an error — no extra
        // round-trip here. The scalar subquery always returns exactly one row
        // (NULL on miss), dodging pgrx's 0-row InvalidPosition error in a nested
        // SPI context.
        let hit: Option<Vec<u8>> = if meta.storage_exists {
            let table = storage::tile_table_name(name);
            Spi::get_one_with_args(
                &format!(
                    r#"SELECT (SELECT mvt FROM "{table}" WHERE z = $1 AND x = $2 AND y = $3 LIMIT 1)"#
                ),
                &[(z as i16).into(), x.into(), y.into()],
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
        return crate::dynamic::generate_tile(src, zu, x, y);
    }

    // z > maxZoom: on-demand slice from the source, if enabled. generate_tile
    // re-validates the catalog identifiers and guards the source table itself.
    if !cfg.dynamic_beyond_max_zoom {
        return Ok(None);
    }
    crate::dynamic::generate_tile(src, zu, x, y)
}
