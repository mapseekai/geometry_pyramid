//! Tile fetch surface. Returns MVT bytes for a (z, x, y) tile, or NULL when
//! the pyramid does not have that tile materialized.

use pgrx::prelude::*;

/// `ST_Tile(pyramid_name, z, x, y) -> bytea`
///
/// Returns the MVT-encoded tile for the given pyramid at zoom `z`, column
/// `x`, row `y`. Returns NULL for empty/sparse cells.
#[pg_extern(name = "ST_Tile")]
fn st_tile(name: &str, z: i32, x: i32, y: i32) -> Option<Vec<u8>> {
    let _ = (name, z, x, y);
    // Tile generation is implemented in a follow-up. The signature is locked
    // now so downstream callers (pg_tileserv, custom HTTP handlers, etc.) can
    // wire up against it.
    None
}

/// `ST_TileEnvelope(z, x, y) -> bytea` (placeholder).
///
/// Returns the tile envelope as a PostGIS geometry encoded WKB. Currently
/// unimplemented.
#[pg_extern(name = "ST_TileEnvelope")]
fn st_tile_envelope(z: i32, x: i32, y: i32) -> Option<Vec<u8>> {
    let _ = (z, x, y);
    None
}
