//! "Duplicates" module: find files with identical content on different inodes
//! of the same filesystem and, on explicit request, replace them with hardlinks.
//!
//! Layout:
//! - `settings` — `dedupe_*` settings, exclusion matcher, scan time window.
//! - `store`    — `dedupe.db` (own connection, WAL) schema and read queries.
//! - `throttle` — stat/byte rate limiting, pause/stop control, idle I/O class.
//! - `scan`     — phase 1 metadata walk (resumable, incremental), then runs
//!   the hash stage and rebuilds the groups.
//! - `hash`     — phase 2 partial/full BLAKE3 hashing per inode.
//! - `groups`   — phase 2 duplicate groups, keeper rule, folder-pair labels.
//! - `jobs`     — background scan thread with pause/resume/cancel.
//! - `apply`    — phase 3 simulate/replace jobs, `dedupe_ops` journal and
//!   startup recovery of interrupted replacements.
//!
//! Data safety: scanning and hashing are read-only on the media; only an
//! explicit replace job modifies anything, one verified file at a time.

pub mod apply;
pub mod groups;
pub mod hash;
pub mod jobs;
pub mod scan;
pub mod settings;
pub mod store;
pub mod throttle;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::db::Database;
use apply::{ApplyJob, ApplyJobs, ApplyRequest};
use jobs::{LiveStatus, ScanJobs};
use settings::DedupeSettings;
use store::{DedupeStore, InventorySummary, ScanRunRow};

/// How often the scheduler checks the automatic-scan window.
const SCHEDULER_TICK: Duration = Duration::from_secs(30);
/// An automatic scan does not start a new full walk if the last one finished
/// less than this long ago (it still resumes an unfinished one).
const AUTO_SCAN_MIN_INTERVAL_HOURS: i64 = 20;

#[derive(Clone)]
pub struct DedupeService {
    inner: Arc<Inner>,
}

struct Inner {
    db: Database,
    store: DedupeStore,
    scans: ScanJobs,
    applies: ApplyJobs,
}

/// Per-root health shown in Settings and on the Duplicates page.
#[derive(Clone, Debug, Serialize)]
pub struct RootCheck {
    pub path: String,
    pub exists: bool,
    pub readable: bool,
    pub device: Option<u64>,
    /// Ignored because it sits inside another root.
    pub nested: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RootsReport {
    pub roots: Vec<RootCheck>,
    /// True when every existing root reports the same `st_dev`, which
    /// hardlinks require.
    pub same_device: bool,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScanStatus {
    /// idle | running | pausing | paused | stopping
    pub state: String,
    pub live: Option<LiveStatus>,
    /// Estimated seconds left, when there is a basis for one.
    pub eta_secs: Option<f64>,
    /// Walk: "previous_scan" (directory count of the last full scan) or
    /// "queue" (only the known frontier — a lower bound on a first scan).
    /// Hashing: "hash_bytes" (bytes left in the current pass).
    pub eta_basis: Option<&'static str>,
    pub unfinished_run: Option<ScanRunRow>,
    pub last_completed: Option<ScanRunRow>,
    pub last_outcome: Option<String>,
    pub last_error: Option<String>,
    pub inventory: InventorySummary,
    pub schedule: Option<String>,
    pub in_schedule_window: bool,
}

impl DedupeService {
    pub fn open(db: Database, dedupe_db_path: &Path) -> Result<Self, rusqlite::Error> {
        let store = DedupeStore::open(dedupe_db_path)?;
        let scans = ScanJobs::new(dedupe_db_path.to_path_buf());
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                store,
                scans,
                applies: ApplyJobs::default(),
            }),
        })
    }

    pub fn store(&self) -> &DedupeStore {
        &self.inner.store
    }

    pub fn settings(&self) -> DedupeSettings {
        DedupeSettings::load(&self.inner.db)
    }

    pub fn db(&self) -> &Database {
        &self.inner.db
    }

    /// Process start: a run left `running` was cut short by a crash or a
    /// restart; mark it resumable. Then start the automatic-scan scheduler.
    pub fn startup(&self) {
        // Crash recovery first: remove `.ms-dedupe-*.tmp` links an
        // interrupted replacement left behind (extra links, never data).
        match apply::recover_pending_links(&self.inner.store.lock(), &apply::RealFs) {
            Ok(n) if n > 0 => tracing::info!("dedupe: removed {n} leftover temporary link(s)"),
            Ok(_) => {}
            Err(e) => tracing::warn!("dedupe: cannot check leftover temporary links: {e}"),
        }
        match self.inner.store.mark_running_interrupted() {
            Ok(n) if n > 0 => tracing::info!("dedupe: {n} interrupted scan run(s) can be resumed"),
            Ok(_) => {}
            Err(e) => tracing::warn!("dedupe: cannot inspect scan runs: {e}"),
        }
        let service = self.clone();
        let _ = std::thread::Builder::new()
            .name("dedupe-scheduler".into())
            .spawn(move || loop {
                std::thread::sleep(SCHEDULER_TICK);
                service.scheduler_tick();
            });
    }

    /// Start or stop automatic scans according to the configured window.
    /// Manual scans are never stopped by the window.
    pub fn scheduler_tick(&self) {
        let settings = self.settings();
        let Some(window) = settings.schedule else {
            return;
        };
        let in_window = window.contains_now();
        let running = self.inner.scans.running_trigger();
        match (in_window, running.as_deref()) {
            (false, Some("schedule")) => {
                tracing::info!("dedupe: scan window closed, stopping (resumable)");
                self.inner.scans.cancel();
            }
            (true, None)
                if !settings.roots.is_empty()
                    && self.inner.applies.running().is_none()
                    && self.auto_scan_due() =>
            {
                tracing::info!("dedupe: scan window open, starting automatic scan");
                if let Err(e) = self.inner.scans.start(settings, "schedule", false) {
                    tracing::warn!("dedupe: automatic scan not started: {e}");
                }
            }
            _ => {}
        }
    }

    fn auto_scan_due(&self) -> bool {
        let conn = self.inner.store.lock();
        if matches!(store::latest_unfinished_run(&conn), Ok(Some(_))) {
            return true;
        }
        conn.query_row(
            "SELECT NOT EXISTS (SELECT 1 FROM scan_runs WHERE state = 'completed'
                 AND finished_at > datetime('now', ?1))",
            [format!("-{AUTO_SCAN_MIN_INTERVAL_HOURS} hours")],
            |row| row.get::<_, bool>(0),
        )
        .unwrap_or(false)
    }

    pub fn start_scan(&self, fresh: bool) -> Result<(), String> {
        let settings = self.settings();
        if settings.roots.is_empty() {
            return Err("Configure at least one dedupe root in Settings first".into());
        }
        if self.inner.applies.running().is_some() {
            return Err("A duplicate job is running; wait for it to finish".into());
        }
        self.inner.scans.start(settings, "manual", fresh)
    }

    /// Start a simulation or a hardlink replacement for the selected groups.
    pub fn start_apply(&self, request: ApplyRequest) -> Result<Arc<ApplyJob>, String> {
        if request.groups.is_empty() {
            return Err("Select at least one group".into());
        }
        if self.inner.scans.is_running() {
            return Err("Stop or wait for the running scan first".into());
        }
        let settings = self.settings();
        if settings.roots.is_empty() {
            return Err("No dedupe roots configured".into());
        }
        self.inner
            .applies
            .start(self.inner.store.path().to_path_buf(), settings, request)
    }

    pub fn apply_job(&self, id: &str) -> Option<Arc<ApplyJob>> {
        self.inner.applies.get(id)
    }

    pub fn latest_apply_job(&self) -> Option<Arc<ApplyJob>> {
        self.inner.applies.latest()
    }

    pub fn pause_scan(&self) -> bool {
        self.inner.scans.pause()
    }

    pub fn resume_scan(&self) -> bool {
        self.inner.scans.resume()
    }

    pub fn cancel_scan(&self) -> bool {
        self.inner.scans.cancel()
    }

    pub fn is_scanning(&self) -> bool {
        self.inner.scans.is_running()
    }

    pub fn scan_status(&self) -> ScanStatus {
        let live = self.inner.scans.live_status();
        let (unfinished_run, last_completed, inventory) = {
            let conn = self.inner.store.lock();
            (
                store::latest_unfinished_run(&conn).ok().flatten(),
                store::latest_completed_run(&conn).ok().flatten(),
                store::inventory_summary(&conn).unwrap_or_default(),
            )
        };
        let (last_outcome, last_error) = self.inner.scans.last_outcome();
        let (eta_secs, eta_basis) = match &live {
            Some(live) if live.stage == "partial" || live.stage == "full" => {
                if live.bytes_per_sec > 0.0 {
                    (
                        Some(
                            (live.hash_bytes_total - live.hash_bytes_done).max(0) as f64
                                / live.bytes_per_sec,
                        ),
                        Some("hash_bytes"),
                    )
                } else {
                    (None, None)
                }
            }
            Some(live) if live.dirs_per_sec > 0.0 => {
                let done = live.counters.dirs_done;
                let previous_total = last_completed
                    .as_ref()
                    .map(|r| r.dirs_done)
                    .filter(|total| *total > done);
                match previous_total {
                    Some(total) => (
                        Some((total - done) as f64 / live.dirs_per_sec),
                        Some("previous_scan"),
                    ),
                    None => (
                        Some(live.pending_dirs as f64 / live.dirs_per_sec),
                        Some("queue"),
                    ),
                }
            }
            _ => (None, None),
        };
        let settings = self.settings();
        ScanStatus {
            state: live
                .as_ref()
                .map(|l| l.state.to_string())
                .unwrap_or_else(|| "idle".into()),
            live,
            eta_secs,
            eta_basis,
            unfinished_run,
            last_completed,
            last_outcome,
            last_error,
            inventory,
            schedule: settings.schedule.map(|w| w.to_string()),
            in_schedule_window: settings.schedule.map(|w| w.contains_now()).unwrap_or(false),
        }
    }

    /// Forget all scan results (keeps the operation journal).
    pub fn clear_scan_cache(&self) -> Result<(), String> {
        if self.is_scanning() {
            return Err("Stop the running scan first".into());
        }
        self.inner
            .store
            .clear_scan_cache()
            .map_err(|e| format!("dedupe.db error: {e}"))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ZfsDataset {
    pub name: String,
    pub mountpoint: String,
    pub used_by_snapshots: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ZfsInfo {
    /// The `zfs` command ran (usually not inside an LXC).
    pub available: bool,
    /// Datasets holding the dedupe roots.
    pub datasets: Vec<ZfsDataset>,
}

/// `usedbysnapshots` of the datasets that hold the roots, when the `zfs`
/// command is available. Freed space only comes back once the snapshots
/// that still reference the old inodes expire.
pub fn zfs_info(roots: &[PathBuf]) -> ZfsInfo {
    let output = std::process::Command::new("zfs")
        .args(["list", "-H", "-p", "-o", "name,mountpoint,usedbysnapshots"])
        .output();
    let Ok(output) = output else {
        return ZfsInfo {
            available: false,
            datasets: Vec::new(),
        };
    };
    if !output.status.success() {
        return ZfsInfo {
            available: false,
            datasets: Vec::new(),
        };
    }
    let all: Vec<ZfsDataset> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            Some(ZfsDataset {
                name: parts.next()?.to_string(),
                mountpoint: parts.next()?.to_string(),
                used_by_snapshots: parts.next()?.trim().parse().ok()?,
            })
        })
        .collect();
    let mut datasets: Vec<ZfsDataset> = Vec::new();
    for root in roots {
        let best = all
            .iter()
            .filter(|d| d.mountpoint.starts_with('/') && root.starts_with(&d.mountpoint))
            .max_by_key(|d| d.mountpoint.len());
        if let Some(best) = best {
            if !datasets.iter().any(|d| d.name == best.name) {
                datasets.push(best.clone());
            }
        }
    }
    ZfsInfo {
        available: true,
        datasets,
    }
}

/// Check every configured root: exists, readable, and on the same `st_dev`
/// (hardlinks cannot cross filesystems — and two separate LXC mount points of
/// the same dataset count as two filesystems).
pub fn check_roots(roots: &[PathBuf]) -> RootsReport {
    let effective = scan::effective_roots(roots);
    let mut checks = Vec::new();
    let mut devices: Vec<u64> = Vec::new();
    for root in roots {
        let nested = !effective.contains(root);
        let mut check = RootCheck {
            path: root.display().to_string(),
            exists: false,
            readable: false,
            device: None,
            nested,
            error: None,
        };
        match scan::root_devices(std::slice::from_ref(root)) {
            Ok(devs) => {
                check.exists = true;
                check.device = devs.first().copied();
                check.readable = std::fs::read_dir(root).is_ok();
                if !check.readable {
                    check.error = Some("Not readable by the MediaSorter service".into());
                }
                if let Some(dev) = check.device {
                    if !nested {
                        devices.push(dev);
                    }
                }
            }
            Err(e) => {
                check.exists = root.exists();
                check.error = Some(e);
            }
        }
        checks.push(check);
    }
    devices.sort_unstable();
    devices.dedup();
    let same_device = devices.len() <= 1;
    let mut warnings = Vec::new();
    if !same_device {
        warnings.push(
            "The dedupe roots are on different filesystems (different st_dev). Hardlinks cannot \
             cross filesystems, so duplicates between them can never be merged. In an LXC, mount \
             the whole dataset once (e.g. `mp0: /ColdGooStore/data,mp=/mnt/data`) instead of \
             one mount point per folder."
                .to_string(),
        );
    }
    for check in checks.iter().filter(|c| c.nested) {
        warnings.push(format!(
            "{} is inside another root and is scanned as part of it.",
            check.path
        ));
    }
    for check in checks.iter().filter(|c| c.error.is_some()) {
        warnings.push(format!(
            "{}: {}",
            check.path,
            check.error.clone().unwrap_or_default()
        ));
    }
    RootsReport {
        roots: checks,
        same_device,
        warnings,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn roots_on_one_filesystem_are_accepted() {
        let dir = std::env::temp_dir().join(format!("ms-dedupe-roots-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        let report = check_roots(&[dir.join("a"), dir.join("b"), dir.join("a/x")]);
        assert!(report.same_device);
        assert!(report.roots[2].nested);
        let missing = check_roots(&[dir.join("missing")]);
        assert!(missing.roots[0].error.is_some());
        assert!(!missing.warnings.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roots_on_different_filesystems_warn() {
        // /proc is always a separate filesystem from the temp dir.
        let tmp = std::env::temp_dir();
        if !Path::new("/proc/self").exists() {
            return;
        }
        let report = check_roots(&[tmp, PathBuf::from("/proc")]);
        assert!(!report.same_device);
        assert!(report.warnings[0].contains("different filesystems"));
    }
}
