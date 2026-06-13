//! Shadow-table lifecycle: full builds write into a shadow table that is
//! atomically swapped into place, so readers never observe a half-built
//! pyramid. With parallel > 1 the shadow is created by an autonomous
//! worker (committed DDL) so background workers can see it.

use crate::storage;
use pgrx::prelude::*;

pub const SHADOW_SUFFIX: &str = "__shadow";

pub fn shadow_table_name(pyramid_name: &str) -> String {
    format!("{}{}", storage::tile_table_name(pyramid_name), SHADOW_SUFFIX)
}

/// SQL that (re)creates the shadow table. Factored out so the parallel
/// path can hand the same DDL to an autonomous ExecSql worker.
pub fn create_sql(pyramid_name: &str) -> String {
    let table = shadow_table_name(pyramid_name);
    format!(
        r#"DROP TABLE IF EXISTS "{table}";
           CREATE TABLE "{table}" (
               z smallint NOT NULL,
               x integer  NOT NULL,
               y integer  NOT NULL,
               mvt bytea  NOT NULL,
               PRIMARY KEY (z, x, y)
           )"#
    )
}

/// Create the shadow table inside the current transaction (parallel = 1).
pub fn create(pyramid_name: &str) -> Result<(), spi::Error> {
    Spi::run(&create_sql(pyramid_name))
}

/// Atomically replace the main tile table with the shadow. Runs in the
/// caller's transaction; the ACCESS EXCLUSIVE window covers only this DDL.
pub fn swap(pyramid_name: &str) -> Result<(), spi::Error> {
    let main = storage::tile_table_name(pyramid_name);
    let shadow = shadow_table_name(pyramid_name);
    Spi::run(&format!(r#"DROP TABLE IF EXISTS "{main}""#))?;
    Spi::run(&format!(r#"ALTER TABLE "{shadow}" RENAME TO "{main}""#))
}

/// Best-effort cleanup of a leftover shadow table.
pub fn abandon(pyramid_name: &str) -> Result<(), spi::Error> {
    let shadow = shadow_table_name(pyramid_name);
    Spi::run(&format!(r#"DROP TABLE IF EXISTS "{shadow}""#))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_name_is_derived_from_main() {
        assert_eq!(shadow_table_name("foo"), "_gp_tiles_foo__shadow");
    }

    #[test]
    fn longest_derived_name_fits_pg_limit() {
        // validate_identifier caps names at 45 → prefix(10) + 45 + 8 = 63
        let longest = "a".repeat(45);
        assert!(shadow_table_name(&longest).len() <= 63);
    }
}
