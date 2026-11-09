//! Background-worker control plane. Jobs and completion status travel
//! through files under $PGDATA/gp_tmp/ because the coordinator's
//! transaction is uncommitted — its catalog rows and job rows would be
//! invisible to workers under MVCC; files are not.

use crate::build::ZoomKnobs;
use crate::config::PyramidConfig;
use crate::tile_rect::TileRect;
use pgrx::bgworkers::{
    BackgroundWorker, BackgroundWorkerBuilder, BackgroundWorkerStatus, DynamicBackgroundWorker,
    SignalWakeFlags,
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

unsafe extern "C-unwind" {
    // A signal handler must use the raw backend symbol, not pgrx's Rust FFI
    // wrapper (which establishes a panic boundary and is not signal-safe).
    #[link_name = "die"]
    fn backend_die(signal: i32);
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Job {
    pub db_oid: u32,
    pub user_oid: u32,
    #[cfg(not(any(feature = "pg17", feature = "pg18")))]
    pub login_oid: u32,
    pub search_path: String,
    pub row_security: String,
    pub mode: WorkerMode,
}

impl Job {
    pub(crate) fn new(mode: WorkerMode) -> Result<Self, Box<dyn Error + Send + Sync>> {
        Ok(Self {
            // Capture the effective role, including SET ROLE, rather than the login role.
            db_oid: unsafe { pg_sys::MyDatabaseId }.to_u32(),
            user_oid: unsafe { pg_sys::GetUserId() }.to_u32(),
            #[cfg(not(any(feature = "pg17", feature = "pg18")))]
            login_oid: unsafe { pg_sys::GetAuthenticatedUserId() }.to_u32(),
            search_path: Spi::get_one("SELECT current_setting('search_path')")?
                .ok_or("search_path is unavailable")?,
            row_security: Spi::get_one("SELECT current_setting('row_security')")?
                .ok_or("row_security is unavailable")?,
            mode,
        })
    }

    fn apply_settings(&self) -> Result<(), spi::Error> {
        Spi::run_with_args(
            "SELECT set_config('search_path', $1, true), set_config('row_security', $2, true)",
            &[
                self.search_path.as_str().into(),
                self.row_security.as_str().into(),
            ],
        )
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) enum WorkerMode {
    /// Execute one SQL script in an autonomous (separately committed)
    /// transaction. Used to create the shadow table for parallel builds.
    ExecSql {
        sql: String,
    },
    CreateStage {
        table: String,
        ddl: String,
    },
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
        config: Box<PyramidConfig>,
        knobs: ZoomKnobs,
        strips: Vec<TileRect>,
        /// Committed prepared table the workers read (None = baseline path).
        prepared_table: Option<String>,
        /// Drive the feature-driven enumeration pipeline for these strips.
        feature_driven: bool,
    },
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
    for extension in ["done", "err", "status"] {
        let _ = fs::remove_file(path.with_extension(extension));
    }
}

fn publish_status(path: &Path, extension: &str, message: &[u8]) -> std::io::Result<()> {
    // The coordinator polls the final path as soon as it exists.
    let pending = path.with_extension("status");
    fs::write(&pending, message)?;
    fs::rename(pending, path.with_extension(extension))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile_rect::TileRect;

    #[test]
    fn job_roundtrips_through_json() {
        let job = Job {
            db_oid: 16384,
            user_oid: 16385,
            #[cfg(not(any(feature = "pg17", feature = "pg18")))]
            login_oid: 10,
            search_path: "data, public".into(),
            row_security: "on".into(),
            mode: WorkerMode::BuildStrips {
                pyramid_name: "p".into(),
                storage_table: "_gp_shadow_p".into(),
                source_table: "src".into(),
                geom_column: "geom".into(),
                fid_column: "gid".into(),
                config: Box::default(),
                knobs: ZoomKnobs {
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
        assert_eq!(back.user_oid, 16385);
        assert_eq!(back.search_path, "data, public");
        assert_eq!(back.row_security, "on");
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

    // The pgrx SIGTERM handler only sets a shutdown flag for cooperative loops.
    // These workers execute SQL: use the backend handler so CHECK_FOR_INTERRUPTS
    // cancels a long query (including pg_sleep) and rolls its transaction back.
    unsafe {
        #[cfg(not(feature = "pg18"))]
        pg_sys::pqsignal(pg_sys::SIGTERM as i32, Some(backend_die));
        #[cfg(feature = "pg18")]
        pg_sys::pqsignal_be(pg_sys::SIGTERM as i32, Some(backend_die));
    }
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP);

    let job: Job = match fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => {
            let _ = publish_status(&path, "err", format!("bad job: {e}").as_bytes());
            return;
        }
    };

    let result = PgTryBuilder::new(|| {
        if job.user_oid == 0 {
            return Err("worker job has no effective user".into());
        }
        // SET ROLE may select a NOLOGIN role. The coordinator already runs as
        // this role; allow its internal connection without granting extra SQL
        // privileges. PG13-16 have no bypass flag: initialize with the original
        // authenticated role, then narrow the session before executing job SQL.
        #[cfg(any(feature = "pg17", feature = "pg18"))]
        let flags = pg_sys::BGWORKER_BYPASS_ROLELOGINCHECK;
        #[cfg(not(any(feature = "pg17", feature = "pg18")))]
        let flags = 0;
        #[cfg(any(feature = "pg17", feature = "pg18"))]
        let connection_user = job.user_oid;
        #[cfg(not(any(feature = "pg17", feature = "pg18")))]
        let connection_user = job.login_oid;
        if connection_user == 0 {
            return Err("worker job has no connection user".into());
        }
        unsafe {
            pg_sys::BackgroundWorkerInitializeConnectionByOid(
                pg_sys::Oid::from(job.db_oid),
                pg_sys::Oid::from(connection_user),
                flags,
            );
        }
        #[cfg(not(any(feature = "pg17", feature = "pg18")))]
        BackgroundWorker::transaction(|| unsafe {
            let user = pg_sys::Oid::from(job.user_oid);
            pg_sys::SetSessionAuthorization(user, pg_sys::superuser_arg(user));
            pg_sys::SetCurrentRoleId(pg_sys::InvalidOid, false);
        });
        run_job(&job)
    })
    .catch_others(|cause| {
        let report = match cause {
            pg_sys::panic::CaughtError::PostgresError(report)
            | pg_sys::panic::CaughtError::ErrorReport(report)
            | pg_sys::panic::CaughtError::RustPanic {
                ereport: report, ..
            } => report,
        };
        let message = report.message().to_owned();
        // Do not return to SPI in an aborted transaction or accidentally commit
        // work preceding the ERROR. This worker exits after writing its status.
        unsafe { pg_sys::AbortCurrentTransaction() };
        Err(message.into())
    })
    .execute();

    match result {
        Ok(()) => {
            let _ = publish_status(&path, "done", b"ok");
        }
        Err(e) => {
            let _ = publish_status(&path, "err", e.to_string().as_bytes());
        }
    }
}

fn run_job(job: &Job) -> Result<(), Box<dyn Error + Send + Sync>> {
    match &job.mode {
        WorkerMode::CreateStage { table, ddl } => BackgroundWorker::transaction(|| {
            job.apply_settings()?;
            crate::shadow::create(table, ddl)
        }),
        WorkerMode::ExecSql { sql } => {
            let result: Result<(), pgrx::spi::Error> = BackgroundWorker::transaction(|| {
                job.apply_settings()?;
                pgrx::Spi::run(sql)
            });
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
                    job.apply_settings()?;
                    crate::build::build_zoom_level_dispatch(
                        src,
                        storage_table,
                        rect,
                        knobs,
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

/// Own the process and its IPC files together. Early returns and PostgreSQL
/// ERROR unwinding stop the worker before removing files it might still write.
pub(crate) struct WorkerJob {
    path: PathBuf,
    handle: Option<DynamicBackgroundWorker>,
}

impl WorkerJob {
    pub(crate) fn start(filename: &str, job: &Job) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let path = write_job(filename, job)?;
        let mut worker = Self { path, handle: None };
        worker.handle = Some(spawn(filename)?);
        Ok(worker)
    }
}

impl Drop for WorkerJob {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // Cleanup also runs while unwinding a query cancellation. Defer a
            // second interrupt until the child has stopped, avoiding a double
            // panic and preventing it from racing the next build.
            unsafe { pg_sys::InterruptHoldoffCount += 1 };
            let _ = handle.terminate().wait_for_shutdown();
            unsafe { pg_sys::InterruptHoldoffCount -= 1 };
        }
        cleanup_job(&self.path);
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

fn run_autonomous(mode: WorkerMode, timeout_ms: i64) -> Result<(), Box<dyn Error + Send + Sync>> {
    let filename = job_filename(unsafe { pg_sys::MyProcPid } as u32, next_job_idx());
    let job = Job::new(mode)?;
    let worker = WorkerJob::start(&filename, &job)?;
    wait_for(slice::from_ref(&worker), timeout_ms)
}

pub(crate) fn create_stage(
    table: &str,
    ddl: String,
    timeout_ms: i64,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    run_autonomous(
        WorkerMode::CreateStage {
            table: table.into(),
            ddl,
        },
        timeout_ms,
    )?;
    crate::shadow::check(table).map(|_| ())
}

#[cfg(any(test, feature = "pg_test"))]
pub(crate) fn exec_sql_autonomous_timeout(
    sql: &str,
    timeout_ms: i64,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    run_autonomous(WorkerMode::ExecSql { sql: sql.into() }, timeout_ms)
}

#[cfg(any(test, feature = "pg_test"))]
pub(crate) fn exec_sql_autonomous(sql: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
    exec_sql_autonomous_timeout(sql, 60_000)
}

/// Poll status files until every job is done, any errors out, or the
/// timeout (ms, 0 = unbounded) elapses.
///
/// A stopped worker without a status file is an error even with no timeout.
pub(crate) fn wait_for(
    workers: &[WorkerJob],
    timeout_ms: i64,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let started = Instant::now();
    let mut poll = Duration::from_millis(2);
    loop {
        let mut pending = 0usize;
        for worker in workers {
            let p = &worker.path;
            let err_path = p.with_extension("err");
            if err_path.exists() {
                let msg = fs::read_to_string(&err_path)?;
                return Err(format!("worker failed: {msg}").into());
            }
            if p.with_extension("done").exists() {
                continue;
            }
            match worker.handle.as_ref().expect("started worker").pid() {
                Ok(_) | Err(BackgroundWorkerStatus::NotYetStarted) => pending += 1,
                Err(BackgroundWorkerStatus::Stopped) => {
                    // Status may have been written between our first file check
                    // and observing process shutdown. Recheck after shutdown.
                    if let Ok(msg) = fs::read_to_string(&err_path) {
                        return Err(format!("worker failed: {msg}").into());
                    }
                    if !p.with_extension("done").exists() {
                        return Err("worker exited without reporting completion".into());
                    }
                }
                Err(status) => return Err(format!("worker unavailable: {status:?}").into()),
            }
        }
        if pending == 0 {
            return Ok(());
        }
        if timeout_ms > 0 && started.elapsed().as_millis() as i64 > timeout_ms {
            return Err(format!("timed out waiting for {pending} worker(s)").into());
        }
        pgrx::check_for_interrupts!();
        thread::sleep(poll);
        poll = (poll * 2).min(Duration::from_millis(50));
    }
}
