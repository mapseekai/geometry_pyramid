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

    // Dirty extent in tile SRS. The outer SELECT always returns exactly one
    // row (the aggregate produces one row even when no input rows match).
    // We use a CASE to return NULL when env is NULL, so Spi::get_one_with_args
    // returns Some(None) / None rather than 0 rows (which would panic).
    let sql = format!(
        r#"SELECT CASE WHEN env IS NOT NULL
                  THEN json_build_array(
                         ST_XMin(env), ST_YMin(env),
                         ST_XMax(env), ST_YMax(env))::text
                  ELSE NULL
                  END
           FROM (
              SELECT ST_Extent(ST_Transform("{geom_column}", $1)) AS env
              FROM "{source_table}"
              WHERE "{geom_column}" IS NOT NULL AND ({where_clause})
           ) e"#
    );
    let row: Option<String> = Spi::get_one_with_args(&sql, &[cfg.tile_srs.into()])?;
    let Some(s) = row else {
        notice!(
            "geometry_pyramid: where_clause matched no rows; pyramid '{}' unchanged",
            pyramid_name
        );
        return Ok(0);
    };
    let arr: [f64; 4] = serde_json::from_str(&s)?;
    let (xmin_m, ymin_m, xmax_m, ymax_m) = (arr[0], arr[1], arr[2], arr[3]);

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

        // Refresh this zoom's stats from the (partially rebuilt) table.
        Spi::run_with_args(
            &format!(
                r#"UPDATE geometry_pyramid_stats SET
                     tile_count     = (SELECT count(*) FROM "{storage_table}" WHERE z = $1),
                     total_bytes    = (SELECT COALESCE(sum(octet_length(mvt)),0) FROM "{storage_table}" WHERE z = $1),
                     max_tile_bytes = (SELECT COALESCE(max(octet_length(mvt)),0) FROM "{storage_table}" WHERE z = $1),
                     build_ms       = $2,
                     built_at       = now()
                   WHERE pyramid_name = $3 AND z = $1"#
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
