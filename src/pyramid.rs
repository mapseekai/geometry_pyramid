//! Top-level SQL surface for pyramid lifecycle management.

use crate::build;
use crate::config::{CatalogRow, PyramidConfig};
use crate::source::PyramidSource;
use crate::storage;
use pgrx::prelude::*;
use std::error::Error;

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
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let cfg = PyramidConfig::parse(config)?;
    validate_identifier(table)?;
    validate_identifier(geom_column)?;
    validate_identifier(fid_column)?;

    let pyramid_name = cfg.name.clone().unwrap_or_else(|| table.to_string());
    validate_identifier(&pyramid_name)?;

    // Single source of truth for config invariants (see PyramidConfig::validate).
    cfg.validate()?;

    let autonomous = build::autonomous_context();
    storage::lock_pyramid(&pyramid_name)?;
    let existing = catalog_meta(&pyramid_name)?;
    let (source, output) = if let Some(meta) = existing {
        if meta.source_table != table {
            return Err("changing a pyramid's source requires a new registration".into());
        }
        (
            meta.bindings
                .source
                .ok_or("registered source no longer exists")?,
            meta.bindings
                .storage
                .ok_or("registered tile table no longer exists")?,
        )
    } else {
        let source = storage::Relation::lookup(&spi::quote_identifier(table))?
            .ok_or("source relation does not exist")?;
        (source, storage::create_output(&pyramid_name)?)
    };
    source.lock()?;
    output.lock_output()?;
    let source_sql = source.sql();
    let src = PyramidSource {
        name: &pyramid_name,
        source_table: &source_sql,
        geom_column,
        fid_column,
        cfg: &cfg,
    };
    let total = build::build(src, &source, &output, autonomous)?;
    Spi::run_with_args(
        "INSERT INTO public.geometry_pyramid_catalog
         (pyramid_name, source_table, source_rel, storage_rel, geom_column, fid_column, config)
         VALUES ($1,$2,$3,$4,$5,$6,$7::jsonb)
         ON CONFLICT (pyramid_name) DO UPDATE SET geom_column=EXCLUDED.geom_column,
         fid_column=EXCLUDED.fid_column, config=EXCLUDED.config, built_at=now()",
        &[
            pyramid_name.as_str().into(),
            table.into(),
            pg_sys::Oid::from(source.oid).into(),
            pg_sys::Oid::from(output.oid).into(),
            geom_column.into(),
            fid_column.into(),
            serde_json::to_string(&cfg)?.into(),
        ],
    )?;
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
fn st_drop_pyramid(name: &str) -> Result<bool, Box<dyn Error + Send + Sync>> {
    validate_identifier(name)?;
    storage::lock_pyramid(name)?;
    let Some(bindings) = read_catalog::<storage::Bindings>(name)? else {
        return Ok(false);
    };
    bindings.validate(name)?;
    if let Some(output) = bindings.storage {
        output.drop_table()?;
        for work in [format!("_gp_shadow_{name}"), format!("_gp_prep_{name}")] {
            if bindings
                .source
                .as_ref()
                .is_some_and(|s| s.schema == output.schema && s.name == work)
            {
                continue;
            }
            let stage = spi::quote_qualified_identifier(&output.schema, &work);
            crate::shadow::discard_if_owned(&stage)?;
        }
    }
    crate::stats::delete_stats(name)?;
    Spi::run_with_args(
        "DELETE FROM public.geometry_pyramid_catalog WHERE pyramid_name=$1",
        &[name.into()],
    )?;
    Ok(true)
}

/// Resolve both registered OIDs without depending on the reader's search path.
fn read_catalog<T: serde::de::DeserializeOwned>(name: &str) -> Result<Option<T>, storage::DbError> {
    let row: Option<pgrx::JsonB> = Spi::get_one_with_args(
        "SELECT (SELECT jsonb_build_object(
            'source_table',c.source_table,'geom_column',c.geom_column,'fid_column',c.fid_column,
            'config',c.config,'bound',c.source_rel IS NOT NULL AND c.storage_rel IS NOT NULL,
            'source',CASE WHEN s.oid IS NOT NULL THEN jsonb_build_object('oid',s.oid::bigint,'schema',sn.nspname,'name',s.relname) END,
            'storage',CASE WHEN t.oid IS NOT NULL THEN jsonb_build_object('oid',t.oid::bigint,'schema',tn.nspname,'name',t.relname) END)
         FROM public.geometry_pyramid_catalog c
         LEFT JOIN pg_catalog.pg_class s ON s.oid=c.source_rel
         LEFT JOIN pg_catalog.pg_namespace sn ON sn.oid=s.relnamespace
         LEFT JOIN pg_catalog.pg_class t ON t.oid=c.storage_rel
         LEFT JOIN pg_catalog.pg_namespace tn ON tn.oid=t.relnamespace
         WHERE c.pyramid_name=$1)", &[name.into()],
    )?;
    Ok(row.map(|v| serde_json::from_value(v.0)).transpose()?)
}

pub(crate) fn catalog_meta(name: &str) -> Result<Option<CatalogRow>, storage::DbError> {
    let Some(meta) = read_catalog::<CatalogRow>(name)? else {
        return Ok(None);
    };
    meta.config.validate()?;
    validate_identifier(&meta.geom_column)?;
    validate_identifier(&meta.fid_column)?;
    meta.bindings.validate(name)?;
    Ok(Some(meta))
}

/// A non-empty clause is trusted predicate SQL evaluated with the caller's
/// privileges. For deleted or moved rows use the old extent as well.
#[pg_extern(name = "st_updatepyramid")]
fn st_update_pyramid(name: &str, where_clause: &str) -> Result<bool, Box<dyn Error + Send + Sync>> {
    validate_identifier(name)?;

    let autonomous = build::autonomous_context();
    storage::lock_pyramid(name)?;
    let Some(meta) = catalog_meta(name)? else {
        return Ok(false);
    };
    let source = meta
        .bindings
        .source
        .as_ref()
        .ok_or("registered source no longer exists")?;
    let output = meta
        .bindings
        .storage
        .as_ref()
        .ok_or("registered tile table no longer exists")?;
    source.lock()?;
    output.lock_output()?;
    let source_sql = source.sql();
    let src = meta.for_source(name, &source_sql);
    let total = if where_clause.trim().is_empty() {
        build::build(src, source, output, autonomous)?
    } else {
        crate::incremental::update(src, output, where_clause)?
    };
    Spi::run_with_args(
        "UPDATE public.geometry_pyramid_catalog SET built_at = now() WHERE pyramid_name = $1",
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
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    validate_identifier(name)?;
    storage::lock_pyramid(name)?;
    let Some(meta) = catalog_meta(name)? else {
        return Ok(false);
    };
    let source = meta
        .bindings
        .source
        .as_ref()
        .ok_or("registered source no longer exists")?;
    let output = meta
        .bindings
        .storage
        .as_ref()
        .ok_or("registered tile table no longer exists")?;
    source.lock()?;
    output.lock_output()?;
    let source_sql = source.sql();
    let src = meta.for_source(name, &source_sql);
    let total = crate::incremental::update_extent(src, output, xmin, ymin, xmax, ymax, srid)?;
    Spi::run_with_args(
        "UPDATE public.geometry_pyramid_catalog SET built_at = now() WHERE pyramid_name = $1",
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
    if validate_identifier(name).is_err() {
        return Ok(None);
    }
    Spi::get_one_with_args(
        "SELECT (SELECT jsonb_build_object(
            'name', c.pyramid_name, 'source_table', c.source_table, 'geom_column',c.geom_column,
            'fid_column',c.fid_column,'config',c.config,'built_at',c.built_at,
            'tile_count',summary.tiles,'stats',summary.zooms)
         FROM public.geometry_pyramid_catalog c
         CROSS JOIN LATERAL (
            SELECT COALESCE(sum(s.tile_count),0)::bigint AS tiles,
                   COALESCE(jsonb_agg(jsonb_build_object(
                       'z',s.z,'tileCount',s.tile_count,'totalBytes',s.total_bytes,
                       'maxTileBytes',s.max_tile_bytes,'buildMs',s.build_ms,'attempts',s.attempts)
                       ORDER BY s.z),'[]'::jsonb) AS zooms
            FROM public.geometry_pyramid_stats s WHERE s.pyramid_name=c.pyramid_name
         ) summary WHERE c.pyramid_name=$1)",
        &[name.into()],
    )
}

// ST_Pyramids() is defined in sql/bootstrap.sql as a SQL function over the
// catalog table — simpler and avoids pgrx's SRF return-shape constraints.

/// Reject identifiers that contain anything other than letters, digits or
/// underscores. SQL identifier injection guard for the dynamic SQL paths.
///
/// The 45-char cap reserves headroom for PG's 63-byte identifier limit:
/// the longest derived name is `_gp_shadow_`(11) + name(45) + `_id_seq`(7) = 63.
pub(crate) fn validate_identifier(ident: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
    if ident.is_empty() {
        return Err("identifier must not be empty".into());
    }
    if ident.len() > 45 {
        return Err(format!("identifier too long: {ident}").into());
    }
    if !ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
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
