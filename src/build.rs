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

    let workers = cfg.parallel.max(1) - 1;
    if workers > 0 && cfg.tile_size_budget_bytes > 0 {
        // The budget retry loop wipes a zoom and rebuilds it within the
        // coordinator's uncommitted transaction. Under parallel>1 the workers
        // have already COMMITTED their strip rows, so a retry's DELETE is
        // invisible to the next round's workers and they would collide on the
        // (z,x,y) primary key. Reject the combination rather than corrupt.
        return Err(
            "tileSizeBudgetBytes (budget retry) is not supported with parallel > 1; \
             set one of them back to its default"
                .into(),
        );
    }
    if workers == 0 {
        // parallel=1: shadow lives in this transaction (no external worker needs to see it).
        crate::shadow::create(pyramid_name)?;
    } else {
        // parallel>1: workers are independent backends, so the shadow DDL must be
        // COMMITTED before they can see it — run it via an autonomous worker.
        crate::worker::exec_sql_autonomous(&crate::shadow::create_sql(pyramid_name))?;
    }
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

        // NOTE: budget-retry with parallel>1 may not fully reset committed worker
        // rows across attempts (the DELETE runs in the coordinator txn and is
        // visible to subsequent worker txns only after coordinator commit). For
        // the default tile_size_budget_bytes=0 there are no retries, so this is
        // a non-issue in the common path.
        run_zoom_strips(
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

/// Execute one zoom round, splitting the rectangle into x-strips and farming
/// them to background workers when cfg.parallel > 1. Slot 0 always runs inline
/// in the coordinator; a failed spawn degrades that slot to inline execution.
/// Returns the total tile count for the zoom (recounted from the table so it
/// includes rows committed by workers).
#[allow(clippy::too_many_arguments)]
fn run_zoom_strips(
    pyramid_name: &str,
    storage_table: &str,
    table: &str,
    geom_column: &str,
    fid_column: &str,
    cfg: &PyramidConfig,
    rect: &TileRect,
    knobs: &ZoomKnobs,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    let workers = cfg.parallel.max(1) - 1;
    if workers == 0 {
        build_zoom_level(
            pyramid_name,
            storage_table,
            table,
            geom_column,
            fid_column,
            cfg,
            rect,
            knobs,
        )?;
        return Ok(inserted_count(storage_table, rect.z)?);
    }

    let strips = rect.split_x(4 * (workers + 1));
    let slots = (workers as usize + 1).min(strips.len());

    let mut spec_paths: Vec<std::path::PathBuf> = Vec::new();
    let mut handles: Vec<pgrx::bgworkers::DynamicBackgroundWorker> = Vec::new();
    let mut inline_strips: Vec<TileRect> = Vec::new();

    for slot in 0..slots {
        let chunk: Vec<TileRect> = strips.iter().copied().skip(slot).step_by(slots).collect();
        if slot == 0 {
            inline_strips = chunk;
            continue;
        }
        // Use a process-unique index, not `slot`: slot numbers repeat every
        // zoom round, so a worker slow to start could otherwise read a spec
        // file already overwritten by a later round.
        let filename = crate::worker::spec_filename(
            unsafe { pg_sys::MyProcPid } as u32,
            crate::worker::next_spec_idx(),
        );
        let spec = crate::worker::WorkerSpec {
            db_oid: unsafe { pg_sys::MyDatabaseId }.to_u32(),
            mode: crate::worker::WorkerMode::BuildStrips {
                pyramid_name: pyramid_name.to_string(),
                storage_table: storage_table.to_string(),
                source_table: table.to_string(),
                geom_column: geom_column.to_string(),
                fid_column: fid_column.to_string(),
                config: cfg.clone(),
                knobs: knobs.to_values(),
                strips: chunk.clone(),
            },
        };
        let path = crate::worker::write_spec(&filename, &spec)?;
        match crate::worker::spawn(&filename) {
            Ok(handle) => {
                spec_paths.push(path);
                handles.push(handle);
            }
            Err(e) => {
                notice!(
                    "geometry_pyramid: worker slot {} unavailable ({}); running inline",
                    slot,
                    e
                );
                crate::worker::cleanup_spec(&path);
                inline_strips.extend(chunk);
            }
        }
    }

    // Coordinator builds its own share while workers run.
    for strip in &inline_strips {
        build_zoom_level(
            pyramid_name,
            storage_table,
            table,
            geom_column,
            fid_column,
            cfg,
            strip,
            knobs,
        )?;
    }

    let wait_result = crate::worker::wait_for(&spec_paths, cfg.build_timeout_ms);
    for p in &spec_paths {
        crate::worker::cleanup_spec(p);
    }
    // Keep worker handles alive until after wait_for; drop them now.
    drop(handles);
    wait_result?;

    Ok(inserted_count(storage_table, rect.z)?)
}

/// Attribute projection fragments for the feature pipeline: (inner, outer).
/// Both are comma-prefixed (`, src."col" AS "col"` / `, prep."col"`) and may be
/// empty; they are spliced after a fixed column list. Column names are
/// revalidated defensively because they ride into dynamic SQL — a name failing
/// the validation is skipped with a notice rather than silently dropped.
pub(crate) fn attribute_projections(cfg: &PyramidConfig) -> (String, String) {
    let mut inner = String::new();
    let mut outer = String::new();
    for col in &cfg.attributes {
        if col.is_empty() || !col.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            notice!(
                "geometry_pyramid: attribute column {:?} is empty or has invalid characters; skipped",
                col
            );
            continue;
        }
        inner.push_str(&format!(r#", src."{col}" AS "{col}""#));
        outer.push_str(&format!(r#", prep."{col}""#));
    }
    (inner, outer)
}

/// The shared `FROM … features` block of the MVT pipeline: a 2-D tile grid
/// (`generate_series`, params $4..$7) joined to the per-tile feature pipeline
/// (projection, per-zoom simplification, min-size + point-drop filters,
/// `ST_AsMVTGeom`). References params $1,$3,$8,$9,$10,$11..$14 and tile coords
/// t.x/t.y — never $2 (layer name) and no GROUP BY. Both the multi-tile build
/// INSERT and the single-tile dynamic SELECT wrap this with
/// `ST_AsMVT(features.*, $2, $3, 'geom','fid')` + `GROUP BY t.x, t.y`.
/// A single tile is just the degenerate range xmin=xmax / ymin=ymax.
pub(crate) fn tile_pipeline_from_sql(
    table: &str,
    geom_column: &str,
    fid_column: &str,
    attrs_inner: &str,
    attrs_outer: &str,
) -> String {
    format!(
        r#"FROM generate_series($4::int, $5::int) AS xs(x)
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
                 ($12::float8 <= 0
                  OR GeometryType(prep.gtile) NOT IN ('POLYGON','MULTIPOLYGON')
                  OR ST_Area(prep.gtile) >= $12::float8)
                 AND ($13::float8 <= 0
                      OR GeometryType(prep.gtile) NOT IN ('LINESTRING','MULTILINESTRING')
                      OR ST_Length(prep.gtile) >= $13::float8)
                 AND ($14::float8 >= 1.0
                      OR GeometryType(prep.gtile) NOT IN ('POINT','MULTIPOINT')
                      OR (abs(hashtextextended(prep.fid::text, 0::bigint)) % 1000000)::float8
                         / 1000000.0 < $14::float8)
           ) features
           WHERE features.geom IS NOT NULL"#
    )
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

    let (attrs_inner, attrs_outer) = attribute_projections(cfg);

    let pipeline = tile_pipeline_from_sql(table, geom_column, fid_column, &attrs_inner, &attrs_outer);
    let sql = format!(
        r#"INSERT INTO "{storage_table}" (z, x, y, mvt)
           SELECT $1::smallint, t.x, t.y, ST_AsMVT(features.*, $2, $3, 'geom', 'fid')
           {pipeline}
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
