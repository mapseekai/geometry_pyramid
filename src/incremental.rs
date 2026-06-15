//! Incremental refresh: rebuild only the tiles whose source rows changed,
//! identified by a caller-supplied where_clause (Ganos semantics — the
//! caller holds table privileges; the clause is their SQL).

use crate::build;
use crate::config::PyramidConfig;
use crate::storage;
use crate::tile_rect::TileRect;
use pgrx::prelude::*;

/// Rebuild the tiles intersecting the extent of rows matched by
/// `where_clause`. Returns the number of tiles rebuilt. Runs entirely in the
/// caller's transaction: a failure rolls back to the intact pyramid.
pub fn update(
    pyramid_name: &str,
    source_table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
    where_clause: &str,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    // Defensive pre-flight: surface a syntax / unknown-column error cleanly
    // before it is embedded in the larger extent query.
    Spi::run(&format!(
        r#"SELECT 1 FROM "{source_table}" WHERE ({where_clause}) LIMIT 0"#
    ))?;

    // Dirty extent in tile SRS over the matched rows. bbox_from_query applies
    // the CASE→NULL guard, so an empty match is Ok(None) here, not a 0-row error.
    let inner = format!(
        r#"SELECT ST_Extent(ST_Transform("{geom_column}", $1)) AS g
           FROM "{source_table}"
           WHERE "{geom_column}" IS NOT NULL AND ({where_clause})"#
    );
    let Some(arr) = build::bbox_from_query(&inner, &[cfg.tile_srs.into()])? else {
        notice!(
            "geometry_pyramid: where_clause matched no rows; pyramid '{}' unchanged",
            pyramid_name
        );
        return Ok(0);
    };

    rebuild_extent(
        pyramid_name,
        source_table,
        geom_column,
        fid_column,
        cfg,
        arr[0],
        arr[1],
        arr[2],
        arr[3],
    )
}

/// Rebuild every tile intersecting the given tile-SRS bbox, across
/// minZoom..=maxZoom: DELETE the dirty rectangle then re-slice it in place,
/// refreshing per-zoom stats. Runs in the caller's transaction; rollback-safe.
pub(crate) fn rebuild_extent(
    pyramid_name: &str,
    source_table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
    xmin_m: f64,
    ymin_m: f64,
    xmax_m: f64,
    ymax_m: f64,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    let storage_table = storage::tile_table_name(pyramid_name);
    let mut rebuilt: i64 = 0;
    for z in cfg.min_zoom..=cfg.max_zoom {
        let started = std::time::Instant::now();
        let rect = TileRect::from_bbox_m(z, xmin_m, ymin_m, xmax_m, ymax_m);
        // Wipe the dirty rectangle, then rebuild it in place. Budget retry is
        // intentionally NOT applied here: a partial MAX(octet_length) check
        // would be skewed by untouched tiles; reuse the knobs from config.
        Spi::run_with_args(
            &format!(
                r#"DELETE FROM "{storage_table}"
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
        let knobs = build::ZoomKnobs::from_config(cfg, z);
        let inserted = build::build_zoom_level(
            pyramid_name,
            &storage_table,
            source_table,
            geom_column,
            fid_column,
            cfg,
            &rect,
            &knobs,
        )?;
        rebuilt += inserted;

        // Refresh this zoom's stats from the (partially rebuilt) table. The
        // three aggregates come from a single FROM-subquery scan rather than
        // three correlated subqueries, so the zoom's rows are read once (the
        // same shape build.rs::zoom_stats already uses).
        Spi::run_with_args(
            &format!(
                r#"UPDATE geometry_pyramid_stats s SET
                     tile_count     = agg.cnt,
                     total_bytes    = agg.total_bytes,
                     max_tile_bytes = agg.max_tile_bytes,
                     build_ms       = $2,
                     built_at       = now()
                   FROM (
                     SELECT count(*)                              AS cnt,
                            COALESCE(sum(octet_length(mvt)), 0)   AS total_bytes,
                            COALESCE(max(octet_length(mvt)), 0)   AS max_tile_bytes
                     FROM "{storage_table}" WHERE z = $1
                   ) agg
                   WHERE s.pyramid_name = $3 AND s.z = $1"#
            ),
            &[
                (z as i32).into(),
                (i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)).into(),
                pyramid_name.into(),
            ],
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
    pyramid_name: &str,
    source_table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
    xmin: f64,
    ymin: f64,
    xmax: f64,
    ymax: f64,
    srid: i32,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
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
            cfg.tile_srs.into(),
        ],
    )?
    else {
        return Ok(0);
    };
    rebuild_extent(
        pyramid_name, source_table, geom_column, fid_column, cfg,
        arr[0], arr[1], arr[2], arr[3],
    )
}
