//! Background-worker control plane. Specs and completion status travel
//! through files under $PGDATA/gp_tmp/ because the coordinator's
//! transaction is uncommitted — its catalog rows and job rows would be
//! invisible to workers under MVCC; files are not.

use crate::config::PyramidConfig;
use crate::tile_rect::TileRect;
use pgrx::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
pub(crate) struct WorkerSpec {
    pub db_oid: u32,
    pub mode: WorkerMode,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum WorkerMode {
    /// Execute one SQL script in an autonomous (separately committed)
    /// transaction. Used to create the shadow table for parallel builds.
    ExecSql { sql: String },
    /// Build tile strips into `storage_table` using the shared
    /// build_zoom_level pipeline, one transaction per strip.
    BuildStrips {
        pyramid_name: String,
        storage_table: String,
        source_table: String,
        geom_column: String,
        fid_column: String,
        config: PyramidConfig,
        knobs: KnobValues,
        strips: Vec<TileRect>,
    },
}

/// Serializable snapshot of build.rs's ZoomKnobs (the budget-retry loop
/// tightens these per attempt, so they ride along with each round's spec).
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub(crate) struct KnobValues {
    pub simplify_tol_m: f64,
    pub min_area_m2: f64,
    pub min_len_m: f64,
    pub drop_threshold: f64,
}

pub(crate) fn spec_filename(backend_pid: u32, idx: u32) -> String {
    format!("gp_{backend_pid}_{idx}.json")
}

/// $PGDATA/gp_tmp — created on demand. Must only be called inside a backend
/// or background-worker process, where `pg_sys::DataDir` is initialized; the
/// assertion guards against misuse from early `_PG_init`.
pub(crate) fn spec_dir() -> PathBuf {
    let datadir = unsafe {
        let ptr = pg_sys::DataDir;
        assert!(
            !ptr.is_null(),
            "DataDir is NULL — spec_dir called outside backend context"
        );
        std::ffi::CStr::from_ptr(ptr as *const _)
            .to_string_lossy()
            .into_owned()
    };
    PathBuf::from(datadir).join("gp_tmp")
}

/// Write a spec file, clearing any stale `.done`/`.err` status. The caller
/// MUST write the spec BEFORE starting the worker that reads it: the worker
/// opens the file by name, so an overwrite after it has opened would not be
/// observed.
pub(crate) fn write_spec(
    filename: &str,
    spec: &WorkerSpec,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let dir = spec_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(filename);
    let _ = std::fs::remove_file(path.with_extension("done"));
    let _ = std::fs::remove_file(path.with_extension("err"));
    std::fs::write(&path, serde_json::to_vec(spec)?)?;
    Ok(path)
}

pub(crate) fn cleanup_spec(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("done"));
    let _ = std::fs::remove_file(path.with_extension("err"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PyramidConfig;
    use crate::tile_rect::TileRect;

    #[test]
    fn spec_roundtrips_through_json() {
        let spec = WorkerSpec {
            db_oid: 16384,
            mode: WorkerMode::BuildStrips {
                pyramid_name: "p".into(),
                storage_table: "_gp_tiles_p__shadow".into(),
                source_table: "src".into(),
                geom_column: "geom".into(),
                fid_column: "gid".into(),
                config: PyramidConfig::default(),
                knobs: KnobValues {
                    simplify_tol_m: 1.0,
                    min_area_m2: 2.0,
                    min_len_m: 3.0,
                    drop_threshold: 0.4,
                },
                strips: vec![TileRect { z: 3, xmin: 0, ymin: 0, xmax: 1, ymax: 7 }],
            },
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: WorkerSpec = serde_json::from_str(&json).unwrap();
        let WorkerMode::BuildStrips { strips, knobs, .. } = back.mode else {
            panic!("wrong mode");
        };
        assert_eq!(back.db_oid, 16384);
        assert_eq!(strips.len(), 1);
        assert_eq!(strips[0].z, 3);
        assert!((knobs.drop_threshold - 0.4).abs() < 1e-9);
    }

    #[test]
    fn spec_filename_fits_bgw_extra() {
        // bgw_extra is 128 bytes including NUL.
        let name = spec_filename(u32::MAX, u32::MAX);
        assert!(name.len() < 127);
    }
}

// ---------------------------------------------------------------------------
// Background-worker runtime (entry point + helpers)
// ---------------------------------------------------------------------------

use pgrx::bgworkers::{
    BackgroundWorker, BackgroundWorkerBuilder, DynamicBackgroundWorker, SignalWakeFlags,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// Per-backend counter so successive workers never reuse a spec filename
/// within the same backend (which would clobber each other's status files,
/// or let a slow-to-start worker read a spec already overwritten by a later
/// round). Combined with the backend PID this makes every spec path unique.
pub(crate) static NEXT_SPEC_IDX: AtomicU32 = AtomicU32::new(0);

/// Allocate a process-unique spec index. Use for every spawned worker.
pub(crate) fn next_spec_idx() -> u32 {
    NEXT_SPEC_IDX.fetch_add(1, Ordering::Relaxed)
}

/// Entry point for dynamic background workers. bgw_extra carries the spec
/// FILENAME; the worker derives the directory from its own DataDir.
#[pg_guard]
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gp_worker_main(_arg: pg_sys::Datum) {
    let filename = BackgroundWorker::get_extra().to_string();
    let path = spec_dir().join(&filename);

    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);

    let spec: WorkerSpec = match std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::write(path.with_extension("err"), format!("bad spec: {e}"));
            return;
        }
    };

    BackgroundWorker::connect_worker_to_spi_by_oid(
        Some(pg_sys::Oid::from(spec.db_oid)),
        None,
    );

    match run_spec(&spec) {
        Ok(()) => {
            let _ = std::fs::write(path.with_extension("done"), b"ok");
        }
        Err(e) => {
            let _ = std::fs::write(path.with_extension("err"), e.to_string());
        }
    }
}

fn run_spec(spec: &WorkerSpec) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match &spec.mode {
        WorkerMode::ExecSql { sql } => {
            let result: Result<(), pgrx::spi::Error> =
                BackgroundWorker::transaction(|| pgrx::Spi::run(sql));
            result?;
            Ok(())
        }
        WorkerMode::BuildStrips {
            pyramid_name,
            storage_table,
            source_table,
            geom_column,
            fid_column,
            config,
            knobs,
            strips,
        } => {
            let k = crate::build::ZoomKnobs::from_values(*knobs);
            for rect in strips {
                let result: Result<i64, pgrx::spi::Error> =
                    BackgroundWorker::transaction(|| {
                        crate::build::build_zoom_level(
                            pyramid_name,
                            storage_table,
                            source_table,
                            geom_column,
                            fid_column,
                            config,
                            rect,
                            &k,
                        )
                    });
                result?;
            }
            Ok(())
        }
    }
}

/// Spawn one dynamic worker for an already-written spec file. Returns the
/// handle, or Err when no worker slot is available (caller degrades to
/// inline execution).
pub(crate) fn spawn(
    filename: &str,
) -> Result<DynamicBackgroundWorker, Box<dyn std::error::Error + Send + Sync>> {
    BackgroundWorkerBuilder::new("geometry_pyramid worker")
        .set_function("gp_worker_main")
        .set_library("geometry_pyramid")
        .set_extra(filename)
        .set_notify_pid(unsafe { pg_sys::MyProcPid })
        .enable_spi_access()
        .load_dynamic()
        .map_err(|e| format!("failed to register background worker: {e:?}").into())
}

/// Run an SQL script in an autonomous transaction via a one-shot worker,
/// waiting for completion. Used for committed shadow-table DDL.
pub(crate) fn exec_sql_autonomous(
    sql: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let filename = spec_filename(unsafe { pg_sys::MyProcPid } as u32, next_spec_idx());
    let spec = WorkerSpec {
        db_oid: unsafe { pg_sys::MyDatabaseId }.to_u32(),
        mode: WorkerMode::ExecSql { sql: sql.to_string() },
    };
    let path = write_spec(&filename, &spec)?;
    let _handle = spawn(&filename)?;
    let result = wait_for(std::slice::from_ref(&path), 60_000);
    cleanup_spec(&path);
    result
}

/// Poll status files until every spec is done, any errors out, or the
/// timeout (ms, 0 = unbounded) elapses.
///
/// A worker that dies without writing a status file (e.g. an OS kill, or a
/// PG `FATAL`/`PANIC` that longjmps past the `.err` write) is only detected
/// here by the timeout — hence callers should pass a non-zero `timeout_ms`
/// for unattended builds. The common failure (a PG `ERROR` inside the
/// worker's transaction) is caught by pgrx and surfaced as a written `.err`.
pub(crate) fn wait_for(
    paths: &[PathBuf],
    timeout_ms: i64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let started = std::time::Instant::now();
    loop {
        let mut pending = 0usize;
        for p in paths {
            let err_path = p.with_extension("err");
            if err_path.exists() {
                let msg = std::fs::read_to_string(&err_path).unwrap_or_default();
                return Err(format!("worker failed: {msg}").into());
            }
            if !p.with_extension("done").exists() {
                pending += 1;
            }
        }
        if pending == 0 {
            return Ok(());
        }
        if timeout_ms > 0 && started.elapsed().as_millis() as i64 > timeout_ms {
            return Err(format!("timed out waiting for {pending} worker(s)").into());
        }
        pgrx::check_for_interrupts!();
        std::thread::sleep(Duration::from_millis(50));
    }
}
