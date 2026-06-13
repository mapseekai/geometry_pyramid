//! Tile materialization for a registered pyramid.
//!
//! We delegate the heavy geometry work to PostGIS via SPI — `ST_TileEnvelope`,
//! `ST_AsMVTGeom`, and the `ST_AsMVT` aggregate are well-tested and avoid
//! re-implementing the MVT codec inside the extension. Our job is to drive
//! them in the right order.

use crate::config::PyramidConfig;
use crate::tile_rect::TileRect;
use pgrx::prelude::*;

/// Per-zoom build outcome, consumed by the stats layer.
#[derive(Clone, Copy, Debug)]
pub struct ZoomBuildResult {
    pub z: u8,
    pub tiles: i64,
    pub total_bytes: i64,
    pub max_tile_bytes: i64,
    pub build_ms: i64,
    pub attempts: u32,
}

/// (tile_count, total_bytes, max_tile_bytes) for one zoom of a tile table.
fn zoom_stats(
    storage_table: &str,
    z: u8,
) -> Result<(i64, i64, i64), Box<dyn std::error::Error + Send + Sync>> {
    let row: Option<String> = Spi::get_one_with_args(
        &format!(
            r#"SELECT json_build_array(
                   count(*),
                   COALESCE(sum(octet_length(mvt)), 0),
                   COALESCE(max(octet_length(mvt)), 0))::text
               FROM "{storage_table}" WHERE z = $1"#
        ),
        &[(z as i32).into()],
    )?;
    let s = row.ok_or("zoom_stats returned no row")?;
    let arr: [i64; 3] = serde_json::from_str(&s)?;
    Ok((arr[0], arr[1], arr[2]))
}

/// Total tile count emitted across all zoom levels.
///
/// Acquires a per-pyramid advisory transaction lock, writes all tiles into
/// a shadow table, then atomically swaps the shadow into place as the main
/// tile table. If anything fails before the swap, the transaction aborts and
/// the shadow DDL is rolled back — leaving the previous main table untouched.
pub fn build(
    pyramid_name: &str,
    table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    // Serialize concurrent builds of the same pyramid for this transaction.
    // The advisory-lock namespace is cluster-global (shared across all
    // databases); the `geometry_pyramid:` prefix makes a collision with an
    // unrelated lock astronomically unlikely (~1/2^63), and the lock is only
    // an optimization against concurrent builders — correctness still rests
    // on the transactional shadow swap.
    Spi::run_with_args(
        "SELECT pg_advisory_xact_lock(hashtextextended('geometry_pyramid:' || $1, 0))",
        &[pyramid_name.into()],
    )?;

    crate::shadow::create(pyramid_name)?;
    let shadow_table = crate::shadow::shadow_table_name(pyramid_name);

    let total = build_into(pyramid_name, &shadow_table, table, geom_column, fid_column, cfg)?;

    crate::shadow::swap(pyramid_name)?;
    Ok(total)
}

/// The build loop, writing into an arbitrary target table.
fn build_into(
    pyramid_name: &str,
    storage_table: &str,
    table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    let bbox = source_bbox(table, geom_column, cfg.tile_srs)?;
    let Some((xmin_m, ymin_m, xmax_m, ymax_m)) = bbox else {
        notice!(
            "geometry_pyramid: source table '{}' has no rows; pyramid '{}' is empty",
            table,
            pyramid_name
        );
        return Ok(0); // empty shadow still gets swapped: empty source -> empty pyramid
    };

    let mut total: i64 = 0;
    for z in cfg.min_zoom..=cfg.max_zoom {
        let rect = TileRect::from_bbox_m(z, xmin_m, ymin_m, xmax_m, ymax_m);
        let result = build_zoom_level_with_budget(
            pyramid_name,
            storage_table,
            table,
            geom_column,
            fid_column,
            cfg,
            &rect,
        )?;
        crate::stats::upsert_zoom_stats(pyramid_name, &result)?;
        total += result.tiles;
    }
    Ok(total)
}

/// Tippecanoe-style `--drop-densest-as-needed` loop. Build the zoom once,
/// then while any tile breaches the byte budget, wipe and rebuild with
/// tighter knobs (more simplification, larger minimum-feature size, lower
/// retention). Bounded by `max_tile_size_attempts`.
fn build_zoom_level_with_budget(
    pyramid_name: &str,
    storage_table: &str,
    table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
    rect: &TileRect,
) -> Result<ZoomBuildResult, Box<dyn std::error::Error + Send + Sync>> {
    let started = std::time::Instant::now();
    let z = rect.z;
    let pixel_m = cfg.pixel_size_m(z);
    let mut knobs = ZoomKnobs::from_config(cfg, z);
    let mut attempt: u32 = 0;

    loop {
        if attempt > 0 {
            // Re-build cleanly: wipe this zoom's tiles before re-inserting,
            // otherwise the (z,x,y) PK collides.
            Spi::run_with_args(
                &format!(r#"DELETE FROM "{storage_table}" WHERE z = $1"#),
                &[(z as i32).into()],
            )?;
        }

        build_zoom_level(
            pyramid_name,
            storage_table,
            table,
            geom_column,
            fid_column,
            cfg,
            rect,
            &knobs,
        )?;

        let (tiles, total_bytes, max_tile_bytes) = zoom_stats(storage_table, z)?;
        notice!(
            "geometry_pyramid: pyramid '{}' z={} attempt={} -> {} tiles",
            pyramid_name,
            z,
            attempt,
            tiles
        );
        if cfg.tile_size_budget_bytes <= 0
            || attempt >= cfg.max_tile_size_attempts
            || max_tile_bytes <= cfg.tile_size_budget_bytes
        {
            return Ok(ZoomBuildResult {
                z: rect.z,
                tiles,
                total_bytes,
                max_tile_bytes,
                build_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
                attempts: attempt + 1,
            });
        }

        // Pressure factor = √(overage). Square-rooted so each retry is a
        // measured tightening rather than a panic; the loop converges
        // geometrically across attempts.
        let factor = (max_tile_bytes as f64 / cfg.tile_size_budget_bytes as f64)
            .sqrt()
            .max(1.05);
        knobs.tighten(factor, pixel_m);
        notice!(
            "geometry_pyramid: pyramid '{}' z={} max_tile={} bytes > budget {} -> tighten x{:.2}",
            pyramid_name,
            z,
            max_tile_bytes,
            cfg.tile_size_budget_bytes,
            factor
        );
        attempt += 1;
    }
}

/// Per-zoom tunable thresholds. Decoupled from `PyramidConfig` so the
/// budget retry loop can ratchet them per attempt without mutating cfg.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ZoomKnobs {
    simplify_tol_m: f64,
    min_area_m2: f64,
    min_len_m: f64,
    drop_threshold: f64,
}

impl ZoomKnobs {
    pub(crate) fn from_config(cfg: &PyramidConfig, z: u8) -> Self {
        let (a, l) = cfg.min_size_thresholds_m(z);
        Self {
            simplify_tol_m: cfg.simplify_tol_m(z),
            min_area_m2: a,
            min_len_m: l,
            drop_threshold: cfg.drop_threshold_at(z),
        }
    }

    /// Apply `factor > 1` worth of pressure. If a knob was disabled (0 or
    /// 1.0), it is bootstrapped to a 1-pixel baseline so the next factor
    /// has something to bite on.
    fn tighten(&mut self, factor: f64, pixel_m: f64) {
        self.simplify_tol_m = self.simplify_tol_m.max(pixel_m) * factor;
        self.min_len_m = self.min_len_m.max(pixel_m) * factor;
        self.min_area_m2 = self.min_len_m * self.min_len_m;
        self.drop_threshold = (self.drop_threshold / factor).clamp(0.0, 1.0);
    }

    pub(crate) fn to_values(self) -> crate::worker::KnobValues {
        crate::worker::KnobValues {
            simplify_tol_m: self.simplify_tol_m,
            min_area_m2: self.min_area_m2,
            min_len_m: self.min_len_m,
            drop_threshold: self.drop_threshold,
        }
    }

    pub(crate) fn from_values(v: crate::worker::KnobValues) -> Self {
        Self {
            simplify_tol_m: v.simplify_tol_m,
            min_area_m2: v.min_area_m2,
            min_len_m: v.min_len_m,
            drop_threshold: v.drop_threshold,
        }
    }
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

pub(crate) fn build_zoom_level(
    pyramid_name: &str,
    storage_table: &str,
    table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
    rect: &TileRect,
    knobs: &ZoomKnobs,
) -> Result<i64, spi::Error> {
    let z = rect.z;
    let (xmin_t, ymin_t, xmax_t, ymax_t) = (rect.xmin, rect.ymin, rect.xmax, rect.ymax);

    // The four tippecanoe-style thresholds for this attempt. `cfg` is no
    // longer the source of truth — the budget retry loop owns them.
    let simplify_tol_m = knobs.simplify_tol_m;
    let min_area_m2 = knobs.min_area_m2;
    let min_len_m = knobs.min_len_m;
    let drop_threshold = knobs.drop_threshold;

    // Attribute projection — column names are revalidated defensively here
    // because they ride along into the dynamic SQL string.
    let mut attrs_inner = String::new();
    let mut attrs_outer = String::new();
    for col in &cfg.attributes {
        if !col.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        attrs_inner.push_str(&format!(r#", src."{col}" AS "{col}""#));
        attrs_outer.push_str(&format!(r#", prep."{col}""#));
    }

    let sql = format!(
        r#"INSERT INTO "{storage_table}" (z, x, y, mvt)
           SELECT $1::smallint, t.x, t.y,
                  ST_AsMVT(features.*, $2, $3, 'geom', 'fid')
           FROM generate_series($4::int, $5::int) AS xs(x)
           CROSS JOIN generate_series($6::int, $7::int) AS ys(y)
           CROSS JOIN LATERAL (SELECT xs.x AS x, ys.y AS y) t
           CROSS JOIN LATERAL (
              SELECT
                 ST_AsMVTGeom(prep.gtile,
                              ST_TileEnvelope($1, t.x, t.y),
                              $3, $9, true) AS geom,
                 prep.fid
                 {attrs_outer}
              FROM (
                 SELECT
                    -- Per-zoom simplification (skip points; they have no edges).
                    CASE
                       WHEN $11::float8 > 0
                            AND GeometryType(src."{geom_column}") NOT IN ('POINT','MULTIPOINT')
                          THEN ST_SimplifyPreserveTopology(
                                 ST_Transform(src."{geom_column}", $8),
                                 $11::float8)
                       ELSE ST_Transform(src."{geom_column}", $8)
                    END AS gtile,
                    src."{fid_column}"::bigint AS fid
                    {attrs_inner}
                 FROM "{table}" src
                 WHERE src."{geom_column}" IS NOT NULL
                   AND ST_Intersects(
                          src."{geom_column}",
                          ST_Transform(ST_TileEnvelope($1, t.x, t.y), $10)
                       )
              ) prep
              WHERE
                 -- Polygon area cutoff (only applied when threshold > 0).
                 ($12::float8 <= 0
                  OR GeometryType(prep.gtile) NOT IN ('POLYGON','MULTIPOLYGON')
                  OR ST_Area(prep.gtile) >= $12::float8)
                 -- Line length cutoff (only applied when threshold > 0).
                 AND ($13::float8 <= 0
                      OR GeometryType(prep.gtile) NOT IN ('LINESTRING','MULTILINESTRING')
                      OR ST_Length(prep.gtile) >= $13::float8)
                 -- Tippecanoe-style deterministic point dropping.
                 AND ($14::float8 >= 1.0
                      OR GeometryType(prep.gtile) NOT IN ('POINT','MULTIPOINT')
                      OR (abs(hashtextextended(prep.fid::text, 0::bigint)) % 1000000)::float8
                         / 1000000.0 < $14::float8)
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
            simplify_tol_m.into(),
            min_area_m2.into(),
            min_len_m.into(),
            drop_threshold.into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tighten_bootstraps_disabled_knobs() {
        // User had everything off (defaults): simplify_pixels=1.0 ON,
        // min_feature_size_px=0 OFF, drop_rate=1.0 OFF. After one tighten
        // round all three should exert pressure.
        let cfg = PyramidConfig::default();
        let mut knobs = ZoomKnobs::from_config(&cfg, 10);
        let pixel_m = cfg.pixel_size_m(10);
        let initial_tol = knobs.simplify_tol_m;
        assert!((knobs.min_area_m2).abs() < 1e-9);
        assert!((knobs.drop_threshold - 1.0).abs() < 1e-9);

        knobs.tighten(2.0, pixel_m);

        assert!(knobs.simplify_tol_m > initial_tol);
        assert!(knobs.min_len_m >= pixel_m * 2.0 - 1e-9);
        assert!(knobs.min_area_m2 > 0.0);
        assert!(knobs.drop_threshold < 1.0 && knobs.drop_threshold > 0.0);
    }

    #[test]
    fn tighten_geometric_per_attempt() {
        let cfg = PyramidConfig::default();
        let pixel_m = cfg.pixel_size_m(10);
        let mut knobs = ZoomKnobs::from_config(&cfg, 10);
        knobs.tighten(2.0, pixel_m);
        let after_one = knobs.simplify_tol_m;
        knobs.tighten(2.0, pixel_m);
        // Second tighten should multiply again, not be capped.
        assert!(
            knobs.simplify_tol_m > after_one * 1.5,
            "expected geometric growth, got {} -> {}",
            after_one,
            knobs.simplify_tol_m
        );
    }
}
