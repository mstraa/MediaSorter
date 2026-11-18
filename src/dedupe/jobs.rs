//! Background scan worker: one at a time, on a dedicated OS thread (the walk
//! is blocking I/O and must never occupy a Tokio worker), with pause, resume
//! and cancel. Cancel keeps the checkpoint: the next start resumes.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use super::scan::{run_scan, LiveProgress, ScanCounters, ScanOutcome};
use super::settings::DedupeSettings;
use super::throttle::RunControl;

#[derive(Default)]
struct Slot {
    control: Option<Arc<RunControl>>,
    trigger: String,
    last_outcome: Option<String>,
    last_error: Option<String>,
}

#[derive(Clone)]
pub struct ScanJobs {
    db_path: PathBuf,
    slot: Arc<Mutex<Slot>>,
    live: Arc<Mutex<LiveProgress>>,
}

/// Live view of the running scan for the status endpoint.
#[derive(Clone, Debug, Serialize)]
pub struct LiveStatus {
    pub state: &'static str,
    pub trigger: String,
    pub run_id: i64,
    pub generation: i64,
    pub resumed: bool,
    pub current_dir: String,
    pub pending_dirs: i64,
    pub counters: ScanCounters,
    pub elapsed_secs: f64,
    pub dirs_per_sec: f64,
    pub entries_per_sec: f64,
    pub io_priority_idle: bool,
    pub last_error: Option<String>,
    /// walk | partial | full | groups
    pub stage: String,
    pub hash_files_total: i64,
    pub hash_files_done: i64,
    pub hash_bytes_total: i64,
    pub hash_bytes_done: i64,
    pub hash_errors: i64,
    pub bytes_per_sec: f64,
}

impl ScanJobs {
    pub fn new(db_path: PathBuf) -> Self {
        Self {
            db_path,
            slot: Arc::new(Mutex::new(Slot::default())),
            live: Arc::new(Mutex::new(LiveProgress::default())),
        }
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_running(&self) -> bool {
        self.slot().control.is_some()
    }

    pub fn running_trigger(&self) -> Option<String> {
        let slot = self.slot();
        slot.control.as_ref().map(|_| slot.trigger.clone())
    }

    /// Start (or resume) a scan. `fresh` discards an unfinished run instead of
    /// resuming it. Errors when a scan is already running.
    pub fn start(
        &self,
        settings: DedupeSettings,
        trigger: &str,
        fresh: bool,
    ) -> Result<(), String> {
        let control = Arc::new(RunControl::new());
        {
            let mut slot = self.slot();
            if slot.control.is_some() {
                return Err("A scan is already running".into());
            }
            slot.control = Some(control.clone());
            slot.trigger = trigger.to_string();
            slot.last_error = None;
            slot.last_outcome = None;
        }
        *self.live.lock().unwrap_or_else(|e| e.into_inner()) = LiveProgress::default();
        let jobs = self.clone();
        let trigger = trigger.to_string();
        let spawned = std::thread::Builder::new()
            .name("dedupe-scan".into())
            .spawn(move || {
                let result = run_scan(
                    &jobs.db_path,
                    &settings,
                    &trigger,
                    fresh,
                    &control,
                    &jobs.live,
                );
                let mut slot = jobs.slot();
                match result {
                    Ok(ScanOutcome::Completed { .. }) => {
                        slot.last_outcome = Some("completed".into());
                    }
                    Ok(ScanOutcome::Stopped { .. }) => {
                        slot.last_outcome = Some("stopped".into());
                    }
                    Err(e) => {
                        tracing::warn!("dedupe scan failed: {e}");
                        slot.last_outcome = Some("failed".into());
                        slot.last_error = Some(e);
                    }
                }
                slot.control = None;
            });
        if let Err(e) = spawned {
            self.slot().control = None;
            return Err(format!("Cannot start the scan thread: {e}"));
        }
        Ok(())
    }

    fn with_control(&self, f: impl FnOnce(&RunControl)) -> bool {
        match self.slot().control.as_ref() {
            Some(control) => {
                f(control);
                true
            }
            None => false,
        }
    }

    pub fn pause(&self) -> bool {
        self.with_control(|c| c.pause())
    }

    pub fn resume(&self) -> bool {
        self.with_control(|c| c.resume())
    }

    /// Stop the walk after the current directory; progress is kept.
    pub fn cancel(&self) -> bool {
        self.with_control(|c| c.stop())
    }

    pub fn last_outcome(&self) -> (Option<String>, Option<String>) {
        let slot = self.slot();
        (slot.last_outcome.clone(), slot.last_error.clone())
    }

    pub fn live_status(&self) -> Option<LiveStatus> {
        let (control, trigger) = {
            let slot = self.slot();
            (slot.control.clone()?, slot.trigger.clone())
        };
        let live = self.live.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let session_secs = live
            .session_started
            .map(|s| s.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        let state = if control.stop_requested() {
            "stopping"
        } else if live.paused {
            "paused"
        } else if control.pause_requested() {
            "pausing"
        } else {
            "running"
        };
        let (dirs_per_sec, entries_per_sec) = if live.paused || session_secs < 1.0 {
            (0.0, 0.0)
        } else {
            (
                (live.totals.dirs_done - live.session_base.dirs_done) as f64 / session_secs,
                (live.totals.entries_seen - live.session_base.entries_seen) as f64 / session_secs,
            )
        };
        let hash_secs = live
            .hash_session_started
            .map(|s| s.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        let bytes_per_sec = if live.paused || hash_secs < 1.0 {
            0.0
        } else {
            (live.hash_bytes_done - live.hash_session_base) as f64 / hash_secs
        };
        let elapsed_secs = live.elapsed_before + if live.paused { 0.0 } else { session_secs };
        Some(LiveStatus {
            state,
            trigger,
            run_id: live.run_id,
            generation: live.generation,
            resumed: live.resumed,
            current_dir: live.current_dir,
            pending_dirs: live.pending_dirs,
            counters: live.totals,
            elapsed_secs,
            dirs_per_sec,
            entries_per_sec,
            io_priority_idle: live.io_priority_idle,
            last_error: live.last_error,
            stage: if live.stage.is_empty() {
                "walk".into()
            } else {
                live.stage
            },
            hash_files_total: live.hash_files_total,
            hash_files_done: live.hash_files_done,
            hash_bytes_total: live.hash_bytes_total,
            hash_bytes_done: live.hash_bytes_done,
            hash_errors: live.hash_errors,
            bytes_per_sec,
        })
    }
}
