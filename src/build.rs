//! Tile materialization for a registered pyramid.
//!
//! We delegate the heavy geometry work to PostGIS via SPI — `ST_TileEnvelope`,
//! `ST_AsMVTGeom`, and the `ST_AsMVT` aggregate are well-tested and avoid
//! re-implementing the MVT codec inside the extension. Our job is to drive
//! them in the right order.

use crate::config::PyramidConfig;
use crate::storage;
use pgrx::prelude::*;

/// Web Mercator half-extent in metres. Used to convert between meter
/// coordinates and tile indices when computing per-zoom tile ranges.
const WEBMERC_HALF_EXTENT: f64 = 20_037_508.342_789_244;

/// Total tile count emitted across all zoom levels.
pub fn build(
    pyramid_name: &str,
    table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    storage::reset_tile_table(pyramid_name)?;
    let storage_table = storage::tile_table_name(pyramid_name);

    // Source bbox in tile SRS. NULL when the source table is empty — in
    // which case there is nothing to materialize.
    let bbox = source_bbox(table, geom_column, cfg.tile_srs)?;
    let Some((xmin_m, ymin_m, xmax_m, ymax_m)) = bbox else {
        notice!(
            "geometry_pyramid: source table '{}' has no rows; pyramid '{}' is empty",
            table,
            pyramid_name
        );
        return Ok(0);
    };

    let mut total: i64 = 0;
    for z in cfg.min_zoom..=cfg.max_zoom {
        let inserted = build_zoom_level(
            pyramid_name,
            &storage_table,
            table,
            geom_column,
            fid_column,
            cfg,
            z,
            xmin_m,
            ymin_m,
            xmax_m,
            ymax_m,
        )?;
        total += inserted;
        notice!(
            "geometry_pyramid: pyramid '{}' z={} -> {} tiles",
            pyramid_name,
            z,
            inserted
        );
    }
    Ok(total)
}

fn source_bbox(
    table: &str,
    geom_column: &str,
    tile_srs: i32,
) -> Result<Option<(f64, f64, f64, f64)>, Box<dyn std::error::Error + Send + Sync>> {
    // Identifiers are validated by callers. We pack the four bbox edges
    // into a JSON array string so we can use Spi::get_one (avoiding the
    // need for a multi-column SPI accessor).
    let sql = format!(
        r#"WITH e AS (
              SELECT ST_Extent(ST_Transform("{geom_column}", $1)) AS env
              FROM "{table}"
              WHERE "{geom_column}" IS NOT NULL
           )
           SELECT json_build_array(
                    ST_XMin(env), ST_YMin(env),
                    ST_XMax(env), ST_YMax(env)
                  )::text
           FROM e
           WHERE env IS NOT NULL"#
    );
    let row: Option<String> = Spi::get_one_with_args(&sql, &[tile_srs.into()])?;
    let Some(s) = row else { return Ok(None) };
    let arr: [f64; 4] = serde_json::from_str(&s)?;
    Ok(Some((arr[0], arr[1], arr[2], arr[3])))
}

#[allow(clippy::too_many_arguments)]
fn build_zoom_level(
    pyramid_name: &str,
    storage_table: &str,
    table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
    z: u8,
    xmin_m: f64,
    ymin_m: f64,
    xmax_m: f64,
    ymax_m: f64,
) -> Result<i64, spi::Error> {
    let (xmin_t, ymin_t, xmax_t, ymax_t) =
        bbox_to_tile_range(z, xmin_m, ymin_m, xmax_m, ymax_m);

    // Build a single set-based INSERT: for every tile in range, aggregate
    // intersecting source rows into one MVT blob. PostGIS does the
    // projection, clipping, quantization, and MVT encoding.
    let attrs_select = if cfg.attributes.is_empty() {
        String::new()
    } else {
        let mut s = String::new();
        for col in &cfg.attributes {
            // Reject hostile column names defensively.
            if !col.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            s.push_str(&format!(r#", src."{col}""#));
        }
        s
    };

    let sql = format!(
        r#"INSERT INTO "{storage_table}" (z, x, y, mvt)
           SELECT $1::smallint, t.x, t.y,
                  ST_AsMVT(features.*, $2, $3, 'geom', 'fid')
           FROM generate_series($4::int, $5::int) AS xs(x)
           CROSS JOIN generate_series($6::int, $7::int) AS ys(y)
           CROSS JOIN LATERAL (SELECT xs.x AS x, ys.y AS y) t
           CROSS JOIN LATERAL (
              SELECT
                 ST_AsMVTGeom(
                    ST_Transform(src."{geom_column}", $8),
                    ST_TileEnvelope($1, t.x, t.y),
                    $3, $9, true
                 ) AS geom,
                 src."{fid_column}"::bigint AS fid
                 {attrs_select}
              FROM "{table}" src
              WHERE src."{geom_column}" IS NOT NULL
                AND ST_Intersects(
                       src."{geom_column}",
                       ST_Transform(ST_TileEnvelope($1, t.x, t.y), $10)
                    )
           ) features
           WHERE features.geom IS NOT NULL
           GROUP BY t.x, t.y
           HAVING ST_AsMVT(features.*, $2, $3, 'geom', 'fid') IS NOT NULL"#
    );

    let inserted_before = inserted_count(storage_table, z)?;
    Spi::run_with_args(
        &sql,
        &[
            (z as i32).into(),
            pyramid_name.into(),
            (cfg.tile_size as i32).into(),
            (xmin_t as i32).into(),
            (xmax_t as i32).into(),
            (ymin_t as i32).into(),
            (ymax_t as i32).into(),
            cfg.tile_srs.into(),
            (cfg.tile_extend as i32).into(),
            cfg.source_srs.into(),
        ],
    )?;
    let inserted_after = inserted_count(storage_table, z)?;
    Ok(inserted_after - inserted_before)
}

fn inserted_count(storage_table: &str, z: u8) -> Result<i64, spi::Error> {
    Spi::get_one_with_args(
        &format!(r#"SELECT count(*)::bigint FROM "{storage_table}" WHERE z = $1"#),
        &[(z as i32).into()],
    )
    .map(|opt: Option<i64>| opt.unwrap_or(0))
}

/// Convert a Web-Mercator metres bbox to inclusive tile (x, y) ranges at
/// zoom `z`. Result is clamped to the valid tile range `[0, 2^z - 1]`.
fn bbox_to_tile_range(
    z: u8,
    xmin_m: f64,
    ymin_m: f64,
    xmax_m: f64,
    ymax_m: f64,
) -> (i64, i64, i64, i64) {
    let n: i64 = 1 << z;
    let tile_span = (2.0 * WEBMERC_HALF_EXTENT) / (n as f64);

    let to_tile_x = |m: f64| ((m + WEBMERC_HALF_EXTENT) / tile_span).floor() as i64;
    let to_tile_y = |m: f64| ((WEBMERC_HALF_EXTENT - m) / tile_span).floor() as i64;

    let xmin_t = to_tile_x(xmin_m).clamp(0, n - 1);
    let xmax_t = to_tile_x(xmax_m).clamp(0, n - 1);
    let ymin_t = to_tile_y(ymax_m).clamp(0, n - 1); // Y axis is inverted
    let ymax_t = to_tile_y(ymin_m).clamp(0, n - 1);
    (xmin_t, ymin_t, xmax_t, ymax_t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn z0_full_world_is_single_tile() {
        let r = bbox_to_tile_range(
            0,
            -WEBMERC_HALF_EXTENT,
            -WEBMERC_HALF_EXTENT,
            WEBMERC_HALF_EXTENT,
            WEBMERC_HALF_EXTENT,
        );
        assert_eq!(r, (0, 0, 0, 0));
    }

    #[test]
    fn z1_full_world_is_four_tiles() {
        let r = bbox_to_tile_range(
            1,
            -WEBMERC_HALF_EXTENT + 1.0,
            -WEBMERC_HALF_EXTENT + 1.0,
            WEBMERC_HALF_EXTENT - 1.0,
            WEBMERC_HALF_EXTENT - 1.0,
        );
        assert_eq!(r, (0, 0, 1, 1));
    }

    #[test]
    fn out_of_range_is_clamped() {
        // A tiny bbox far outside Web Mercator's valid extent should still
        // clamp into the valid index space rather than producing negatives.
        let r = bbox_to_tile_range(
            5,
            -1e9, -1e9, -9e8, -9e8,
        );
        let n: i64 = 1 << 5;
        assert!(r.0 >= 0 && r.2 <= n - 1);
        assert!(r.1 >= 0 && r.3 <= n - 1);
    }
}
