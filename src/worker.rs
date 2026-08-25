//! Background-worker control plane. Jobs and completion status travel
//! through files under $PGDATA/gp_tmp/ because the coordinator's
//! transaction is uncommitted — its catalog rows and job rows would be
//! invisible to workers under MVCC; files are not.

use crate::config::PyramidConfig;
use crate::tile_rect::TileRect;
use pgrx::bgworkers::{
    BackgroundWorker, BackgroundWorkerBuilder, DynamicBackgroundWorker, SignalWakeFlags,
};
use pgrx::prelude::*;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::ffi::CStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize)]
pub(crate) struct Job {
    pub db_oid: u32,
    pub mode: WorkerMode,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum WorkerMode {
    /// Execute one SQL script in an autonomous (separately committed)
    /// transaction. Used to create the shadow table for parallel builds.
    ExecSql { sql: String },
    /// Build tile strips into `storage_table`, one transaction per strip.
    /// When `prepared_table` is set the worker drives the fast prepared
    /// pipeline (prepared tile-major, or feature-driven when `feature_driven`)
    /// against the committed, worker-visible prepared table; otherwise it runs
    /// the baseline tile-major path straight off the source.
    BuildStrips {
        pyramid_name: String,
        storage_table: String,
        source_table: String,
        geom_column: String,
        fid_column: String,
        config: PyramidConfig,
        knobs: KnobValues,
        strips: Vec<TileRect>,
        /// Committed prepared table the workers read (None = baseline path).
        prepared_table: Option<String>,
        /// Drive the feature-driven enumeration pipeline for these strips.
        feature_driven: bool,
    },
}

/// Serializable snapshot of build.rs's ZoomKnobs (the budget-retry loop
/// tightens these per attempt, so they ride along with each round's job).
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub(crate) struct KnobValues {
    pub simplify_tol_m: f64,
    pub min_area_m2: f64,
    pub min_len_m: f64,
    pub drop_threshold: f64,
}

pub(crate) fn job_filename(backend_pid: u32, idx: u32) -> String {
    format!("gp_{backend_pid}_{idx}.json")
}

/// $PGDATA/gp_tmp — created on demand. Must only be called inside a backend
/// or background-worker process, where `pg_sys::DataDir` is initialized; the
/// assertion guards against misuse from early `_PG_init`.
pub(crate) fn job_dir() -> PathBuf {
    let datadir = unsafe {
        let ptr = pg_sys::DataDir;
        assert!(
            !ptr.is_null(),
            "DataDir is NULL — job_dir called outside backend context"
        );
        CStr::from_ptr(ptr as *const _)
            .to_string_lossy()
            .into_owned()
    };
    PathBuf::from(datadir).join("gp_tmp")
}

/// Write a job file, clearing any stale `.done`/`.err` status. The caller
/// MUST write the job BEFORE starting the worker that reads it: the worker
/// opens the file by name, so an overwrite after it has opened would not be
/// observed.
pub(crate) fn write_job(
    filename: &str,
    job: &Job,
) -> Result<PathBuf, Box<dyn Error + Send + Sync>> {
    let dir = job_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join(filename);
    let _ = fs::remove_file(path.with_extension("done"));
    let _ = fs::remove_file(path.with_extension("err"));
    fs::write(&path, serde_json::to_vec(job)?)?;
    Ok(path)
}

pub(crate) fn cleanup_job(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("done"));
    let _ = fs::remove_file(path.with_extension("err"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PyramidConfig;
    use crate::tile_rect::TileRect;

    #[test]
    fn job_roundtrips_through_json() {
        let job = Job {
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
                strips: vec![TileRect {
                    z: 3,
                    xmin: 0,
                    ymin: 0,
                    xmax: 1,
                    ymax: 7,
                }],
                prepared_table: Some("_gp_prep_p".into()),
                feature_driven: true,
            },
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: Job = serde_json::from_str(&json).unwrap();
        let WorkerMode::BuildStrips {
            strips,
            knobs,
            prepared_table,
            feature_driven,
            ..
        } = back.mode
        else {
            panic!("wrong mode");
        };
        assert_eq!(back.db_oid, 16384);
        assert_eq!(strips.len(), 1);
        assert_eq!(strips[0].z, 3);
        assert!((knobs.drop_threshold - 0.4).abs() < 1e-9);
        assert_eq!(prepared_table.as_deref(), Some("_gp_prep_p"));
        assert!(feature_driven);
    }

    #[test]
    fn job_filename_fits_bgw_extra() {
        // bgw_extra is 128 bytes including NUL.
        let name = job_filename(u32::MAX, u32::MAX);
        assert!(name.len() < 127);
    }
}

// ---------------------------------------------------------------------------
// Background-worker runtime (entry point + helpers)
// ---------------------------------------------------------------------------

/// Per-backend counter so successive workers never reuse a job filename
/// within the same backend (which would clobber each other's status files,
/// or let a slow-to-start worker read a job already overwritten by a later
/// round). Combined with the backend PID this makes every job path unique.
pub(crate) static NEXT_JOB_IDX: AtomicU32 = AtomicU32::new(0);

/// Allocate a process-unique job index. Use for every spawned worker.
pub(crate) fn next_job_idx() -> u32 {
    NEXT_JOB_IDX.fetch_add(1, Ordering::Relaxed)
}

/// Entry point for dynamic background workers. bgw_extra carries the job
/// FILENAME; the worker derives the directory from its own DataDir.
#[pg_guard]
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gp_worker_main(_arg: pg_sys::Datum) {
    let filename = BackgroundWorker::get_extra().to_string();
    let path = job_dir().join(&filename);

    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);

    let job: Job = match fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => {
            let _ = fs::write(path.with_extension("err"), format!("bad job: {e}"));
            return;
        }
    };

    BackgroundWorker::connect_worker_to_spi_by_oid(Some(pg_sys::Oid::from(job.db_oid)), None);

    match run_job(&job) {
        Ok(()) => {
            let _ = fs::write(path.with_extension("done"), b"ok");
        }
        Err(e) => {
            let _ = fs::write(path.with_extension("err"), e.to_string());
        }
    }
}

fn run_job(job: &Job) -> Result<(), Box<dyn Error + Send + Sync>> {
    match &job.mode {
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
            prepared_table,
            feature_driven,
        } => {
            let k = crate::build::ZoomKnobs::from_values(*knobs);
            let src = crate::source::PyramidSource {
                name: pyramid_name,
                source_table,
                geom_column,
                fid_column,
                cfg: config,
            };
            // The committed prepared table already carries its GiST index
            // (workers never run the index-free feature-major-points path —
            // that stays parallel=1), so report the index as present.
            let prepared = prepared_table
                .as_ref()
                .map(|t| crate::prepared::PreparedBuildSource::new(t.clone(), true));
            for rect in strips {
                let result: Result<i64, pgrx::spi::Error> = BackgroundWorker::transaction(|| {
                    crate::build::build_zoom_level_dispatch(
                        src,
                        storage_table,
                        rect,
                        &k,
                        prepared.as_ref(),
                        false, // feature_major_points: never farmed to workers
                        *feature_driven,
                    )
                });
                result?;
            }
            Ok(())
        }
    }
}

/// Spawn one dynamic worker for an already-written job file. Returns the
/// handle, or Err when no worker slot is available (caller degrades to
/// inline execution).
pub(crate) fn spawn(
    filename: &str,
) -> Result<DynamicBackgroundWorker, Box<dyn Error + Send + Sync>> {
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
/// waiting up to `timeout_ms` (0 = unbounded) for completion. Used for
/// committed shadow-table DDL and the committed prepared-table transform.
pub(crate) fn exec_sql_autonomous_timeout(
    sql: &str,
    timeout_ms: i64,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let filename = job_filename(unsafe { pg_sys::MyProcPid } as u32, next_job_idx());
    let job = Job {
        db_oid: unsafe { pg_sys::MyDatabaseId }.to_u32(),
        mode: WorkerMode::ExecSql {
            sql: sql.to_string(),
        },
    };
    let path = write_job(&filename, &job)?;
    let _handle = spawn(&filename)?;
    let result = wait_for(slice::from_ref(&path), timeout_ms);
    cleanup_job(&path);
    result
}

/// Run committed DDL with the default 60s budget (fast schema-only work).
pub(crate) fn exec_sql_autonomous(sql: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
    exec_sql_autonomous_timeout(sql, 60_000)
}

/// Poll status files until every job is done, any errors out, or the
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
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let started = Instant::now();
    loop {
        let mut pending = 0usize;
        for p in paths {
            let err_path = p.with_extension("err");
            if err_path.exists() {
                let msg = fs::read_to_string(&err_path).unwrap_or_default();
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
        thread::sleep(Duration::from_millis(50));
    }
}
