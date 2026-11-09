//! Incremental refresh: rebuild only the tiles whose source rows changed,
//! identified by a caller-supplied where_clause (Ganos semantics — the
//! caller holds table privileges; the clause is their SQL).

use crate::build;
use crate::source::PyramidSource;
use crate::storage::Relation;
use pgrx::prelude::*;
use std::error::Error;
use std::time::Instant;

/// Rebuild the tiles intersecting the extent of rows matched by
/// `where_clause`. Returns the number of tiles rebuilt. Runs entirely in the
/// caller's transaction: a failure rolls back to the intact pyramid.
pub fn update(
    src: PyramidSource,
    output: &Relation,
    where_clause: &str,
) -> Result<i64, Box<dyn Error + Send + Sync>> {
    // Defensive pre-flight: surface a syntax / unknown-column error cleanly
    // before it is embedded in the larger extent query.
    Spi::run(&format!(
        r#"SELECT 1 FROM {} WHERE ({where_clause}) LIMIT 0"#,
        src.source_table
    ))?;

    // Dirty extent in tile SRS over the matched rows. bbox_from_query applies
    // the CASE→NULL guard, so an empty match is Ok(None) here, not a 0-row error.
    let inner = format!(
        r#"SELECT ST_Extent(ST_Transform("{geom}", $1)) AS g
           FROM {source}
           WHERE "{geom}" IS NOT NULL AND ({where_clause})"#,
        geom = src.geom_column,
        source = src.source_table,
    );
    let Some(arr) = build::bbox_from_query(&inner, &[src.cfg.tile_srs.into()])? else {
        notice!(
            "geometry_pyramid: where_clause matched no rows; pyramid '{}' unchanged",
            src.name
        );
        return Ok(0);
    };

    rebuild_extent(src, output, arr[0], arr[1], arr[2], arr[3])
}

/// Rebuild every tile intersecting the given tile-SRS bbox, across
/// minZoom..=maxZoom: DELETE the dirty rectangle then re-slice it in place,
/// refreshing per-zoom stats. Runs in the caller's transaction; rollback-safe.
pub(crate) fn rebuild_extent(
    src: PyramidSource,
    output: &Relation,
    xmin_m: f64,
    ymin_m: f64,
    xmax_m: f64,
    ymax_m: f64,
) -> Result<i64, Box<dyn Error + Send + Sync>> {
    let storage_table = output.sql();
    let mut rebuilt: i64 = 0;
    for z in src.cfg.min_zoom..=src.cfg.max_zoom {
        let started = Instant::now();
        let rect = build::build_rect(src.cfg, z, (xmin_m, ymin_m, xmax_m, ymax_m));
        let (saved, _) = crate::stats::effective_knobs(src, z)?;
        if src.cfg.tile_size_budget_bytes > 0 && saved.is_none() {
            return build::rebuild_local(src, output);
        }
        let knobs = saved.unwrap_or_else(|| build::ZoomKnobs::from_config(src.cfg, z));
        Spi::run_with_args(
            &format!(
                r#"DELETE FROM ONLY {storage_table}
                   WHERE z = $1 AND x BETWEEN $2 AND $3 AND y BETWEEN $4 AND $5"#
            ),
            &[
                (z as i32).into(),
                (rect.xmin as i32).into(),
                (rect.xmax as i32).into(),
                (rect.ymin as i32).into(),
                (rect.ymax as i32).into(),
            ],
        )?;
        let inserted = build::build_zoom_level(src, &storage_table, &rect, &knobs)?;
        rebuilt += inserted;

        let (tiles, total_bytes, max_tile_bytes) = build::zoom_stats(&storage_table, z)?;
        if src.cfg.tile_size_budget_bytes > 0 && max_tile_bytes > src.cfg.tile_size_budget_bytes {
            return build::rebuild_local(src, output);
        }
        crate::stats::upsert_zoom_stats(
            src.name,
            &build::ZoomBuildResult {
                z,
                tiles,
                total_bytes,
                max_tile_bytes,
                knobs,
                attempts: 1,
                build_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
            },
        )?;
    }
    Ok(rebuilt)
}

/// Rebuild the tiles intersecting the caller-supplied bbox. The bbox is given
/// in `srid`; it is transformed to the pyramid's tileSRS before computing the
/// dirty rectangle. Unlike the where_clause path this re-slices the region from
/// the CURRENT source, so it correctly reflects inserts, updates, deletes, and
/// moves there. Returns the number of tiles rebuilt.
pub fn update_extent(
    src: PyramidSource,
    output: &Relation,
    xmin: f64,
    ymin: f64,
    xmax: f64,
    ymax: f64,
    srid: i32,
) -> Result<i64, Box<dyn Error + Send + Sync>> {
    // Transform the caller bbox from `srid` into tileSRS, then take its four
    // edges. ST_MakeEnvelope normalizes min/max; bbox_from_query's CASE→NULL
    // guard turns a NULL transform result into a no-op Ok(0).
    let inner = "SELECT ST_Transform(ST_MakeEnvelope($1,$2,$3,$4,$5), $6) AS g";
    let Some(arr) = build::bbox_from_query(
        inner,
        &[
            xmin.into(),
            ymin.into(),
            xmax.into(),
            ymax.into(),
            srid.into(),
            src.cfg.tile_srs.into(),
        ],
    )?
    else {
        return Ok(0);
    };
    // Nonlinear projections and geographic intervals wrapping the dateline
    // can extend outside the box of the four transformed corners.
    let canonical_geographic = srid == 4326 && xmin >= -180.0 && xmax <= 180.0 && xmin <= xmax;
    if srid != 3857 && !canonical_geographic {
        return build::rebuild_local(src, output);
    }
    rebuild_extent(src, output, arr[0], arr[1], arr[2], arr[3])
}
