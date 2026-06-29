//! Tile materialization for a registered pyramid.
//!
//! We delegate the heavy geometry work to PostGIS via SPI — `ST_TileEnvelope`,
//! `ST_AsMVTGeom`, and the `ST_AsMVT` aggregate are well-tested and avoid
//! re-implementing the MVT codec inside the extension. Our job is to drive
//! them in the right order.

use crate::config::PyramidConfig;
use crate::source::PyramidSource;
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
pub fn build(src: PyramidSource) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    // Serialize concurrent builds of the same pyramid for this transaction.
    // The advisory-lock namespace is cluster-global (shared across all
    // databases); the `geometry_pyramid:` prefix makes a collision with an
    // unrelated lock astronomically unlikely (~1/2^63), and the lock is only
    // an optimization against concurrent builders — correctness still rests
    // on the transactional shadow swap.
    Spi::run_with_args(
        "SELECT pg_advisory_xact_lock(hashtextextended('geometry_pyramid:' || $1, 0))",
        &[src.name.into()],
    )?;

    let workers = src.cfg.effective_parallel() - 1;
    if workers > 0 && src.cfg.tile_size_budget_bytes > 0 {
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
        crate::shadow::create(src.name)?;
    } else {
        // parallel>1: workers are independent backends, so the shadow DDL must be
        // COMMITTED before they can see it — run it via an autonomous worker.
        crate::worker::exec_sql_autonomous(&crate::shadow::create_sql(src.name))?;
    }
    let shadow_table = crate::shadow::shadow_table_name(src.name);

    let total = build_into(src, &shadow_table)?;

    crate::shadow::swap(src.name)?;
    Ok(total)
}

/// The build loop, writing into an arbitrary target table.
fn build_into(
    src: PyramidSource,
    storage_table: &str,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    // Validate each LOD rule's filter against the src up front, so a bad
    // expr fails cleanly (naming the rule) before any prep/shadow work and
    // never leaves a partial pyramid. `AND false` makes it a pure parse/column
    // check that scans nothing.
    for (k, rule) in src.cfg.build_rules.iter().enumerate() {
        let probe = format!(
            r#"SELECT 1 FROM "{}" WHERE ({}) AND false LIMIT 1"#,
            src.source_table, rule.filter
        );
        Spi::run(&probe).map_err(|e| {
            Box::<dyn std::error::Error + Send + Sync>::from(format!(
                "buildRules[{k}] filter is invalid: {e}"
            ))
        })?;
    }

    let bbox = source_bbox(src.source_table, src.geom_column, src.cfg.tile_srs)?;
    let Some((xmin_m, ymin_m, xmax_m, ymax_m)) = bbox else {
        notice!(
            "geometry_pyramid: source table '{}' has no rows; pyramid '{}' is empty",
            src.source_table,
            src.name
        );
        return Ok(0); // empty shadow still gets swapped: empty source -> empty pyramid
    };

    let parallel = src.cfg.effective_parallel() > 1;
    let mut prepared = if uses_prepared_build(src.cfg) {
        let options = if config_allows_feature_major_points(src.cfg) {
            crate::prepared::PreparedBuildOptions::without_geom_tile_index()
        } else {
            crate::prepared::PreparedBuildOptions::with_geom_tile_index()
        };
        Some(if parallel {
            // Committed + worker-visible. The transform/index runs once here as
            // a serial prefix (it borrows the build's own timeout), then every
            // worker reads the result instead of re-transforming the source.
            crate::prepared::prepare_source_committed(src, options, src.cfg.build_timeout_ms)?
        } else {
            crate::prepared::prepare_source_for_build(src, options)?
        })
    } else {
        None
    };
    let feature_major_points = if let Some(prepared) = prepared.as_mut() {
        should_use_feature_major_points(src, prepared)?
    } else {
        false
    };

    // Feature-driven enumeration (the all-geometry generalization of
    // feature-major): decided once here. The prep check tells us whether it can
    // pay off at all; `rows` is hoisted out of the zoom loop because the per-zoom
    // gate consults it on every iteration. Feature-major (points) wins outright
    // when it applies, so the two are mutually exclusive.
    let (feature_driven_allowed, feature_driven_rows) = match prepared.as_ref() {
        Some(prepared) if !feature_major_points => {
            should_use_feature_driven(src, prepared, (xmin_m, ymin_m, xmax_m, ymax_m))?
        }
        _ => (false, 0),
    };

    let mut total: i64 = 0;
    for z in src.cfg.min_zoom..=src.cfg.max_zoom {
        let rect = TileRect::from_bbox_m(z, xmin_m, ymin_m, xmax_m, ymax_m);
        // Below the R(z) > C·rows crossover the dense tile-major path is as fast,
        // so feature-driven engages only on the sparse/high-zoom levels.
        let feature_driven =
            feature_driven_allowed && feature_driven_zoom_beneficial(&rect, feature_driven_rows);
        let result = build_zoom_level_with_budget(
            src,
            storage_table,
            &rect,
            prepared.as_ref(),
            feature_major_points,
            feature_driven,
        )?;
        crate::stats::upsert_zoom_stats(src.name, &result)?;
        total += result.tiles;
    }

    // The committed prepared table outlives the coordinator transaction, so it
    // must be dropped explicitly once all zooms are built (the temp variant is
    // reclaimed at commit). Dropped in the coordinator transaction — see
    // [`crate::prepared::drop_prepared_committed`] for why it must not be
    // autonomous.
    if parallel {
        if let Some(p) = prepared.as_ref() {
            crate::prepared::drop_prepared_committed(&p.table)?;
        }
    }
    Ok(total)
}

/// Whether to materialize a prepared (pre-transformed) source table. Now
/// parallel-agnostic: under `parallel>1` the table is committed and
/// worker-visible (see [`crate::prepared::prepare_source_committed`]) so the
/// workers can run the fast prepared/feature-driven path on their strips
/// instead of re-transforming the source per strip per zoom (the baseline).
fn uses_prepared_build(cfg: &PyramidConfig) -> bool {
    cfg.prepared_build
}

/// Feature-major *points* stays single-backend: its pipeline bins every point
/// arithmetically with no GiST prefilter, so a worker handed one x-strip would
/// still scan and bin the whole point set — `W` workers ⇒ `W×` the work, a net
/// loss. Under `parallel>1` a point source falls to the prepared tile-major or
/// (when enabled) feature-driven path, both of which are strip-bounded.
fn config_allows_feature_major_points(cfg: &PyramidConfig) -> bool {
    cfg.prepared_build
        && cfg.effective_parallel() == 1
        && cfg.feature_major_points
        && cfg.tile_srs == 3857
}

/// `featureMajorPoints` only pays off when the pyramid is deep enough that the
/// top zoom levels dominate the build. Feature-major cost is ~flat per zoom
/// (O(features)), while tile-major cost grows ~4x per zoom (O(R(z)) bbox-cell
/// enumerate + GiST-probe), so for shallow pyramids the many cheap low zooms —
/// where feature-major's fixed per-feature binning is the slower path — dominate
/// the build. The `featureMajorMinZoom` knob (default 13) gates this; below it we
/// fall back to prepared tile-major so shallow pyramids don't regress.
fn feature_major_zoom_sufficient(cfg: &PyramidConfig) -> bool {
    cfg.max_zoom >= cfg.feature_major_min_zoom
}

fn should_use_feature_major_points(
    src: PyramidSource,
    prepared: &mut crate::prepared::PreparedBuildSource,
) -> Result<bool, spi::Error> {
    if !config_allows_feature_major_points(src.cfg) {
        if src.cfg.feature_major_points && src.cfg.tile_srs != 3857 {
            notice!(
                "geometry_pyramid: featureMajorPoints=true requires tileSRS=3857; using prepared tile-major path"
            );
        } else if src.cfg.feature_major_points && src.cfg.effective_parallel() > 1 {
            // Explain the silent demotion: feature-major points is single-backend
            // (see `config_allows_feature_major_points`), so under parallel>1 the
            // build runs the worker-parallel prepared tile-major / feature-driven
            // path instead. Without this the user gets no signal their knob was
            // dropped. Print the resolved count so `parallel:0` (auto) reads true.
            notice!(
                "geometry_pyramid: featureMajorPoints=true is single-backend; under parallel={} using the worker-parallel prepared tile-major path instead",
                src.cfg.effective_parallel()
            );
        }
        return Ok(false);
    }

    if !feature_major_zoom_sufficient(src.cfg) {
        notice!(
            "geometry_pyramid: featureMajorPoints=true skipped — maxZoom {} is below featureMajorMinZoom {} where it pays off; using prepared tile-major path",
            src.cfg.max_zoom,
            src.cfg.feature_major_min_zoom
        );
        crate::prepared::ensure_geom_tile_index(prepared)?;
        return Ok(false);
    }

    if !prepared_table_is_point_only(prepared)? {
        notice!(
            "geometry_pyramid: featureMajorPoints=true requires a pure POINT source; using prepared tile-major path"
        );
        crate::prepared::ensure_geom_tile_index(prepared)?;
        return Ok(false);
    }

    notice!(
        "geometry_pyramid: featureMajorPoints=true enabled for pyramid '{}'",
        src.name
    );
    Ok(true)
}

fn prepared_table_is_point_only(
    prepared: &crate::prepared::PreparedBuildSource,
) -> Result<bool, spi::Error> {
    Spi::get_one::<bool>(&format!(
        r#"SELECT COALESCE(bool_and(geom_type = 'POINT'), false)
           FROM "{table}"
           WHERE geom_tile IS NOT NULL"#,
        table = prepared.table
    ))
    .map(|opt| opt.unwrap_or(false))
}

/// Per-zoom gate for feature-driven enumeration: engage only when the full tile
/// lattice `R(z)` exceeds this multiple of the source row count — i.e. the bbox
/// is sparse enough at this zoom that empty-cell enumeration dominates. Below it,
/// the dense tile-major path is as fast and avoids the per-feature explode. The
/// guard, not this constant, is what makes the knob safe, so it is not tuned hard.
const FEATURE_DRIVEN_MIN_FILL_RATIO: f64 = 2.0;

fn config_allows_feature_driven(cfg: &PyramidConfig) -> bool {
    uses_prepared_build(cfg) && cfg.feature_driven_enum && cfg.tile_srs == 3857
}

/// Cheap one-pass prep stats: `(row_count, feature_tile_pairs_at_maxZoom)`. The
/// second value is exactly what the feature-driven path would enumerate at the
/// most favorable zoom (largest `R(z)`); comparing it to `R(maxZoom)` decides
/// whether the explode is ever worth it.
fn feature_driven_prep_stats(
    prepared: &crate::prepared::PreparedBuildSource,
    max_zoom: u8,
) -> Result<(i64, i64), Box<dyn std::error::Error + Send + Sync>> {
    // `tw` (tile side in metres at maxZoom) is computed in SQL from the integer
    // zoom so no float is interpolated into the statement.
    let sql = format!(
        r#"SELECT json_build_array(
               count(*),
               COALESCE(sum(
                 (floor((ST_XMax(geom_tile) + 20037508.342789244) / tw.w)
                - floor((ST_XMin(geom_tile) + 20037508.342789244) / tw.w) + 1)
               * (floor((20037508.342789244 - ST_YMin(geom_tile)) / tw.w)
                - floor((20037508.342789244 - ST_YMax(geom_tile)) / tw.w) + 1)
               ), 0)::bigint)::text
           FROM "{table}",
                (SELECT 40075016.685578488 / power(2.0, {max_zoom}::float8) AS w) tw
           WHERE geom_tile IS NOT NULL"#,
        table = prepared.table
    );
    let row: Option<String> = Spi::get_one(&sql)?;
    let s = row.ok_or("feature_driven_prep_stats returned no row")?;
    let arr: [i64; 2] = serde_json::from_str(&s)?;
    Ok((arr[0], arr[1]))
}

/// Decide whether feature-driven enumeration may run for this build, returning
/// `(allowed, row_count)`. It is allowed only when `featureDrivenEnum` is on,
/// `tileSRS=3857`, and the envelope explode at maxZoom enumerates **fewer**
/// feature-tile pairs than the full lattice `R(maxZoom)`. That single check
/// rules out both dense extents (every cell occupied) and a handful of
/// bbox-spanning features (whose explode alone rivals the lattice) — so turning
/// the knob on is always safe; at worst it falls back to prepared tile-major.
fn should_use_feature_driven(
    src: PyramidSource,
    prepared: &crate::prepared::PreparedBuildSource,
    bbox_m: (f64, f64, f64, f64),
) -> Result<(bool, i64), Box<dyn std::error::Error + Send + Sync>> {
    if !src.cfg.feature_driven_enum {
        return Ok((false, 0));
    }
    if !config_allows_feature_driven(src.cfg) {
        if src.cfg.tile_srs != 3857 {
            notice!(
                "geometry_pyramid: featureDrivenEnum=true requires tileSRS=3857; using prepared tile-major path"
            );
        }
        return Ok((false, 0));
    }

    let (rows, pairs_at_maxzoom) = feature_driven_prep_stats(prepared, src.cfg.max_zoom)?;
    let (xmin_m, ymin_m, xmax_m, ymax_m) = bbox_m;
    let r_maxzoom =
        TileRect::from_bbox_m(src.cfg.max_zoom, xmin_m, ymin_m, xmax_m, ymax_m).cell_count();

    if rows <= 0 || pairs_at_maxzoom >= r_maxzoom {
        notice!(
            "geometry_pyramid: featureDrivenEnum=true skipped — envelope explode ({} feature-tile pairs at maxZoom) is not below the full lattice R({})={}; using prepared tile-major path",
            pairs_at_maxzoom,
            src.cfg.max_zoom,
            r_maxzoom
        );
        return Ok((false, rows.max(0)));
    }

    notice!(
        "geometry_pyramid: featureDrivenEnum=true enabled for pyramid '{}' (R({})={} vs {} feature-tile pairs at maxZoom)",
        src.name,
        src.cfg.max_zoom,
        r_maxzoom,
        pairs_at_maxzoom
    );
    Ok((true, rows))
}

/// Per-zoom gate: `R(z) > C·rows`. See [`FEATURE_DRIVEN_MIN_FILL_RATIO`].
fn feature_driven_zoom_beneficial(rect: &TileRect, rows: i64) -> bool {
    rows > 0 && (rect.cell_count() as f64) > FEATURE_DRIVEN_MIN_FILL_RATIO * (rows as f64)
}

/// Tippecanoe-style `--drop-densest-as-needed` loop. Build the zoom once,
/// then while any tile breaches the byte budget, wipe and rebuild with
/// tighter knobs (more simplification, larger minimum-feature size, lower
/// retention). Bounded by `max_tile_size_attempts`.
fn build_zoom_level_with_budget(
    src: PyramidSource,
    storage_table: &str,
    rect: &TileRect,
    prepared: Option<&crate::prepared::PreparedBuildSource>,
    feature_major_points: bool,
    feature_driven: bool,
) -> Result<ZoomBuildResult, Box<dyn std::error::Error + Send + Sync>> {
    let cfg = src.cfg;
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
            src,
            storage_table,
            rect,
            &knobs,
            prepared,
            feature_major_points,
            feature_driven,
        )?;

        let (tiles, total_bytes, max_tile_bytes) = zoom_stats(storage_table, z)?;
        notice!(
            "geometry_pyramid: pyramid '{}' z={} attempt={} -> {} tiles",
            src.name,
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
            src.name,
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

/// Run `inner_sql` — whose single output column must be a geometry/box aliased
/// `g` — and return its extent as `[xmin, ymin, xmax, ymax]`, or `None` when
/// `g` is NULL. Centralizes two things every extent query in the crate needs:
/// the `CASE WHEN g IS NOT NULL` guard (so the outer SELECT always returns one
/// row — pgrx errors on a 0-row `get_one` result in a nested SPI context) and
/// the `json_build_array(...)::text` packing the scalar SPI accessor relies on.
pub(crate) fn bbox_from_query(
    inner_sql: &str,
    args: &[pgrx::datum::DatumWithOid],
) -> Result<Option<[f64; 4]>, Box<dyn std::error::Error + Send + Sync>> {
    let sql = format!(
        "SELECT CASE WHEN g IS NOT NULL \
                THEN json_build_array(ST_XMin(g), ST_YMin(g), ST_XMax(g), ST_YMax(g))::text \
                ELSE NULL END \
         FROM ({inner_sql}) s"
    );
    let row: Option<String> = Spi::get_one_with_args(&sql, args)?;
    let Some(s) = row else { return Ok(None) };
    Ok(Some(serde_json::from_str(&s)?))
}

fn source_bbox(
    table: &str,
    geom_column: &str,
    tile_srs: i32,
) -> Result<Option<(f64, f64, f64, f64)>, Box<dyn std::error::Error + Send + Sync>> {
    // Identifiers are validated by callers.
    let inner = format!(
        r#"SELECT ST_Extent(ST_Transform("{geom_column}", $1)) AS g
           FROM "{table}" WHERE "{geom_column}" IS NOT NULL"#
    );
    Ok(bbox_from_query(&inner, &[tile_srs.into()])?.map(|a| (a[0], a[1], a[2], a[3])))
}

/// Run the chosen build path (baseline, prepared tile-major, feature-major
/// points, or feature-driven) for one tile rect. The single dispatch point
/// shared by the coordinator's inline path and the background workers, so a
/// worker strip is built byte-for-byte the way the coordinator would build it.
pub(crate) fn build_zoom_level_dispatch(
    src: PyramidSource,
    storage_table: &str,
    rect: &TileRect,
    knobs: &ZoomKnobs,
    prepared: Option<&crate::prepared::PreparedBuildSource>,
    feature_major_points: bool,
    feature_driven: bool,
) -> Result<i64, spi::Error> {
    match prepared {
        Some(prepared) if feature_major_points => build_zoom_level_prepared_points_feature_major(
            src,
            storage_table,
            rect,
            knobs,
            prepared,
        ),
        Some(prepared) if feature_driven => {
            build_zoom_level_prepared_feature_driven(src, storage_table, rect, knobs, prepared)
        }
        Some(prepared) => build_zoom_level_prepared(src, storage_table, rect, knobs, prepared),
        None => build_zoom_level(src, storage_table, rect, knobs),
    }
}

/// Below this many tile cells a zoom is too cheap to be worth worker spawn +
/// IPC; the coordinator builds it inline. Morton gridding targets the expensive
/// high-zoom levels where `R(z)` is large (the same regime feature-driven
/// engages), so the cheap shallow levels fall through to the coordinator.
const PARALLEL_MIN_CELLS: i64 = 64;

/// Divisions per axis for the Morton block grid: ~4 blocks per worker slot so
/// the interleaved block→worker assignment has enough granularity to balance a
/// clustered extent. `grid²` blocks total.
fn morton_grid(slots: usize) -> u32 {
    let target = (4 * slots) as f64;
    target.sqrt().ceil() as u32
}

/// Execute one zoom round, partitioning the rectangle into Morton (Z-order)
/// blocks and farming them to background workers when cfg.parallel > 1. Slot 0
/// always runs inline in the coordinator; a failed spawn degrades that slot to
/// inline execution. Each block is built via [`build_zoom_level_dispatch`], so
/// workers run the same fast prepared/feature-driven path the coordinator would
/// — clamped to their block's tile coordinates, which makes every (z,x,y) owned
/// by exactly one block (no overlap, no gap). Blocks are assigned to workers
/// **interleaved** in Morton order (block i → slot i mod slots) so a contiguous
/// dense core spreads across workers rather than landing on one. Returns the
/// total tile count for the zoom (recounted from the table so it includes rows
/// committed by workers).
fn run_zoom_strips(
    src: PyramidSource,
    storage_table: &str,
    rect: &TileRect,
    knobs: &ZoomKnobs,
    prepared: Option<&crate::prepared::PreparedBuildSource>,
    feature_major_points: bool,
    feature_driven: bool,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    let workers = src.cfg.effective_parallel() - 1;
    if workers == 0 || rect.cell_count() <= PARALLEL_MIN_CELLS {
        build_zoom_level_dispatch(
            src,
            storage_table,
            rect,
            knobs,
            prepared,
            feature_major_points,
            feature_driven,
        )?;
        return Ok(inserted_count(storage_table, rect.z)?);
    }

    let slots = workers as usize + 1;
    // Baseline tile-major keeps its proven x-strip partition (it re-scans the
    // source per tile via LATERAL, so 2-D blocking buys it little). The prepared
    // and feature-driven fast paths scan the prepared table, so they use Morton
    // blocks: a cluster confined to a sub-region then balances across workers.
    let blocks = if prepared.is_some() {
        rect.split_morton(morton_grid(slots))
    } else {
        rect.split_x(4 * slots as u32)
    };
    let slots = slots.min(blocks.len());
    // Workers read the committed prepared table by name; the index-free
    // feature-major-points path never reaches here (it stays parallel=1).
    let prepared_table = prepared.map(|p| p.table.clone());

    let mut spec_paths: Vec<std::path::PathBuf> = Vec::new();
    let mut handles: Vec<pgrx::bgworkers::DynamicBackgroundWorker> = Vec::new();
    let mut inline_strips: Vec<TileRect> = Vec::new();

    for slot in 0..slots {
        // Interleave in Morton order: slot s takes blocks s, s+slots, … so the
        // contiguous Morton run covering a dense core is split across slots.
        let chunk: Vec<TileRect> = blocks.iter().copied().skip(slot).step_by(slots).collect();
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
                pyramid_name: src.name.to_string(),
                storage_table: storage_table.to_string(),
                source_table: src.source_table.to_string(),
                geom_column: src.geom_column.to_string(),
                fid_column: src.fid_column.to_string(),
                config: src.cfg.clone(),
                knobs: knobs.to_values(),
                strips: chunk.clone(),
                prepared_table: prepared_table.clone(),
                feature_driven,
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
        build_zoom_level_dispatch(
            src,
            storage_table,
            strip,
            knobs,
            prepared,
            feature_major_points,
            feature_driven,
        )?;
    }

    let wait_result = crate::worker::wait_for(&spec_paths, src.cfg.build_timeout_ms);
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
pub(crate) fn attribute_projections_for(
    cfg: &PyramidConfig,
    inner_alias: &str,
    outer_alias: &str,
) -> (String, String) {
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
        inner.push_str(&format!(r#", {inner_alias}."{col}" AS "{col}""#));
        outer.push_str(&format!(r#", {outer_alias}."{col}""#));
    }
    (inner, outer)
}

pub(crate) fn attribute_projections(cfg: &PyramidConfig) -> (String, String) {
    attribute_projections_for(cfg, "src", "prep")
}

/// LOD rule predicate columns for the prepared-table CTAS: one boolean column
/// `_gp_rule_k` per rule, evaluated once from the src row. Empty when there
/// are no rules. Mirrors the inner fragment shape of `attribute_projections_for`.
pub(crate) fn rule_projections_for(cfg: &PyramidConfig) -> String {
    let mut s = String::new();
    for (k, rule) in cfg.build_rules.iter().enumerate() {
        s.push_str(&format!(r#", ({}) AS _gp_rule_{k}"#, rule.filter));
    }
    s
}

/// Which side a per-zoom rule clause targets: the prepared table (precomputed
/// `_gp_rule_k` booleans) or the raw src table (the inline filter expr).
#[derive(Clone, Copy, Debug)]
pub(crate) enum RuleTarget {
    Prepared,
    Source,
}

/// The ` AND …` clause restricting zoom `z` to features passing every rule whose
/// band covers `z`. Empty when no rule covers `z`. `Prepared` references the
/// precomputed `prep_src._gp_rule_k`; `src` inlines the raw filter (unqualified
/// columns resolve against the single src table in scope).
pub(crate) fn rules_clause_for_zoom(cfg: &PyramidConfig, z: u8, target: RuleTarget) -> String {
    let mut s = String::new();
    for (k, rule) in cfg.build_rules.iter().enumerate() {
        if z >= rule.min_zoom && z <= rule.max_zoom {
            match target {
                RuleTarget::Prepared => s.push_str(&format!(" AND prep_src._gp_rule_{k}")),
                RuleTarget::Source => s.push_str(&format!(" AND ({})", rule.filter)),
            }
        }
    }
    s
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
    rule_where: &str,
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
                   AND src."{geom_column}" && ST_Transform(
                          ST_TileEnvelope($1, t.x, t.y, margin => ($9::float8 / $3::float8)),
                          $10
                       ){rule_where}
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

/// Prepared-build variant of the shared `FROM ... features` block. The source
/// rows have already been transformed into tile SRS and indexed on `geom_tile`,
/// so per-tile work can use a direct Web-Mercator envelope filter.
pub(crate) fn tile_pipeline_from_prepared_sql(
    prepared_table: &str,
    attrs_inner: &str,
    attrs_outer: &str,
    rule_where: &str,
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
                            AND prep_src.geom_type NOT IN ('POINT','MULTIPOINT')
                          THEN ST_SimplifyPreserveTopology(prep_src.geom_tile, $11::float8)
                       ELSE prep_src.geom_tile
                    END AS gtile,
                    prep_src.geom_type,
                    prep_src.fid
                    {attrs_inner}
                 FROM "{prepared_table}" prep_src
                 WHERE prep_src.geom_tile && ST_TileEnvelope(
                    $1, t.x, t.y, margin => ($9::float8 / $3::float8)
                 ){rule_where}
              ) prep
              WHERE
                 ($12::float8 <= 0
                  OR prep.geom_type NOT IN ('POLYGON','MULTIPOLYGON')
                  OR ST_Area(prep.gtile) >= $12::float8)
                 AND ($13::float8 <= 0
                      OR prep.geom_type NOT IN ('LINESTRING','MULTILINESTRING')
                      OR ST_Length(prep.gtile) >= $13::float8)
                 AND ($14::float8 >= 1.0
                      OR prep.geom_type NOT IN ('POINT','MULTIPOINT')
                      OR (abs(hashtextextended(prep.fid::text, 0::bigint)) % 1000000)::float8
                         / 1000000.0 < $14::float8)
           ) features
           WHERE features.geom IS NOT NULL"#
    )
}

pub(crate) fn point_feature_major_pipeline_sql(
    prepared_table: &str,
    attrs_inner: &str,
    attrs_outer: &str,
    attrs_mvt: &str,
    rule_where: &str,
) -> String {
    format!(
        r#"WITH raw AS (
             SELECT
                floor((ST_X(prep_src.geom_tile) + 20037508.342789244)
                      / (40075016.685578488 / power(2.0, $1::float8)))::int AS x,
                floor((20037508.342789244 - ST_Y(prep_src.geom_tile))
                      / (40075016.685578488 / power(2.0, $1::float8)))::int AS y,
                prep_src.geom_tile,
                prep_src.fid
                {attrs_inner}
             FROM "{prepared_table}" prep_src
             WHERE prep_src.geom_type = 'POINT'
               AND ($14::float8 >= 1.0
                    OR (abs(hashtextextended(prep_src.fid::text, 0::bigint)) % 1000000)::float8
                       / 1000000.0 < $14::float8){rule_where}
           ),
           features AS (
             SELECT
                raw.x,
                raw.y,
                ST_AsMVTGeom(raw.geom_tile,
                             ST_TileEnvelope($1, raw.x, raw.y),
                             $3, $9, true) AS geom,
                raw.fid
                {attrs_outer}
             FROM raw
             WHERE raw.x BETWEEN $4::int AND $5::int
               AND raw.y BETWEEN $6::int AND $7::int
               AND raw.x >= 0
               AND raw.y >= 0
               AND raw.x < power(2.0, $1::float8)::int
               AND raw.y < power(2.0, $1::float8)::int
           ),
           grouped AS (
             SELECT f.x AS x, f.y AS y,
                    count(*) AS feature_count,
                    ST_AsMVT(mvt_features.*, $2, $3, 'geom', 'fid') AS mvt
             FROM features f
             CROSS JOIN LATERAL (
                SELECT f.geom, f.fid
                {attrs_mvt}
             ) mvt_features
             WHERE f.geom IS NOT NULL
             GROUP BY f.x, f.y
           )"#
    )
}

/// Feature-driven candidate-tile pipeline — the all-geometry generalization of
/// [`point_feature_major_pipeline_sql`]. Each prepared feature derives its own
/// candidate tile range from its envelope, expanded by the same `margin=$9/$3`
/// the tile-major `&&` prefilter uses (plus a ±1 float-safety pad), explodes it
/// with `generate_series`, and clamps to the build rect (`$4..$7`). Every step
/// after that — per-zoom simplify, min-size + point-drop filters, `ST_AsMVTGeom`,
/// the `IS NOT NULL` membership test, `ST_AsMVT` — is identical to
/// [`tile_pipeline_from_prepared_sql`], so the emitted tile set is provably the
/// same. Per-feature work (simplify + min-size filters) runs once here, before
/// the explode, rather than once per overlapping cell as in the tile-major path.
///
/// The four attribute fragments project the optional attribute columns through
/// the CTE chain: `prep_src -> prepped -> kept -> features(f)`.
pub(crate) fn feature_driven_pipeline_sql(
    prepared_table: &str,
    attrs_inner: &str,        // , prep_src."col" AS "col"
    attrs_from_prepped: &str, // , prepped."col"
    attrs_from_kept: &str,    // , kept."col"
    attrs_mvt: &str,          // , f."col"
    rule_where: &str,
) -> String {
    format!(
        r#"WITH prepped AS (
             SELECT
                CASE
                   WHEN $11::float8 > 0
                        AND prep_src.geom_type NOT IN ('POINT','MULTIPOINT')
                      THEN ST_SimplifyPreserveTopology(prep_src.geom_tile, $11::float8)
                   ELSE prep_src.geom_tile
                END AS gtile,
                prep_src.geom_type,
                prep_src.fid
                {attrs_inner},
                floor((ST_XMin(prep_src.geom_tile) - k.mm + 20037508.342789244) / k.tw)::int - 1 AS tx0,
                floor((ST_XMax(prep_src.geom_tile) + k.mm + 20037508.342789244) / k.tw)::int + 1 AS tx1,
                floor((20037508.342789244 - ST_YMax(prep_src.geom_tile) - k.mm) / k.tw)::int - 1 AS ty0,
                floor((20037508.342789244 - ST_YMin(prep_src.geom_tile) + k.mm) / k.tw)::int + 1 AS ty1
             FROM "{prepared_table}" prep_src
             CROSS JOIN (
                SELECT 40075016.685578488 / power(2.0, $1::float8) AS tw,
                       ($9::float8 / $3::float8)
                       * (40075016.685578488 / power(2.0, $1::float8)) AS mm
             ) k
             WHERE prep_src.geom_tile IS NOT NULL
               -- Strip read-filter: skip features whose envelope cannot reach
               -- any tile in the build rect [$4..$5]x[$6..$7], so a parallel
               -- worker GiST-scans only its block instead of every feature. The
               -- envelope is the rect's tile span in metres, padded by the
               -- margin mm plus 2 tiles -- strictly wider than the explode's
               -- mm +/- 1 tile, so it is a provable superset of what the
               -- explode+clamp keeps (cannot drop a tile; the exact membership
               -- test still runs below). On the serial full-rect build it spans
               -- the whole bbox and excludes nothing.
               AND prep_src.geom_tile && ST_MakeEnvelope(
                     $4::float8 * k.tw - 20037508.342789244 - k.mm - 2 * k.tw,
                     20037508.342789244 - ($7::float8 + 1) * k.tw - k.mm - 2 * k.tw,
                     ($5::float8 + 1) * k.tw - 20037508.342789244 + k.mm + 2 * k.tw,
                     20037508.342789244 - $6::float8 * k.tw + k.mm + 2 * k.tw,
                     $8)
               AND ($14::float8 >= 1.0
                    OR prep_src.geom_type NOT IN ('POINT','MULTIPOINT')
                    OR (abs(hashtextextended(prep_src.fid::text, 0::bigint)) % 1000000)::float8
                       / 1000000.0 < $14::float8){rule_where}
           ),
           kept AS (
             SELECT
                prepped.gtile,
                prepped.geom_type,
                prepped.fid
                {attrs_from_prepped},
                prepped.tx0, prepped.tx1, prepped.ty0, prepped.ty1
             FROM prepped
             WHERE
                ($12::float8 <= 0
                 OR prepped.geom_type NOT IN ('POLYGON','MULTIPOLYGON')
                 OR ST_Area(prepped.gtile) >= $12::float8)
                AND ($13::float8 <= 0
                     OR prepped.geom_type NOT IN ('LINESTRING','MULTILINESTRING')
                     OR ST_Length(prepped.gtile) >= $13::float8)
           ),
           features AS (
             SELECT
                gx AS x,
                gy AS y,
                ST_AsMVTGeom(kept.gtile, ST_TileEnvelope($1, gx, gy), $3, $9, true) AS geom,
                kept.fid
                {attrs_from_kept}
             FROM kept
             CROSS JOIN LATERAL
                generate_series(GREATEST(kept.tx0, $4::int), LEAST(kept.tx1, $5::int)) AS gx
             CROSS JOIN LATERAL
                generate_series(GREATEST(kept.ty0, $6::int), LEAST(kept.ty1, $7::int)) AS gy
             WHERE gx >= 0 AND gx < power(2.0, $1::float8)::int
               AND gy >= 0 AND gy < power(2.0, $1::float8)::int
           ),
           grouped AS (
             SELECT f.x AS x, f.y AS y,
                    count(*) AS feature_count,
                    ST_AsMVT(mvt_features.*, $2, $3, 'geom', 'fid') AS mvt
             FROM features f
             CROSS JOIN LATERAL (
                SELECT f.geom, f.fid
                {attrs_mvt}
             ) mvt_features
             WHERE f.geom IS NOT NULL
             GROUP BY f.x, f.y
           )"#
    )
}

pub(crate) fn build_zoom_level(
    src: PyramidSource,
    storage_table: &str,
    rect: &TileRect,
    knobs: &ZoomKnobs,
) -> Result<i64, spi::Error> {
    let cfg = src.cfg;
    let z = rect.z;
    let (xmin_t, ymin_t, xmax_t, ymax_t) = (rect.xmin, rect.ymin, rect.xmax, rect.ymax);

    // The four tippecanoe-style thresholds for this attempt. `cfg` is no
    // longer the source of truth — the budget retry loop owns them.
    let simplify_tol_m = knobs.simplify_tol_m;
    let min_area_m2 = knobs.min_area_m2;
    let min_len_m = knobs.min_len_m;
    let drop_threshold = knobs.drop_threshold;

    let (attrs_inner, attrs_outer) = attribute_projections(cfg);

    let rule_where = rules_clause_for_zoom(cfg, z, RuleTarget::Source);
    let pipeline = tile_pipeline_from_sql(
        src.source_table,
        src.geom_column,
        src.fid_column,
        &attrs_inner,
        &attrs_outer,
        &rule_where,
    );
    let split_size = cfg.split_size;
    // Aggregate each tile's MVT exactly once. The previous form computed
    // ST_AsMVT in both the SELECT and the HAVING, so the (expensive) tile
    // encode ran twice per group. Group once in a CTE, then filter the already
    // computed columns — same output, same splitSize semantics, half the encode.
    let sql = format!(
        r#"WITH grouped AS (
             SELECT t.x AS x, t.y AS y,
                    count(*) AS feature_count,
                    ST_AsMVT(features.*, $2, $3, 'geom', 'fid') AS mvt
             {pipeline}
             GROUP BY t.x, t.y
           )
           INSERT INTO "{storage_table}" (z, x, y, format, srid, mvt)
           SELECT $1::smallint, x, y, 'pbf'::text, $8::integer, mvt
           FROM grouped
           WHERE feature_count >= {split_size} AND mvt IS NOT NULL"#
    );

    let inserted_before = inserted_count(storage_table, z)?;
    Spi::run_with_args(
        &sql,
        &[
            (z as i32).into(),
            src.name.into(),
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

fn build_zoom_level_prepared(
    src: PyramidSource,
    storage_table: &str,
    rect: &TileRect,
    knobs: &ZoomKnobs,
    prepared: &crate::prepared::PreparedBuildSource,
) -> Result<i64, spi::Error> {
    let cfg = src.cfg;
    let z = rect.z;
    let (xmin_t, ymin_t, xmax_t, ymax_t) = (rect.xmin, rect.ymin, rect.xmax, rect.ymax);
    let simplify_tol_m = knobs.simplify_tol_m;
    let min_area_m2 = knobs.min_area_m2;
    let min_len_m = knobs.min_len_m;
    let drop_threshold = knobs.drop_threshold;
    let (attrs_inner, attrs_outer) = attribute_projections_for(cfg, "prep_src", "prep");

    let rule_where = rules_clause_for_zoom(cfg, z, RuleTarget::Prepared);
    let pipeline = tile_pipeline_from_prepared_sql(&prepared.table, &attrs_inner, &attrs_outer, &rule_where);
    let split_size = cfg.split_size;
    let sql = format!(
        r#"WITH grouped AS (
             SELECT t.x AS x, t.y AS y,
                    count(*) AS feature_count,
                    ST_AsMVT(features.*, $2, $3, 'geom', 'fid') AS mvt
             {pipeline}
             GROUP BY t.x, t.y
           )
           INSERT INTO "{storage_table}" (z, x, y, format, srid, mvt)
           SELECT $1::smallint, x, y, 'pbf'::text, $8::integer, mvt
           FROM grouped
           WHERE feature_count >= {split_size} AND mvt IS NOT NULL"#
    );

    let inserted_before = inserted_count(storage_table, z)?;
    Spi::run_with_args(
        &sql,
        &[
            (z as i32).into(),
            src.name.into(),
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

fn build_zoom_level_prepared_points_feature_major(
    src: PyramidSource,
    storage_table: &str,
    rect: &TileRect,
    knobs: &ZoomKnobs,
    prepared: &crate::prepared::PreparedBuildSource,
) -> Result<i64, spi::Error> {
    let cfg = src.cfg;
    let z = rect.z;
    let (xmin_t, ymin_t, xmax_t, ymax_t) = (rect.xmin, rect.ymin, rect.xmax, rect.ymax);
    let drop_threshold = knobs.drop_threshold;
    let (attrs_inner, attrs_outer) = attribute_projections_for(cfg, "prep_src", "raw");
    let (_, attrs_mvt) = attribute_projections_for(cfg, "prep_src", "f");
    let rule_where = rules_clause_for_zoom(cfg, z, RuleTarget::Prepared);
    let pipeline =
        point_feature_major_pipeline_sql(&prepared.table, &attrs_inner, &attrs_outer, &attrs_mvt, &rule_where);
    let split_size = cfg.split_size;
    let sql = format!(
        r#"{pipeline}
           INSERT INTO "{storage_table}" (z, x, y, format, srid, mvt)
           SELECT $1::smallint, x, y, 'pbf'::text, $8::integer, mvt
           FROM grouped
           WHERE feature_count >= {split_size}
             AND mvt IS NOT NULL"#
    );

    let inserted_before = inserted_count(storage_table, z)?;
    Spi::run_with_args(
        &sql,
        &[
            (z as i32).into(),
            src.name.into(),
            (cfg.tile_size as i32).into(),
            (xmin_t as i32).into(),
            (xmax_t as i32).into(),
            (ymin_t as i32).into(),
            (ymax_t as i32).into(),
            cfg.tile_srs.into(),
            (cfg.tile_extend as i32).into(),
            cfg.source_srs.into(),
            knobs.simplify_tol_m.into(),
            knobs.min_area_m2.into(),
            knobs.min_len_m.into(),
            drop_threshold.into(),
        ],
    )?;
    let inserted_after = inserted_count(storage_table, z)?;
    Ok(inserted_after - inserted_before)
}

fn build_zoom_level_prepared_feature_driven(
    src: PyramidSource,
    storage_table: &str,
    rect: &TileRect,
    knobs: &ZoomKnobs,
    prepared: &crate::prepared::PreparedBuildSource,
) -> Result<i64, spi::Error> {
    let cfg = src.cfg;
    let z = rect.z;
    let (xmin_t, ymin_t, xmax_t, ymax_t) = (rect.xmin, rect.ymin, rect.xmax, rect.ymax);
    let (attrs_inner, attrs_from_prepped) = attribute_projections_for(cfg, "prep_src", "prepped");
    let (_, attrs_from_kept) = attribute_projections_for(cfg, "x", "kept");
    let (_, attrs_mvt) = attribute_projections_for(cfg, "x", "f");
    let rule_where = rules_clause_for_zoom(cfg, z, RuleTarget::Prepared);
    let pipeline = feature_driven_pipeline_sql(
        &prepared.table,
        &attrs_inner,
        &attrs_from_prepped,
        &attrs_from_kept,
        &attrs_mvt,
        &rule_where,
    );
    let split_size = cfg.split_size;
    let sql = format!(
        r#"{pipeline}
           INSERT INTO "{storage_table}" (z, x, y, format, srid, mvt)
           SELECT $1::smallint, x, y, 'pbf'::text, $8::integer, mvt
           FROM grouped
           WHERE feature_count >= {split_size}
             AND mvt IS NOT NULL"#
    );

    let inserted_before = inserted_count(storage_table, z)?;
    Spi::run_with_args(
        &sql,
        &[
            (z as i32).into(),
            src.name.into(),
            (cfg.tile_size as i32).into(),
            (xmin_t as i32).into(),
            (xmax_t as i32).into(),
            (ymin_t as i32).into(),
            (ymax_t as i32).into(),
            cfg.tile_srs.into(),
            (cfg.tile_extend as i32).into(),
            cfg.source_srs.into(),
            knobs.simplify_tol_m.into(),
            knobs.min_area_m2.into(),
            knobs.min_len_m.into(),
            knobs.drop_threshold.into(),
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

    #[test]
    fn prepared_pipeline_reads_preprojected_geometry() {
        let sql = tile_pipeline_from_prepared_sql(
            "_gp_prep_42",
            r#", prep_src."kind" AS "kind""#,
            r#", prep."kind""#,
            "",
        );

        assert!(sql.contains(r#"FROM "_gp_prep_42" prep_src"#));
        assert!(sql.contains("prep_src.geom_tile && ST_TileEnvelope"));
        assert!(sql.contains("ST_SimplifyPreserveTopology(prep_src.geom_tile"));
        assert!(!sql.contains("ST_Transform"));
    }

    #[test]
    fn attribute_projections_can_target_aliases() {
        let cfg = PyramidConfig {
            attributes: vec!["kind".into()],
            ..Default::default()
        };

        let (inner, outer) = attribute_projections_for(&cfg, "prep_src", "prep");

        assert_eq!(inner, r#", prep_src."kind" AS "kind""#);
        assert_eq!(outer, r#", prep."kind""#);
    }

    #[test]
    fn prepared_build_enabled_whenever_requested() {
        let off = PyramidConfig::default();
        assert!(!uses_prepared_build(&off));

        let single = PyramidConfig {
            prepared_build: true,
            parallel: 1,
            ..Default::default()
        };
        assert!(uses_prepared_build(&single));

        // PR4: prepared build is now parallel-agnostic — under parallel>1 the
        // table is committed and worker-visible so workers run the fast path.
        let parallel = PyramidConfig {
            prepared_build: true,
            parallel: 2,
            ..Default::default()
        };
        assert!(uses_prepared_build(&parallel));
    }

    #[test]
    fn feature_major_points_stays_single_backend() {
        // Feature-major points must NOT engage under parallel>1 (each worker
        // would re-scan the whole point set). It falls back to the strip-bounded
        // prepared/feature-driven path instead.
        let single = PyramidConfig {
            prepared_build: true,
            feature_major_points: true,
            parallel: 1,
            tile_srs: 3857,
            ..Default::default()
        };
        assert!(config_allows_feature_major_points(&single));

        let parallel = PyramidConfig {
            parallel: 2,
            ..single.clone()
        };
        assert!(!config_allows_feature_major_points(&parallel));
        // ...but prepared build itself stays on under parallel.
        assert!(uses_prepared_build(&parallel));
    }

    #[test]
    fn feature_driven_allowed_under_parallel() {
        // Feature-driven IS strip-bounded (its candidate-tile explode clamps to
        // the build rect), so it's allowed to run in parallel workers.
        let parallel = PyramidConfig {
            prepared_build: true,
            feature_driven_enum: true,
            parallel: 2,
            tile_srs: 3857,
            ..Default::default()
        };
        assert!(config_allows_feature_driven(&parallel));
    }

    #[test]
    fn feature_major_points_requires_prepared_single_backend_web_mercator() {
        let enabled = PyramidConfig {
            prepared_build: true,
            feature_major_points: true,
            parallel: 1,
            tile_srs: 3857,
            ..Default::default()
        };
        assert!(config_allows_feature_major_points(&enabled));

        let without_prepared = PyramidConfig {
            prepared_build: false,
            ..enabled.clone()
        };
        assert!(!config_allows_feature_major_points(&without_prepared));

        let parallel = PyramidConfig {
            parallel: 2,
            ..enabled.clone()
        };
        assert!(!config_allows_feature_major_points(&parallel));

        let non_web_mercator = PyramidConfig {
            tile_srs: 4326,
            ..enabled
        };
        assert!(!config_allows_feature_major_points(&non_web_mercator));
    }

    #[test]
    fn feature_major_requires_sufficient_max_zoom() {
        let min = crate::config::DEFAULT_FEATURE_MAJOR_MIN_ZOOM;

        // Shallow pyramids regress under feature-major; gate them out.
        let shallow = PyramidConfig {
            max_zoom: min - 1,
            ..Default::default()
        };
        assert!(!feature_major_zoom_sufficient(&shallow));

        let at_threshold = PyramidConfig {
            max_zoom: min,
            ..Default::default()
        };
        assert!(feature_major_zoom_sufficient(&at_threshold));

        // Default pyramids are deep (maxZoom 18), so feature-major stays enabled.
        assert!(feature_major_zoom_sufficient(&PyramidConfig::default()));

        // The knob can force feature-major on for shallow pyramids (e.g. tests).
        let forced = PyramidConfig {
            max_zoom: 2,
            feature_major_min_zoom: 0,
            ..Default::default()
        };
        assert!(feature_major_zoom_sufficient(&forced));
    }

    #[test]
    fn point_feature_major_pipeline_does_not_enumerate_bbox_tiles() {
        let sql = point_feature_major_pipeline_sql("_gp_prep_test", "", "", "", "");
        assert!(
            !sql.contains("generate_series"),
            "point feature-major path must not enumerate bbox tile cells"
        );
        assert!(sql.contains("ST_X(prep_src.geom_tile)"));
        assert!(sql.contains("GROUP BY f.x, f.y"));
    }

    #[test]
    fn feature_driven_gate_requires_sparse_lattice() {
        // Dense extent: few cells, many rows -> tile-major is as fast, skip.
        let dense = TileRect {
            z: 6,
            xmin: 0,
            ymin: 0,
            xmax: 4,
            ymax: 4,
        }; // 25 cells
        assert!(!feature_driven_zoom_beneficial(&dense, 1000));

        // Sparse / high zoom: R(z) >> rows -> feature-driven pays off.
        let sparse = TileRect {
            z: 14,
            xmin: 0,
            ymin: 0,
            xmax: 999,
            ymax: 999,
        }; // 1e6 cells
        assert!(feature_driven_zoom_beneficial(&sparse, 30));

        // Right at the crossover the gate stays closed (strict `>` C·rows).
        let at_ratio = TileRect {
            z: 6,
            xmin: 0,
            ymin: 0,
            xmax: 9,
            ymax: 9,
        }; // 100 cells
        assert!(!feature_driven_zoom_beneficial(&at_ratio, 50)); // 100 == 2*50

        // No rows -> never (avoids div-by-nothing and a pointless explode).
        assert!(!feature_driven_zoom_beneficial(&sparse, 0));
    }

    #[test]
    fn feature_driven_pipeline_derives_tiles_from_envelopes() {
        let sql = feature_driven_pipeline_sql("_gp_prep_test", "", "", "", "", "");
        // Derives a candidate tile range per feature from its envelope...
        assert!(sql.contains("ST_XMin(prep_src.geom_tile)"));
        assert!(sql.contains("ST_YMax(prep_src.geom_tile)"));
        // ...prefilters the scan to the build rect via a GiST && so a parallel
        // worker reads only its block (a provable superset of the explode)...
        assert!(sql.contains("prep_src.geom_tile && ST_MakeEnvelope("));
        // ...then explodes + clamps to the build rect ($4..$7), never the full
        // unconditional lattice the tile-major path enumerates.
        assert!(sql.contains("generate_series(GREATEST(kept.tx0, $4::int)"));
        assert!(sql.contains("LEAST(kept.tx1, $5::int)"));
        // ...and reuses the identical clip + membership test for equivalence.
        assert!(sql.contains("ST_AsMVTGeom(kept.gtile, ST_TileEnvelope($1, gx, gy)"));
        assert!(sql.contains("WHERE f.geom IS NOT NULL"));
        assert!(sql.contains("GROUP BY f.x, f.y"));
    }

    #[test]
    fn rule_projections_number_by_index() {
        let cfg = PyramidConfig::parse(
            r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"filter":"a < 5"},
                              {"minZoom":5,"maxZoom":8,"filter":"b > 1"}]}"#,
        )
        .unwrap();
        let p = rule_projections_for(&cfg);
        assert!(p.contains("(a < 5) AS _gp_rule_0"), "got: {p}");
        assert!(p.contains("(b > 1) AS _gp_rule_1"), "got: {p}");
    }

    #[test]
    fn rule_projections_empty_when_no_rules() {
        assert_eq!(rule_projections_for(&PyramidConfig::default()), "");
    }

    #[test]
    fn rules_clause_prepared_ands_covering_booleans() {
        let cfg = PyramidConfig::parse(
            r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"filter":"a < 5"},
                              {"minZoom":3,"maxZoom":8,"filter":"b > 1"}]}"#,
        )
        .unwrap();
        assert_eq!(
            rules_clause_for_zoom(&cfg, 3, RuleTarget::Prepared),
            " AND prep_src._gp_rule_0 AND prep_src._gp_rule_1"
        );
        assert_eq!(
            rules_clause_for_zoom(&cfg, 6, RuleTarget::Prepared),
            " AND prep_src._gp_rule_1"
        );
        assert_eq!(rules_clause_for_zoom(&cfg, 10, RuleTarget::Prepared), "");
    }

    #[test]
    fn rules_clause_source_inlines_filter() {
        let cfg = PyramidConfig::parse(
            r#"{"buildRules":[{"minZoom":0,"maxZoom":4,"filter":"a < 5"}]}"#,
        )
        .unwrap();
        assert_eq!(
            rules_clause_for_zoom(&cfg, 2, RuleTarget::Source),
            " AND (a < 5)"
        );
    }
}
