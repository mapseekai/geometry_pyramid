//! Build-local prepared source tables for the opt-in first full-build path.
//!
//! The prepared table is session-local and only used by the coordinator
//! backend (`parallel=1`). Background workers run in separate backends and
//! cannot see this temp table.

use crate::source::PyramidSource;
use pgrx::prelude::*;

#[derive(Debug)]
pub(crate) struct PreparedBuildSource {
    pub table: String,
    pub has_geom_tile_index: bool,
}

impl PreparedBuildSource {
    pub(crate) fn new(table: String, has_geom_tile_index: bool) -> Self {
        Self {
            table,
            has_geom_tile_index,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PreparedBuildOptions {
    pub(crate) create_geom_tile_index: bool,
}

impl PreparedBuildOptions {
    pub(crate) fn with_geom_tile_index() -> Self {
        Self {
            create_geom_tile_index: true,
        }
    }

    pub(crate) fn without_geom_tile_index() -> Self {
        Self {
            create_geom_tile_index: false,
        }
    }
}

pub(crate) fn prepare_source_for_build(
    src: PyramidSource<'_>,
    options: PreparedBuildOptions,
) -> Result<PreparedBuildSource, Box<dyn std::error::Error + Send + Sync>> {
    let table = prepared_table_name(
        unsafe { pg_sys::MyProcPid } as u32,
        crate::worker::next_spec_idx(),
    );
    let (attrs_inner, _) = crate::build::attribute_projections_for(src.cfg, "src", "prep");

    Spi::run_with_args(
        &create_table_sql(
            &table,
            src.source_table,
            src.geom_column,
            src.fid_column,
            &attrs_inner,
        ),
        &[src.cfg.tile_srs.into()],
    )?;
    if options.create_geom_tile_index {
        Spi::run(&create_index_sql(&table))?;
    }
    Spi::run(&analyze_sql(&table))?;

    Ok(PreparedBuildSource::new(
        table,
        options.create_geom_tile_index,
    ))
}

pub(crate) fn ensure_geom_tile_index(prepared: &mut PreparedBuildSource) -> Result<(), spi::Error> {
    if prepared.has_geom_tile_index {
        return Ok(());
    }

    Spi::run(&create_index_sql(&prepared.table))?;
    Spi::run(&analyze_sql(&prepared.table))?;
    prepared.has_geom_tile_index = true;
    Ok(())
}

fn prepared_table_name(pid: u32, idx: u32) -> String {
    format!("_gp_prep_{pid}_{idx}")
}

fn create_table_sql(
    prepared_table: &str,
    source_table: &str,
    geom_column: &str,
    fid_column: &str,
    attrs_inner: &str,
) -> String {
    format!(
        r#"CREATE TEMP TABLE "{prepared_table}" ON COMMIT DROP AS
           SELECT
              src."{fid_column}"::bigint AS fid,
              ST_Transform(src."{geom_column}", $1) AS geom_tile,
              GeometryType(src."{geom_column}") AS geom_type
              {attrs_inner}
           FROM "{source_table}" src
           WHERE src."{geom_column}" IS NOT NULL"#
    )
}

fn create_index_sql(prepared_table: &str) -> String {
    format!(
        r#"CREATE INDEX "{prepared_table}_geom_tile_idx" ON "{prepared_table}" USING GIST (geom_tile)"#
    )
}

fn analyze_sql(prepared_table: &str) -> String {
    format!(r#"ANALYZE "{prepared_table}""#)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_table_sql_projects_geometry_and_attributes() {
        let sql = create_table_sql(
            "_gp_prep_7",
            "roads",
            "geom",
            "gid",
            r#", src."kind" AS "kind""#,
        );

        assert!(sql.contains(r#"CREATE TEMP TABLE "_gp_prep_7" ON COMMIT DROP AS"#));
        assert!(sql.contains(r#"src."gid"::bigint AS fid"#));
        assert!(sql.contains(r#"ST_Transform(src."geom", $1) AS geom_tile"#));
        assert!(sql.contains(r#"GeometryType(src."geom") AS geom_type"#));
        assert!(sql.contains(r#", src."kind" AS "kind""#));
        assert!(sql.contains(r#"FROM "roads" src"#));
        assert!(sql.contains(r#"WHERE src."geom" IS NOT NULL"#));
    }

    #[test]
    fn index_sql_targets_geom_tile() {
        assert_eq!(
            create_index_sql("_gp_prep_7"),
            r#"CREATE INDEX "_gp_prep_7_geom_tile_idx" ON "_gp_prep_7" USING GIST (geom_tile)"#
        );
    }

    #[test]
    fn prepare_options_can_skip_initial_geom_index() {
        let opts = PreparedBuildOptions::without_geom_tile_index();
        assert!(!opts.create_geom_tile_index);

        let opts = PreparedBuildOptions::with_geom_tile_index();
        assert!(opts.create_geom_tile_index);
    }

    #[test]
    fn prepared_source_tracks_geom_index_state() {
        let prepared = PreparedBuildSource::new("_gp_prep_7".into(), false);
        assert_eq!(prepared.table, "_gp_prep_7");
        assert!(!prepared.has_geom_tile_index);

        let prepared = PreparedBuildSource::new("_gp_prep_7".into(), true);
        assert!(prepared.has_geom_tile_index);
    }
}
