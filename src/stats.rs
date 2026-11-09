//! Per-zoom build statistics persistence.

use crate::build::{ZoomBuildResult, ZoomKnobs};
use crate::source::PyramidSource;
use crate::storage::DbError;
use pgrx::prelude::*;

pub fn upsert_zoom_stats(pyramid_name: &str, r: &ZoomBuildResult) -> Result<(), DbError> {
    Spi::run_with_args(
        "INSERT INTO public.geometry_pyramid_stats \
           (pyramid_name, z, tile_count, total_bytes, max_tile_bytes, build_ms, attempts, effective_knobs, built_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now()) \
         ON CONFLICT (pyramid_name, z) DO UPDATE SET \
           tile_count     = EXCLUDED.tile_count, \
           total_bytes    = EXCLUDED.total_bytes, \
           max_tile_bytes = EXCLUDED.max_tile_bytes, \
           build_ms       = EXCLUDED.build_ms, \
           attempts       = EXCLUDED.attempts, \
           effective_knobs = EXCLUDED.effective_knobs, \
           built_at       = now()",
        &[
            pyramid_name.into(),
            (r.z as i16).into(),
            r.tiles.into(),
            r.total_bytes.into(),
            r.max_tile_bytes.into(),
            r.build_ms.into(),
            (r.attempts as i32).into(),
            pgrx::JsonB(serde_json::to_value(r.knobs)?).into(),
        ],
    )?;
    Ok(())
}

pub fn delete_stats(pyramid_name: &str) -> Result<(), spi::Error> {
    Spi::run_with_args(
        "DELETE FROM public.geometry_pyramid_stats WHERE pyramid_name = $1",
        &[pyramid_name.into()],
    )
}

pub(crate) fn effective_knobs(
    src: PyramidSource,
    z: u8,
) -> Result<(Option<ZoomKnobs>, i32), DbError> {
    if src.cfg.tile_size_budget_bytes <= 0 || z < src.cfg.min_zoom || z > src.cfg.max_zoom {
        return Ok((None, 0));
    }
    let (value, attempts) = Spi::get_two_with_args::<pgrx::JsonB, i32>(
        "SELECT s.effective_knobs,s.attempts FROM (VALUES (1)) anchor(n)
         LEFT JOIN public.geometry_pyramid_stats s ON s.pyramid_name=$1 AND s.z=$2",
        &[src.name.into(), (z as i16).into()],
    )?;
    Ok((
        value.map(|v| serde_json::from_value(v.0)).transpose()?,
        attempts.unwrap_or(0),
    ))
}
