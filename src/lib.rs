//! geometry_pyramid - vector tile pyramid for PostgreSQL geometry tables.
//!
//! Inspired by Alibaba GanosBase `ganos_geometry_pyramid`. Provides SQL
//! functions to build, inspect, query, update, and drop a multi-resolution
//! pyramid index over a geometry column.

use pgrx::prelude::*;

::pgrx::pg_module_magic!();

mod config;
mod pyramid;
mod tile;

extension_sql_file!("../sql/bootstrap.sql", bootstrap);

/// Returns the version of the geometry_pyramid extension.
#[pg_extern]
fn geometry_pyramid_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn test_version_returns_pkg_version() {
        let v = Spi::get_one::<String>("SELECT geometry_pyramid_version()")
            .expect("SPI failed")
            .expect("null version");
        assert_eq!(v, env!("CARGO_PKG_VERSION"));
    }
}

/// Required by `cargo pgrx test`.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
