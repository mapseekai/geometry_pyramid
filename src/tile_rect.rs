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

    /// Tiles whose closed envelopes intersect a bbox. Unlike assigning a point
    /// to its containing tile, a bbox on a grid line also touches its neighbour.
    pub fn intersecting_bbox_m(z: u8, xmin: f64, ymin: f64, xmax: f64, ymax: f64) -> Self {
        let mut rect = Self::from_bbox_m(z, xmin, ymin, xmax, ymax);
        let n = 1i64 << z;
        let span = 2.0 * WEBMERC_HALF_EXTENT / n as f64;
        rect.xmin = (((xmin + WEBMERC_HALF_EXTENT) / span).ceil() as i64)
            .saturating_sub(1)
            .clamp(0, n - 1);
        rect.ymin = (((WEBMERC_HALF_EXTENT - ymax) / span).ceil() as i64)
            .saturating_sub(1)
            .clamp(0, n - 1);
        rect
    }

    pub fn width(&self) -> i64 {
        self.xmax - self.xmin + 1
    }

    /// Number of tile cells in the rectangle — `R(z)` for the data bbox, i.e. how
    /// many cells the full-lattice tile-major build enumerates at this zoom.
    pub fn cell_count(&self) -> i64 {
        self.width() * (self.ymax - self.ymin + 1)
    }

    pub fn height(&self) -> i64 {
        self.ymax - self.ymin + 1
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

    /// Split into 2-D blocks ordered by the Morton (Z-order) code of their grid
    /// position. Unlike [`split_x`](Self::split_x) (X axis only), this grids
    /// both axes, so a cluster confined to a sub-region is divided across blocks
    /// in both dimensions; combined with the caller's interleaved block→worker
    /// assignment, a contiguous dense core spreads across workers instead of
    /// landing on one. `grid` is the divisions per axis, so the result has at
    /// most `grid²` blocks (fewer when an axis is narrower than `grid`).
    ///
    /// The blocks are a disjoint partition that **exactly covers** `self`: every
    /// (x, y) in `self` falls in exactly one block. Ownership is therefore
    /// tile-exact regardless of the Morton ordering — the ordering only decides
    /// which worker builds a block, never which tiles exist — so the emitted
    /// tile set is identical to any other partition (or to a serial build).
    pub fn split_morton(&self, grid: u32) -> Vec<TileRect> {
        debug_assert!(
            self.xmin <= self.xmax && self.ymin <= self.ymax,
            "TileRect invariant violated"
        );
        let gx = (grid as i64).clamp(1, self.width());
        let gy = (grid as i64).clamp(1, self.height());
        let x_edges = axis_edges(self.xmin, self.width(), gx);
        let y_edges = axis_edges(self.ymin, self.height(), gy);

        let mut blocks: Vec<(u64, TileRect)> = Vec::with_capacity((gx * gy) as usize);
        for (by, &(ylo, yhi)) in y_edges.iter().enumerate() {
            for (bx, &(xlo, xhi)) in x_edges.iter().enumerate() {
                blocks.push((
                    morton2(bx as u32, by as u32),
                    TileRect {
                        z: self.z,
                        xmin: xlo,
                        ymin: ylo,
                        xmax: xhi,
                        ymax: yhi,
                    },
                ));
            }
        }
        // Stable sort by Morton code yields Z-order traversal of the block grid.
        blocks.sort_by_key(|(code, _)| *code);
        blocks.into_iter().map(|(_, rect)| rect).collect()
    }
}

/// Balanced inclusive `[lo, hi]` ranges partitioning `[start, start+len)` into
/// `parts` pieces — the first `len % parts` get one extra cell (same scheme as
/// [`TileRect::split_x`]). `parts` must be in `1..=len`.
fn axis_edges(start: i64, len: i64, parts: i64) -> Vec<(i64, i64)> {
    let base = len / parts;
    let extra = len % parts;
    let mut out = Vec::with_capacity(parts as usize);
    let mut p = start;
    for i in 0..parts {
        let w = base + if i < extra { 1 } else { 0 };
        out.push((p, p + w - 1));
        p += w;
    }
    out
}

/// Morton (Z-order) code interleaving the bits of `x` and `y` (x in even bit
/// positions, y in odd). Inputs here are small block indices, so a plain
/// bit-walk is clear and ample.
fn morton2(mut x: u32, mut y: u32) -> u64 {
    let mut code: u64 = 0;
    let mut shift: u32 = 0;
    while x != 0 || y != 0 {
        code |= ((x & 1) as u64) << (2 * shift);
        code |= ((y & 1) as u64) << (2 * shift + 1);
        x >>= 1;
        y >>= 1;
        shift += 1;
    }
    code
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
        let outside = TileRect::intersecting_bbox_m(24, -1e30, 1e30, -1e30, 1e30);
        assert_eq!(
            (outside.xmin, outside.ymin, outside.cell_count()),
            (0, 0, 1)
        );

        let r = TileRect::from_bbox_m(5, -1e9, -1e9, -9e8, -9e8);
        let n: i64 = 1 << 5;
        assert!(r.xmin >= 0 && r.xmax < n);
        assert!(r.ymin >= 0 && r.ymax < n);
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

    #[test]
    fn morton2_interleaves_bits() {
        // Z-order over a 2x2 then 4x4 grid.
        assert_eq!(morton2(0, 0), 0);
        assert_eq!(morton2(1, 0), 1);
        assert_eq!(morton2(0, 1), 2);
        assert_eq!(morton2(1, 1), 3);
        assert_eq!(morton2(2, 0), 4);
        assert_eq!(morton2(3, 3), 15);
    }

    /// The correctness contract the parallel partition rests on: the blocks are
    /// a disjoint partition that covers `self` exactly. Every (x,y) in the rect
    /// is claimed by exactly one block — an overlap (two blocks) or gap (zero)
    /// would mean a duplicate-PK INSERT or a missing tile in a parallel build.
    fn assert_exact_cover(r: &TileRect, blocks: &[TileRect]) {
        let mut counts = vec![0u32; (r.width() * r.height()) as usize];
        for b in blocks {
            assert_eq!(b.z, r.z, "block kept the zoom");
            assert!(
                b.xmin >= r.xmin && b.xmax <= r.xmax && b.ymin >= r.ymin && b.ymax <= r.ymax,
                "block stays within the rect"
            );
            for y in b.ymin..=b.ymax {
                for x in b.xmin..=b.xmax {
                    let idx = ((y - r.ymin) * r.width() + (x - r.xmin)) as usize;
                    counts[idx] += 1;
                }
            }
        }
        assert!(
            counts.iter().all(|&c| c == 1),
            "every cell covered exactly once (no overlap, no gap)"
        );
    }

    #[test]
    fn split_morton_covers_exactly_without_overlap() {
        let r = TileRect {
            z: 12,
            xmin: 3,
            ymin: 7,
            xmax: 22, // width 20
            ymax: 18, // height 12
        };
        for grid in [1u32, 2, 3, 4, 5] {
            let blocks = r.split_morton(grid);
            assert!(blocks.len() <= (grid * grid) as usize);
            assert_exact_cover(&r, &blocks);
        }
    }

    #[test]
    fn split_morton_is_z_order() {
        // A 2x2 block grid over a 4x4 rect: blocks emitted in Morton order
        // (0,0) -> (1,0) -> (0,1) -> (1,1), i.e. their lower corners step
        // x-first then y, the Z curve.
        let r = TileRect {
            z: 4,
            xmin: 0,
            ymin: 0,
            xmax: 3,
            ymax: 3,
        };
        let blocks = r.split_morton(2);
        assert_eq!(blocks.len(), 4);
        let corners: Vec<(i64, i64)> = blocks.iter().map(|b| (b.xmin, b.ymin)).collect();
        assert_eq!(corners, vec![(0, 0), (2, 0), (0, 2), (2, 2)]);
    }

    #[test]
    fn split_morton_degrades_when_grid_exceeds_extent() {
        // width 3, height 2 -> at most 6 unit blocks, still an exact cover.
        let r = TileRect {
            z: 3,
            xmin: 0,
            ymin: 0,
            xmax: 2,
            ymax: 1,
        };
        let blocks = r.split_morton(10);
        assert_eq!(blocks.len(), 6);
        assert_exact_cover(&r, &blocks);
    }

    #[test]
    fn split_morton_one_grid_is_identity() {
        let r = TileRect {
            z: 3,
            xmin: 1,
            ymin: 2,
            xmax: 7,
            ymax: 9,
        };
        assert_eq!(r.split_morton(1), vec![r]);
    }
}
