//! Per-zoom build statistics persistence.

use crate::build::ZoomBuildResult;
use pgrx::prelude::*;

pub fn upsert_zoom_stats(pyramid_name: &str, r: &ZoomBuildResult) -> Result<(), spi::Error> {
    Spi::run_with_args(
        "INSERT INTO geometry_pyramid_stats \
           (pyramid_name, z, tile_count, total_bytes, max_tile_bytes, build_ms, attempts, built_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, now()) \
         ON CONFLICT (pyramid_name, z) DO UPDATE SET \
           tile_count     = EXCLUDED.tile_count, \
           total_bytes    = EXCLUDED.total_bytes, \
           max_tile_bytes = EXCLUDED.max_tile_bytes, \
           build_ms       = EXCLUDED.build_ms, \
           attempts       = EXCLUDED.attempts, \
           built_at       = now()",
        &[
            pyramid_name.into(),
            (r.z as i16).into(),
            r.tiles.into(),
            r.total_bytes.into(),
            r.max_tile_bytes.into(),
            r.build_ms.into(),
            (r.attempts as i32).into(),
        ],
    )
}

pub fn delete_stats(pyramid_name: &str) -> Result<(), spi::Error> {
    Spi::run_with_args(
        "DELETE FROM geometry_pyramid_stats WHERE pyramid_name = $1",
        &[pyramid_name.into()],
    )
}
