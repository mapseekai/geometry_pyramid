//! Shadow-table lifecycle: full builds write into a shadow table that is
//! atomically swapped into place, so readers never observe a half-built
//! pyramid. With parallel > 1 the shadow is created by an autonomous
//! worker (committed DDL) so background workers can see it.
//!
//! # Safety contract
//! Every `pyramid_name` argument **must** have been validated by
//! [`crate::pyramid::validate_identifier`] before being passed to any
//! function in this module. The DDL strings are built with double-quoted
//! identifiers, which is injection-safe only because `validate_identifier`
//! excludes the double-quote character (it allows `[A-Za-z0-9_]` only).

use crate::storage;
use pgrx::prelude::*;

pub const SHADOW_SUFFIX: &str = "__shadow";

pub fn shadow_table_name(pyramid_name: &str) -> String {
    format!("{}{}", storage::tile_table_name(pyramid_name), SHADOW_SUFFIX)
}

/// SQL that (re)creates the shadow table. Factored out so the parallel
/// path can hand the same DDL to an autonomous ExecSql worker. The CREATE
/// body comes from [`storage::tile_table_create_sql`] so the shadow schema
/// can never drift from the main tile table.
pub fn create_sql(pyramid_name: &str) -> String {
    let table = shadow_table_name(pyramid_name);
    format!(
        r#"DROP TABLE IF EXISTS "{table}"; {}"#,
        storage::tile_table_create_sql(&table)
    )
}

/// Create the shadow table inside the current transaction (parallel = 1).
pub fn create(pyramid_name: &str) -> Result<(), spi::Error> {
    Spi::run(&create_sql(pyramid_name))
}

/// Replace the main tile table with the shadow. Runs in the caller's
/// transaction, so it is all-or-nothing: a crash or error before commit
/// rolls the whole swap back and the previous main table survives.
///
/// Both statements take `ACCESS EXCLUSIVE` on the named tables, so a
/// concurrent reader that has not yet acquired its `ACCESS SHARE` lock
/// blocks until this transaction commits and then sees the new table. The
/// one residual gap is OID-level: a reader that resolved the old main
/// table's OID *before* the swap committed can observe a transient
/// "relation does not exist" on its next access, because DROP+RENAME gives
/// the replacement table a fresh OID. This is a transient error, never data
/// corruption, and is the accepted trade-off of the shadow-swap design
/// (a fully reader-transparent swap would require a view indirection layer).
pub fn swap(pyramid_name: &str) -> Result<(), spi::Error> {
    let main = storage::tile_table_name(pyramid_name);
    let shadow = shadow_table_name(pyramid_name);
    Spi::run(&format!(r#"DROP TABLE IF EXISTS "{main}""#))?;
    Spi::run(&format!(r#"ALTER TABLE "{shadow}" RENAME TO "{main}""#))?;
    // ALTER TABLE ... RENAME does not rename the `id` column's owned
    // bigserial sequence, so realign it to the main table name (DROP main
    // above also dropped any prior "{main}_id_seq", freeing the name). Keeps
    // \d / pg_dump output consistent; IF EXISTS guards the no-id-column case.
    Spi::run(&format!(
        r#"ALTER SEQUENCE IF EXISTS "{shadow}_id_seq" RENAME TO "{main}_id_seq""#
    ))
}

/// Best-effort cleanup of a leftover shadow table.
pub fn abandon(pyramid_name: &str) -> Result<(), spi::Error> {
    let shadow = shadow_table_name(pyramid_name);
    // Only attempt the DROP when the shadow actually exists. A normal drop has
    // no leftover shadow (a successful build renamed it to the main table), so
    // a bare `DROP TABLE IF EXISTS` would emit a chatty "does not exist,
    // skipping" NOTICE every time. to_regclass returns NULL (no error) when the
    // relation is missing; IF EXISTS is retained so a concurrent drop between
    // the check and the DROP is still a no-op rather than an error.
    let exists: Option<String> = Spi::get_one_with_args(
        "SELECT to_regclass($1)::text",
        &[format!("\"{shadow}\"").into()],
    )?;
    if exists.is_some() {
        Spi::run(&format!(r#"DROP TABLE IF EXISTS "{shadow}""#))?;
    }
    Ok(())
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
