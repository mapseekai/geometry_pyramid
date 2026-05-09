//! Per-pyramid tile storage. Each pyramid gets its own table named
//! `_geometry_pyramid_tiles_<name>` keyed by `(z, x, y)`.

use pgrx::prelude::*;

/// Table-name prefix for tile storage. Kept short so the suffix budget for
/// pyramid names stays generous within the 63-char identifier limit.
pub const TILE_TABLE_PREFIX: &str = "_gp_tiles_";

pub fn tile_table_name(pyramid_name: &str) -> String {
    format!("{TILE_TABLE_PREFIX}{pyramid_name}")
}

/// Drop and recreate the storage table for a pyramid.
pub fn reset_tile_table(pyramid_name: &str) -> Result<(), spi::Error> {
    let table = tile_table_name(pyramid_name);
    // Identifiers are validated upstream by `validate_identifier`; re-check
    // here as a defense-in-depth measure before composing DDL.
    debug_assert!(
        table
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    );
    Spi::run(&format!(r#"DROP TABLE IF EXISTS "{table}""#))?;
    Spi::run(&format!(
        r#"CREATE TABLE "{table}" (
            z smallint NOT NULL,
            x integer  NOT NULL,
            y integer  NOT NULL,
            mvt bytea  NOT NULL,
            PRIMARY KEY (z, x, y)
        )"#
    ))?;
    Ok(())
}

/// Drop the storage table if it exists. Used by `ST_DropPyramid`.
pub fn drop_tile_table(pyramid_name: &str) -> Result<(), spi::Error> {
    let table = tile_table_name(pyramid_name);
    Spi::run(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
}
