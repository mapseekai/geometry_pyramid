//! Tile fetch surface. Returns MVT bytes for a (z, x, y) tile, or NULL when
//! the pyramid does not have that tile materialized.

use crate::storage;
use pgrx::prelude::*;

/// `ST_Tile(pyramid_name, z, x, y) -> bytea`
///
/// Returns the MVT-encoded tile for the given pyramid at zoom `z`, column
/// `x`, row `y`. Returns NULL for empty/sparse cells or unknown pyramids.
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
    let table = storage::tile_table_name(name);
    let sql = format!(
        r#"SELECT mvt FROM "{table}" WHERE z = $1 AND x = $2 AND y = $3"#
    );
    let bytes: Option<Vec<u8>> = Spi::get_one_with_args(
        &sql,
        &[(z as i16).into(), x.into(), y.into()],
    )
    .unwrap_or(None);
    Ok(bytes)
}
