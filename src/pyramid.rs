//! Top-level SQL surface for pyramid lifecycle management.
//!
//! These functions are the public API mirroring GanosBase. The current
//! implementation is a scaffold: it validates arguments, parses config, and
//! records pyramid metadata. Tile generation is implemented incrementally
//! in follow-up work.

use crate::config::PyramidConfig;
use pgrx::prelude::*;

/// `ST_BuildPyramid(table, geom_column, fid_column, config_json) -> bool`
///
/// Builds (or rebuilds) a vector pyramid for `table` using `geom_column` as
/// the geometry source and `fid_column` as the stable feature id. `config`
/// is a JSON document; pass `''` for defaults.
#[pg_extern(name = "ST_BuildPyramid")]
fn st_build_pyramid(
    table: &str,
    geom_column: &str,
    fid_column: &str,
    config: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let cfg = PyramidConfig::parse(config)?;
    validate_identifier(table)?;
    validate_identifier(geom_column)?;
    validate_identifier(fid_column)?;

    let pyramid_name = cfg.name.clone().unwrap_or_else(|| table.to_string());
    validate_identifier(&pyramid_name)?;

    Spi::run_with_args(
        "INSERT INTO geometry_pyramid_catalog \
         (pyramid_name, source_table, geom_column, fid_column, config) \
         VALUES ($1, $2, $3, $4, $5::jsonb) \
         ON CONFLICT (pyramid_name) DO UPDATE SET \
           source_table = EXCLUDED.source_table, \
           geom_column  = EXCLUDED.geom_column, \
           fid_column   = EXCLUDED.fid_column, \
           config       = EXCLUDED.config, \
           built_at     = now()",
        &[
            pyramid_name.into(),
            table.into(),
            geom_column.into(),
            fid_column.into(),
            serde_json::to_string(&cfg)?.into(),
        ],
    )?;

    notice!(
        "geometry_pyramid: registered pyramid '{}' over {}({}) (fid={}). \
         Tile materialization is not yet implemented.",
        pyramid_name,
        table,
        geom_column,
        fid_column
    );
    Ok(true)
}

/// `ST_DropPyramid(name) -> bool`
#[pg_extern(name = "ST_DropPyramid")]
fn st_drop_pyramid(
    name: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    validate_identifier(name)?;
    let removed = Spi::get_one_with_args::<i64>(
        "WITH d AS (DELETE FROM geometry_pyramid_catalog WHERE pyramid_name = $1 RETURNING 1) \
         SELECT count(*) FROM d",
        &[name.into()],
    )?
    .unwrap_or(0);
    Ok(removed > 0)
}

/// `ST_UpdatePyramid(name, where_clause) -> bool`
///
/// Re-builds the subset of pyramid tiles whose source rows match
/// `where_clause`. Pass `''` to refresh everything.
#[pg_extern(name = "ST_UpdatePyramid")]
fn st_update_pyramid(name: &str, _where_clause: &str) -> Result<bool, spi::Error> {
    let exists = Spi::get_one_with_args::<bool>(
        "SELECT EXISTS(SELECT 1 FROM geometry_pyramid_catalog WHERE pyramid_name = $1)",
        &[name.into()],
    )?
    .unwrap_or(false);
    if !exists {
        return Ok(false);
    }
    Spi::run_with_args(
        "UPDATE geometry_pyramid_catalog SET built_at = now() WHERE pyramid_name = $1",
        &[name.into()],
    )?;
    Ok(true)
}

/// `ST_Pyramid(name) -> jsonb` -- returns metadata for a pyramid.
#[pg_extern(name = "ST_Pyramid")]
fn st_pyramid_info(name: &str) -> Result<Option<pgrx::JsonB>, spi::Error> {
    let row: Option<String> = Spi::get_one_with_args(
        "SELECT jsonb_build_object( \
            'name', pyramid_name, \
            'source_table', source_table, \
            'geom_column', geom_column, \
            'fid_column', fid_column, \
            'config', config, \
            'built_at', built_at \
         )::text \
         FROM geometry_pyramid_catalog WHERE pyramid_name = $1",
        &[name.into()],
    )?;
    Ok(row
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .map(pgrx::JsonB))
}

/// Reject identifiers that contain anything other than letters, digits or
/// underscores. SQL identifier injection guard for the dynamic SQL paths.
fn validate_identifier(ident: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if ident.is_empty() {
        return Err("identifier must not be empty".into());
    }
    if !ident
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(format!("invalid identifier: {ident}").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_identifier;

    #[test]
    fn accepts_simple_identifier() {
        assert!(validate_identifier("foo_bar_1").is_ok());
    }

    #[test]
    fn rejects_quoted_identifier() {
        assert!(validate_identifier("\"foo\"").is_err());
        assert!(validate_identifier("foo;DROP").is_err());
        assert!(validate_identifier("").is_err());
    }
}
