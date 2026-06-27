//! Tile-index rectangles — the unit of work shared by the full,
//! incremental, and parallel build paths.

/// Web Mercator half-extent in metres.
pub const WEBMERC_HALF_EXTENT: f64 = 20_037_508.342_789_244;

/// Inclusive tile-index rectangle at a fixed zoom.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TileRect {
    pub z: u8,
    pub xmin: i64,
    pub ymin: i64,
    pub xmax: i64,
    pub ymax: i64,
}

impl TileRect {
    /// Convert a Web-Mercator metres bbox into the inclusive tile rectangle
    /// covering it at zoom `z`, clamped to `[0, 2^z - 1]`.
    pub fn from_bbox_m(z: u8, xmin_m: f64, ymin_m: f64, xmax_m: f64, ymax_m: f64) -> Self {
        let n: i64 = 1 << z;
        let tile_span = (2.0 * WEBMERC_HALF_EXTENT) / (n as f64);
        let to_tile_x = |m: f64| ((m + WEBMERC_HALF_EXTENT) / tile_span).floor() as i64;
        let to_tile_y = |m: f64| ((WEBMERC_HALF_EXTENT - m) / tile_span).floor() as i64;
        Self {
            z,
            xmin: to_tile_x(xmin_m).clamp(0, n - 1),
            ymin: to_tile_y(ymax_m).clamp(0, n - 1), // Y axis is inverted
            xmax: to_tile_x(xmax_m).clamp(0, n - 1),
            ymax: to_tile_y(ymin_m).clamp(0, n - 1),
        }
    }

    pub fn width(&self) -> i64 {
        self.xmax - self.xmin + 1
    }

    /// Number of tile cells in the rectangle — `R(z)` for the data bbox, i.e. how
    /// many cells the full-lattice tile-major build enumerates at this zoom.
    pub fn cell_count(&self) -> i64 {
        self.width() * (self.ymax - self.ymin + 1)
    }

    /// Split into at most `parts` disjoint x-strips that exactly cover
    /// `self`. Degrades to one strip per column when `parts > width`.
    pub fn split_x(&self, parts: u32) -> Vec<TileRect> {
        debug_assert!(
            self.xmin <= self.xmax,
            "TileRect invariant violated: xmin > xmax"
        );
        let parts = (parts as i64).clamp(1, self.width());
        let base = self.width() / parts;
        let extra = self.width() % parts;
        let mut out = Vec::with_capacity(parts as usize);
        let mut x = self.xmin;
        for i in 0..parts {
            let w = base + if i < extra { 1 } else { 0 };
            out.push(TileRect {
                z: self.z,
                xmin: x,
                ymin: self.ymin,
                xmax: x + w - 1,
                ymax: self.ymax,
            });
            x += w;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn z0_full_world_is_single_tile() {
        let r = TileRect::from_bbox_m(
            0,
            -WEBMERC_HALF_EXTENT,
            -WEBMERC_HALF_EXTENT,
            WEBMERC_HALF_EXTENT,
            WEBMERC_HALF_EXTENT,
        );
        assert_eq!((r.xmin, r.ymin, r.xmax, r.ymax), (0, 0, 0, 0));
    }

    #[test]
    fn z1_full_world_is_four_tiles() {
        let r = TileRect::from_bbox_m(
            1,
            -WEBMERC_HALF_EXTENT + 1.0,
            -WEBMERC_HALF_EXTENT + 1.0,
            WEBMERC_HALF_EXTENT - 1.0,
            WEBMERC_HALF_EXTENT - 1.0,
        );
        assert_eq!((r.xmin, r.ymin, r.xmax, r.ymax), (0, 0, 1, 1));
        let exact = TileRect::from_bbox_m(
            1,
            -WEBMERC_HALF_EXTENT,
            -WEBMERC_HALF_EXTENT,
            WEBMERC_HALF_EXTENT,
            WEBMERC_HALF_EXTENT,
        );
        assert_eq!(
            (exact.xmin, exact.ymin, exact.xmax, exact.ymax),
            (0, 0, 1, 1)
        );
    }

    #[test]
    fn out_of_range_is_clamped() {
        let r = TileRect::from_bbox_m(5, -1e9, -1e9, -9e8, -9e8);
        let n: i64 = 1 << 5;
        assert!(r.xmin >= 0 && r.xmax <= n - 1);
        assert!(r.ymin >= 0 && r.ymax <= n - 1);
        let r2 = TileRect::from_bbox_m(5, 1e9, 1e9, 2e9, 2e9);
        assert_eq!((r2.xmin, r2.ymin, r2.xmax, r2.ymax), (31, 0, 31, 0));
    }

    #[test]
    fn split_x_covers_exactly_without_overlap() {
        let r = TileRect {
            z: 10,
            xmin: 3,
            ymin: 0,
            xmax: 17,
            ymax: 5,
        };
        let parts = r.split_x(4);
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0].xmin, 3);
        assert_eq!(parts[3].xmax, 17);
        for w in parts.windows(2) {
            assert_eq!(w[0].xmax + 1, w[1].xmin);
        }
        let total: i64 = parts.iter().map(|p| p.width()).sum();
        assert_eq!(total, r.width());
        assert!(parts
            .iter()
            .all(|p| p.z == 10 && p.ymin == 0 && p.ymax == 5));
    }

    #[test]
    fn split_x_more_parts_than_width_degrades() {
        let r = TileRect {
            z: 3,
            xmin: 0,
            ymin: 0,
            xmax: 2,
            ymax: 2,
        }; // width 3
        let parts = r.split_x(10);
        assert_eq!(parts.len(), 3);
    }

    #[test]
    fn split_x_one_part_is_identity() {
        let r = TileRect {
            z: 3,
            xmin: 0,
            ymin: 0,
            xmax: 7,
            ymax: 7,
        };
        assert_eq!(r.split_x(1), vec![r]);
    }
}
