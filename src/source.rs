//! A borrowed view of a pyramid's identity plus its parsed config.
//!
//! The build / refresh / slice paths all need the same five values
//! (`name`, `source_table`, `geom_column`, `fid_column`, `cfg`). Bundling them
//! into one `Copy` context lets those functions take a single argument instead
//! of five positional ones — and removes a real footgun: `geom_column` and
//! `fid_column` are both `&str`, so as bare positional args nothing stops a
//! caller from transposing them. Named struct fields make that impossible.

use crate::config::PyramidConfig;

/// Identity + config for one pyramid, borrowed for the duration of an
/// operation. Cheap to copy (five references).
#[derive(Clone, Copy)]
pub(crate) struct PyramidSource<'a> {
    /// Pyramid name; also the stem of the `_gp_tiles_<name>` storage table.
    pub name: &'a str,
    /// SQL-qualified, quoted relation resolved from the registered source OID.
    pub source_table: &'a str,
    /// Geometry column in `source_table`.
    pub geom_column: &'a str,
    /// Stable feature-id column in `source_table`.
    pub fid_column: &'a str,
    /// Parsed pyramid configuration.
    pub cfg: &'a PyramidConfig,
}

impl crate::config::CatalogRow {
    pub(crate) fn for_source<'a>(
        &'a self,
        name: &'a str,
        source_table: &'a str,
    ) -> PyramidSource<'a> {
        PyramidSource {
            name,
            source_table,
            geom_column: &self.geom_column,
            fid_column: &self.fid_column,
            cfg: &self.config,
        }
    }
}
