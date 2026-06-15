//! Per-pyramid tile storage. Each pyramid gets its own table named
//! `_geometry_pyramid_tiles_<name>` keyed by `(z, x, y)`.

use pgrx::prelude::*;

/// Table-name prefix for tile storage. Kept short so the suffix budget for
/// pyramid names stays generous within the 63-char identifier limit.
pub const TILE_TABLE_PREFIX: &str = "_gp_tiles_";

pub fn tile_table_name(pyramid_name: &str) -> String {
    format!("{TILE_TABLE_PREFIX}{pyramid_name}")
}

/// `CREATE TABLE` DDL for a tile table named `table`. Single source of the
/// tile-table schema, shared by the main-table and shadow-table paths so the
/// two can never drift. `table` must be a validated identifier (callers pass
/// names that passed `validate_identifier`).
pub fn tile_table_create_sql(table: &str) -> String {
    format!(
        r#"CREATE TABLE "{table}" (
            id     bigserial,
            z      smallint NOT NULL,
            x      integer  NOT NULL,
            y      integer  NOT NULL,
            format text     NOT NULL,
            srid   integer  NOT NULL,
            mvt    bytea    NOT NULL,
            PRIMARY KEY (z, x, y)
        )"#
    )
}

/// Drop the storage table if it exists. Used by `ST_DropPyramid`.
pub fn drop_tile_table(pyramid_name: &str) -> Result<(), spi::Error> {
    let table = tile_table_name(pyramid_name);
    // Drop only when present so a registered-but-unbuilt pyramid (no storage
    // table) does not emit a "does not exist, skipping" NOTICE. to_regclass
    // returns NULL on a missing relation; IF EXISTS guards a concurrent drop.
    let exists: Option<String> = Spi::get_one_with_args(
        "SELECT to_regclass($1)::text",
        &[format!("\"{table}\"").into()],
    )?;
    if exists.is_some() {
        Spi::run(&format!(r#"DROP TABLE IF EXISTS "{table}""#))?;
    }
    Ok(())
}
