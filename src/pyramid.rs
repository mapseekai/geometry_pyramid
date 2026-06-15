//! Top-level SQL surface for pyramid lifecycle management.

use crate::build;
use crate::config::PyramidConfig;
use crate::storage;
use pgrx::prelude::*;

/// `ST_BuildPyramid(table, geom_column, fid_column, config_json) -> bool`
///
/// Builds (or rebuilds) a vector pyramid for `table` using `geom_column` as
/// the geometry source and `fid_column` as the stable feature id. `config`
/// is a JSON document; pass `''` for defaults.
#[pg_extern(name = "st_buildpyramid")]
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

    if cfg.min_zoom > cfg.max_zoom {
        return Err(format!(
            "minZoom ({}) must be <= maxZoom ({})",
            cfg.min_zoom, cfg.max_zoom
        )
        .into());
    }
    if cfg.max_zoom > 24 {
        return Err(format!("maxZoom ({}) exceeds the supported limit of 24", cfg.max_zoom).into());
    }
    if cfg.tile_size == 0 {
        // tile_size is the MVT extent and the divisor in pixel_size_m and in the
        // candidate-filter margin (tileExtend/tileSize); zero would divide by zero.
        return Err("tileSize must be > 0".into());
    }

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
            pyramid_name.as_str().into(),
            table.into(),
            geom_column.into(),
            fid_column.into(),
            serde_json::to_string(&cfg)?.into(),
        ],
    )?;

    let total = build::build(&pyramid_name, table, geom_column, fid_column, &cfg)?;
    notice!(
        "geometry_pyramid: pyramid '{}' built with {} tile(s) across z={}..={}",
        pyramid_name,
        total,
        cfg.min_zoom,
        cfg.max_zoom
    );
    Ok(true)
}

/// `ST_DropPyramid(name) -> bool`
#[pg_extern(name = "st_droppyramid")]
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
    storage::drop_tile_table(name)?;
    crate::stats::delete_stats(name)?;
    crate::shadow::abandon(name)?;
    Ok(removed > 0)
}

/// `ST_UpdatePyramid(name, where_clause) -> bool`
///
/// Re-builds the pyramid identified by `name` using its previously
/// registered configuration.
///
/// When `where_clause` is empty, performs a full rebuild (shadow-swap,
/// exactly as `ST_BuildPyramid`).
///
/// When `where_clause` is non-empty, performs an **incremental** refresh:
/// computes the bounding box of all source rows matching the clause,
/// derives the dirty tile rectangle per zoom, DELETEs those tiles from
/// the main tile table and rebuilds just them in place — all within the
/// caller's transaction (failure rolls back; the pyramid is never
/// left half-updated). This follows the Ganos trust model: the caller
/// holds source-table privileges and the clause is executed as their SQL.
///
/// # Limitation: the clause must cover a row's OLD location
///
/// The dirty rectangle is the extent of rows the clause matches *now*. A row
/// that moved across tiles, or was deleted, leaves a stale tile at its former
/// position unless the clause also selects that region. Concretely:
/// - **Move across tiles**: `WHERE id = 42` only cleans the new position; the
///   old tile keeps the stale feature. Pass a clause covering both (e.g. a
///   bbox that spans old+new), or do a full rebuild.
/// - **Delete**: after `DELETE FROM src WHERE id = 42`, the row no longer
///   matches any clause, so `ST_UpdatePyramid(name, 'id = 42')` matches zero
///   rows and is a no-op — the old tile is never cleaned. Use a clause that
///   matches by region (e.g. `ST_Intersects(geom_or_bbox, ...)` against a
///   captured old extent), or a full rebuild.
/// Load and validate a pyramid's catalog row: (source_table, geom_column,
/// fid_column, config). Returns `None` when the pyramid is unknown.
///
/// The MAX(...) wrapper guarantees exactly one row (NULL when unknown) so
/// `get_one_with_args` never sees a 0-row result, which pgrx 0.16 surfaces as
/// a "SpiTupleTable positioned before the start or after the end" error inside
/// a nested SPI context. The three identifiers are re-validated before they
/// ride into dynamic SQL — defense-in-depth, matching ST_BuildPyramid.
pub(crate) fn catalog_meta(
    name: &str,
) -> Result<Option<(String, String, String, PyramidConfig)>, Box<dyn std::error::Error + Send + Sync>> {
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
    let source_table = parsed["source_table"]
        .as_str()
        .ok_or("catalog row has NULL source_table")?
        .to_string();
    let geom_column = parsed["geom_column"]
        .as_str()
        .ok_or("catalog row has NULL geom_column")?
        .to_string();
    let fid_column = parsed["fid_column"]
        .as_str()
        .ok_or("catalog row has NULL fid_column")?
        .to_string();
    let cfg: PyramidConfig = serde_json::from_value(parsed["config"].clone())?;
    validate_identifier(&source_table)?;
    validate_identifier(&geom_column)?;
    validate_identifier(&fid_column)?;
    Ok(Some((source_table, geom_column, fid_column, cfg)))
}

#[pg_extern(name = "st_updatepyramid")]
fn st_update_pyramid(
    name: &str,
    where_clause: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    validate_identifier(name)?;

    let Some((source_table, geom_column, fid_column, cfg)) = catalog_meta(name)? else {
        return Ok(false);
    };

    let total = if where_clause.trim().is_empty() {
        build::build(name, &source_table, &geom_column, &fid_column, &cfg)?
    } else {
        crate::incremental::update(
            name,
            &source_table,
            &geom_column,
            &fid_column,
            &cfg,
            where_clause,
        )?
    };
    Spi::run_with_args(
        "UPDATE geometry_pyramid_catalog SET built_at = now() WHERE pyramid_name = $1",
        &[name.into()],
    )?;
    if where_clause.trim().is_empty() {
        notice!(
            "geometry_pyramid: pyramid '{}' refreshed ({} tiles)",
            name,
            total
        );
    } else {
        notice!(
            "geometry_pyramid: pyramid '{}' incrementally refreshed ({} tiles)",
            name,
            total
        );
    }
    Ok(true)
}

/// `ST_UpdatePyramidByExtent(name, xmin, ymin, xmax, ymax, srid) -> bool`
///
/// Range-based incremental refresh. Re-slices every tile intersecting the
/// given bounding box — interpreted in `srid` and transformed to the pyramid's
/// tileSRS — from the CURRENT source data, across minZoom..=maxZoom, in the
/// caller's transaction (failure rolls back).
///
/// Unlike `ST_UpdatePyramid(name, where_clause)`, the rebuilt region is defined
/// by the caller's bbox rather than by rows a clause matches *now*, so it
/// correctly reflects inserts, updates, deletes, and cross-tile moves within
/// that region — the pure-delete / move-out cases the where_clause path cannot
/// clean. Returns `false` for an unknown pyramid; `true` on success (including a
/// degenerate bbox that maps to no tiles).
#[pg_extern(name = "st_updatepyramidbyextent")]
fn st_update_pyramid_by_extent(
    name: &str,
    xmin: f64,
    ymin: f64,
    xmax: f64,
    ymax: f64,
    srid: i32,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    validate_identifier(name)?;
    let Some((source_table, geom_column, fid_column, cfg)) = catalog_meta(name)? else {
        return Ok(false);
    };
    let total = crate::incremental::update_extent(
        name,
        &source_table,
        &geom_column,
        &fid_column,
        &cfg,
        xmin,
        ymin,
        xmax,
        ymax,
        srid,
    )?;
    Spi::run_with_args(
        "UPDATE geometry_pyramid_catalog SET built_at = now() WHERE pyramid_name = $1",
        &[name.into()],
    )?;
    notice!(
        "geometry_pyramid: pyramid '{}' range-refreshed ({} tiles)",
        name,
        total
    );
    Ok(true)
}

/// `ST_Pyramid(name) -> jsonb` — metadata for a single pyramid, including
/// tile count.
#[pg_extern(name = "st_pyramid")]
fn st_pyramid_info(name: &str) -> Result<Option<pgrx::JsonB>, spi::Error> {
    // An invalid identifier can never name a real pyramid, and guards the
    // dynamic count(*) query below against SQL identifier injection.
    if validate_identifier(name).is_err() {
        return Ok(None);
    }
    let storage_table = storage::tile_table_name(name);
    // Only count when the storage table exists. An unknown or
    // registered-but-unbuilt pyramid has no table; counting it directly would
    // raise "relation does not exist" (a longjmp that .unwrap_or cannot catch).
    let reg: Option<String> = Spi::get_one_with_args(
        "SELECT to_regclass($1)::text",
        &[format!("\"{storage_table}\"").into()],
    )?;
    let tile_count: i64 = if reg.is_some() {
        Spi::get_one(&format!(r#"SELECT count(*)::bigint FROM "{storage_table}""#))?.unwrap_or(0)
    } else {
        0
    };

    // MAX(...) guarantees exactly one row (NULL when the pyramid is unknown),
    // so get_one_with_args never sees a 0-row result (which pgrx 0.16 errors on
    // in a nested SPI context). NULL → Ok(None).
    let row: Option<String> = Spi::get_one_with_args(
        "SELECT MAX(jsonb_build_object( \
            'name', pyramid_name, \
            'source_table', source_table, \
            'geom_column', geom_column, \
            'fid_column', fid_column, \
            'config', config, \
            'built_at', built_at, \
            'tile_count', $2::bigint, \
            'stats', (SELECT COALESCE(jsonb_agg(jsonb_build_object(\
                'z', s.z, \
                'tileCount', s.tile_count, \
                'totalBytes', s.total_bytes, \
                'maxTileBytes', s.max_tile_bytes, \
                'buildMs', s.build_ms, \
                'attempts', s.attempts) ORDER BY s.z), '[]'::jsonb) \
                FROM geometry_pyramid_stats s WHERE s.pyramid_name = $1) \
         )::text) \
         FROM geometry_pyramid_catalog WHERE pyramid_name = $1",
        &[name.into(), tile_count.into()],
    )?;
    Ok(row
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .map(pgrx::JsonB))
}

// ST_Pyramids() is defined in sql/bootstrap.sql as a SQL function over the
// catalog table — simpler and avoids pgrx's SRF return-shape constraints.

/// Reject identifiers that contain anything other than letters, digits or
/// underscores. SQL identifier injection guard for the dynamic SQL paths.
///
/// The 45-char cap reserves headroom for PG's 63-byte identifier limit:
/// the longest derived name is `_gp_tiles_`(10) + name(45) + `__shadow`(8) = 63.
pub(crate) fn validate_identifier(
    ident: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if ident.is_empty() {
        return Err("identifier must not be empty".into());
    }
    if ident.len() > 45 {
        return Err(format!("identifier too long: {ident}").into());
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

    #[test]
    fn rejects_identifier_longer_than_45() {
        assert!(validate_identifier(&"a".repeat(45)).is_ok());
        assert!(validate_identifier(&"a".repeat(46)).is_err());
    }
}
