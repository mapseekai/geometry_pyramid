//! Prepared source tables for the opt-in full-build fast paths (prepared
//! tile-major and feature-driven enumeration).
//!
//! Two lifetimes, by parallelism:
//! - **`parallel=1`** — a `TEMP ... ON COMMIT DROP` table created in the
//!   coordinator transaction ([`prepare_source_for_build`]). Session-local;
//!   reclaimed automatically at commit.
//! - **`parallel>1`** — a *committed*, worker-visible table created via an
//!   autonomous worker ([`prepare_source_committed`]). Background workers run
//!   in separate backends and can only see committed data, so a temp table
//!   would be invisible to them. It uses a deterministic per-pyramid name and
//!   `DROP TABLE IF EXISTS` at creation (self-healing, exactly like the shadow
//!   table); the caller drops it after the build.

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
    let attrs_inner = format!(
        "{}{}",
        crate::build::prepared_attribute_projections(src.cfg, "src"),
        crate::build::sample_key_projections_for(src.cfg, "src")
    );
    let rules_inner = crate::build::rule_projections_for(src.cfg);

    Spi::run_with_args(
        &create_table_sql(
            &table,
            src.source_table,
            src.geom_column,
            src.fid_column,
            &attrs_inner,
            &rules_inner,
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

/// Build the *committed*, worker-visible prepared table for a parallel build.
///
/// Runs `DROP IF EXISTS` + `CREATE TABLE … AS` (+ optional GiST index) +
/// `ANALYZE` as one autonomous (separately committed) script so background
/// worker backends can see it under MVCC. The `DROP IF EXISTS` makes a
/// leftover table from a crashed prior run self-heal — the per-pyramid name is
/// deterministic, and concurrent same-pyramid builds are already serialized by
/// the per-pyramid advisory transaction lock in [`crate::build::build`].
///
/// Waits up to `timeout_ms` (0 = unbounded) for the transform to finish — this
/// is the serial transform/index prefix the parallel tile build runs on top of,
/// so it borrows the build's own timeout rather than the short DDL default.
pub(crate) fn prepare_source_committed(
    src: PyramidSource<'_>,
    options: PreparedBuildOptions,
    timeout_ms: i64,
) -> Result<PreparedBuildSource, Box<dyn std::error::Error + Send + Sync>> {
    let table = prepared_table_name_for(src.name);
    let attrs_inner = format!(
        "{}{}",
        crate::build::prepared_attribute_projections(src.cfg, "src"),
        crate::build::sample_key_projections_for(src.cfg, "src")
    );
    let rules_inner = crate::build::rule_projections_for(src.cfg);

    // One autonomous script: tile_srs is inlined (a validated i32, injection-safe)
    // because the autonomous executor runs raw SQL without bind parameters.
    let mut script = format!(
        r#"DROP TABLE IF EXISTS "{table}"; {}"#,
        create_table_committed_sql(
            &table,
            src.source_table,
            src.geom_column,
            src.fid_column,
            &attrs_inner,
            &rules_inner,
            src.cfg.tile_srs,
        )
    );
    if options.create_geom_tile_index {
        script.push(';');
        script.push_str(&create_index_sql(&table));
    }
    script.push(';');
    script.push_str(&analyze_sql(&table));

    crate::worker::exec_sql_autonomous_timeout(&script, timeout_ms)?;
    Ok(PreparedBuildSource::new(
        table,
        options.create_geom_tile_index,
    ))
}

/// Drop a committed prepared table once the build no longer needs it, in the
/// **coordinator's own transaction**.
///
/// It must NOT be an autonomous worker: the coordinator has held `ACCESS SHARE`
/// on this table since the prep-gate read it (and keeps it for the inline
/// strips), so an autonomous `DROP`'s `ACCESS EXCLUSIVE` would block on the
/// coordinator while the coordinator blocks waiting for that worker — a deadlock
/// Postgres can't see (the coordinator waits on a status file, not a lock), so
/// it would burn the full worker timeout every build. A same-transaction lock
/// upgrade has no such conflict. The drop commits with the build; if a later
/// failure rolls the build back the table simply self-heals via the next
/// build's `DROP IF EXISTS`.
pub(crate) fn drop_prepared_committed(table: &str) -> Result<(), spi::Error> {
    Spi::run(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
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

/// Deterministic per-pyramid name for the committed prepared table. Stable
/// across runs so a leaked table self-heals via `DROP IF EXISTS`. `_gp_prep_`
/// (9) + a ≤45-char validated pyramid name stays within the 63-char limit.
fn prepared_table_name_for(pyramid_name: &str) -> String {
    format!("_gp_prep_{pyramid_name}")
}

/// Committed (non-temp) variant of [`create_table_sql`]. Same projection, but
/// the target survives the creating transaction so other backends can read it,
/// and `tile_srs` is inlined because the autonomous executor takes no binds.
fn create_table_committed_sql(
    prepared_table: &str,
    source_table: &str,
    geom_column: &str,
    fid_column: &str,
    attrs_inner: &str,
    rules_inner: &str,
    tile_srs: i32,
) -> String {
    format!(
        r#"CREATE TABLE "{prepared_table}" AS
           SELECT
              src."{fid_column}"::bigint AS fid,
              ST_Transform(src."{geom_column}", {tile_srs}) AS geom_tile,
              GeometryType(src."{geom_column}") AS geom_type
              {attrs_inner}{rules_inner}
           FROM "{source_table}" src
           WHERE src."{geom_column}" IS NOT NULL"#
    )
}

fn create_table_sql(
    prepared_table: &str,
    source_table: &str,
    geom_column: &str,
    fid_column: &str,
    attrs_inner: &str,
    rules_inner: &str,
) -> String {
    format!(
        r#"CREATE TEMP TABLE "{prepared_table}" ON COMMIT DROP AS
           SELECT
              src."{fid_column}"::bigint AS fid,
              ST_Transform(src."{geom_column}", $1) AS geom_tile,
              GeometryType(src."{geom_column}") AS geom_type
              {attrs_inner}{rules_inner}
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
            "",
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
    fn committed_table_name_is_deterministic_per_pyramid() {
        // Stable across runs (no pid/idx) so a leaked table self-heals.
        assert_eq!(prepared_table_name_for("roads"), "_gp_prep_roads");
        // Longest validated pyramid name (45) stays within the 63-char limit.
        assert!(prepared_table_name_for(&"a".repeat(45)).len() <= 63);
    }

    #[test]
    fn committed_create_sql_is_persistent_and_inlines_srs() {
        let sql = create_table_committed_sql(
            "_gp_prep_roads",
            "roads",
            "geom",
            "gid",
            r#", src."kind" AS "kind""#,
            "",
            3857,
        );
        // Persistent: no TEMP / ON COMMIT DROP so workers can see it.
        assert!(sql.contains(r#"CREATE TABLE "_gp_prep_roads" AS"#));
        assert!(!sql.contains("TEMP"));
        assert!(!sql.contains("ON COMMIT DROP"));
        // tile_srs inlined (autonomous executor takes no bind params).
        assert!(sql.contains(r#"ST_Transform(src."geom", 3857) AS geom_tile"#));
        assert!(sql.contains(r#", src."kind" AS "kind""#));
    }

    #[test]
    fn create_table_sql_projects_rule_columns() {
        let sql = create_table_sql(
            "_gp_prep_7",
            "roads",
            "geom",
            "gid",
            "",
            r#", (scalerank < 5) AS _gp_rule_0"#,
        );
        assert!(sql.contains(r#"(scalerank < 5) AS _gp_rule_0"#));
    }

    #[test]
    fn create_table_sql_projects_sample_key_columns() {
        let sql = create_table_sql(
            "_gp_prep_7",
            "roads",
            "geom",
            "gid",
            r#", src."kind"::text AS _gp_sample_0_1"#,
            "",
        );
        assert!(sql.contains(r#"src."kind"::text AS _gp_sample_0_1"#));
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
