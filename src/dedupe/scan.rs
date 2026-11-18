//! Phase 1: the metadata walk.
//!
//! Single-threaded on purpose (parallel walks make a raidz of HDDs slower, not
//! faster), throttled by a stat budget, never follows symlinks, never crosses
//! into another filesystem, and records each regular file's identity in
//! `dedupe.db`.
//!
//! **Resumable.** The frontier of directories still to walk lives in
//! `scan_queue`. Work is committed in batches (~1,000 rows or 2 s, whichever
//! comes first); a directory is removed from the queue in the same transaction
//! that records its files and enqueues its subdirectories. A crash, restart or
//! cancel therefore replays at most one uncommitted batch, and replaying is
//! harmless because every write is an upsert. Memory stays bounded: nothing
//! but the current directory's entries is held in RAM.
//!
//! **Incremental.** Rows are keyed by path and upserted; hashes (phase 2) are
//! keyed by `(dev, ino, size, mtime)` and survive as long as those do. When a
//! run completes, rows it did not see get `gone_at`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use super::groups::rebuild_groups;
use super::hash::{run_hash, HashCounters, HashOutcome};
use super::settings::{DedupeSettings, ExcludeMatcher};
use super::store::{self, open_connection};
use super::throttle::{set_idle_io_priority, RateLimiter, RunControl};

/// Commit after this many written rows...
const BATCH_ROWS: usize = 1_000;
/// ...or after this long, so a slow (throttled) batch still checkpoints often
/// and never holds the write lock for long.
const BATCH_MAX_AGE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ScanCounters {
    pub dirs_done: i64,
    pub entries_seen: i64,
    pub files_recorded: i64,
    pub bytes_recorded: i64,
    pub skipped_small: i64,
    pub skipped_recent: i64,
    pub skipped_excluded: i64,
    pub skipped_other_fs: i64,
    pub skipped_errors: i64,
}

impl ScanCounters {
    fn add(&mut self, other: &ScanCounters) {
        self.dirs_done += other.dirs_done;
        self.entries_seen += other.entries_seen;
        self.files_recorded += other.files_recorded;
        self.bytes_recorded += other.bytes_recorded;
        self.skipped_small += other.skipped_small;
        self.skipped_recent += other.skipped_recent;
        self.skipped_excluded += other.skipped_excluded;
        self.skipped_other_fs += other.skipped_other_fs;
        self.skipped_errors += other.skipped_errors;
    }

    fn from_run(run: &store::ScanRunRow) -> Self {
        Self {
            dirs_done: run.dirs_done,
            entries_seen: run.entries_seen,
            files_recorded: run.files_recorded,
            bytes_recorded: run.bytes_recorded,
            skipped_small: run.skipped_small,
            skipped_recent: run.skipped_recent,
            skipped_excluded: run.skipped_excluded,
            skipped_other_fs: run.skipped_other_fs,
            skipped_errors: run.skipped_errors,
        }
    }
}

/// In-memory progress of the running walk, read by the status endpoint.
#[derive(Clone, Debug, Default)]
pub struct LiveProgress {
    pub run_id: i64,
    pub generation: i64,
    pub resumed: bool,
    pub current_dir: String,
    pub pending_dirs: i64,
    pub totals: ScanCounters,
    /// Counters when this session (start or resume) began, for rates.
    pub session_base: ScanCounters,
    pub session_started: Option<Instant>,
    /// Walking time from earlier sessions of the same run.
    pub elapsed_before: f64,
    pub paused: bool,
    pub io_priority_idle: bool,
    pub last_error: Option<String>,
    /// "walk", then "partial" / "full" (hashing), then "groups".
    pub stage: String,
    pub hash_files_total: i64,
    pub hash_files_done: i64,
    pub hash_bytes_total: i64,
    pub hash_bytes_done: i64,
    pub hash_errors: i64,
    /// `hash_bytes_done` when the current hashing session started (rates).
    pub hash_session_base: i64,
    pub hash_session_started: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ScanOutcome {
    Completed { run_id: i64 },
    Stopped { run_id: i64 },
}

/// File identity captured by `lstat`.
#[derive(Clone, Copy, Debug)]
struct Ids {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ns: i64,
    ctime_ns: i64,
    nlink: u64,
    uid: u32,
    gid: u32,
    mode: u32,
}

#[cfg(unix)]
fn ids(meta: &fs::Metadata) -> Ids {
    use std::os::unix::fs::MetadataExt;
    Ids {
        dev: meta.dev(),
        ino: meta.ino(),
        size: meta.size(),
        mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
        ctime_ns: meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
        nlink: meta.nlink(),
        uid: meta.uid(),
        gid: meta.gid(),
        mode: meta.mode(),
    }
}

#[cfg(not(unix))]
fn ids(meta: &fs::Metadata) -> Ids {
    Ids {
        dev: 0,
        ino: 0,
        size: meta.len(),
        mtime_ns: 0,
        ctime_ns: 0,
        nlink: 1,
        uid: 0,
        gid: 0,
        mode: 0,
    }
}

/// Drop roots nested inside another root (they would be walked twice).
pub fn effective_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut sorted: Vec<PathBuf> = roots.to_vec();
    sorted.sort();
    sorted.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for root in sorted {
        if !out.iter().any(|kept| root.starts_with(kept)) {
            out.push(root);
        }
    }
    out
}

/// `st_dev` of each root (not following a symlinked root). Errors name the
/// offending root.
pub fn root_devices(roots: &[PathBuf]) -> Result<Vec<u64>, String> {
    roots
        .iter()
        .map(|root| {
            let meta = fs::symlink_metadata(root)
                .map_err(|e| format!("Cannot read dedupe root {}: {e}", root.display()))?;
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "Dedupe root {} is a symlink; use the real path ({})",
                    root.display(),
                    fs::canonicalize(root)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| "unresolvable".into())
                ));
            }
            if !meta.is_dir() {
                return Err(format!("Dedupe root {} is not a directory", root.display()));
            }
            Ok(ids(&meta).dev)
        })
        .collect()
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Why processing a directory ended early.
enum DirAbort {
    Stop,
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for DirAbort {
    fn from(e: rusqlite::Error) -> Self {
        DirAbort::Db(e)
    }
}

struct Walker<'a> {
    conn: &'a Connection,
    run_id: i64,
    generation: i64,
    roots: Vec<PathBuf>,
    root_devs: Vec<u64>,
    matcher: ExcludeMatcher,
    min_size: u64,
    min_age_ns: i64,
    limiter: RateLimiter,
    control: &'a RunControl,
    last_error: Option<String>,
}

impl Walker<'_> {
    /// Walk one directory: record its eligible files and enqueue its
    /// subdirectories. Returns this directory's counters and how many rows
    /// were written. The caller wraps it in a savepoint so an abort leaves no
    /// partial directory behind.
    fn process_dir(
        &mut self,
        root_index: usize,
        dir: &Path,
    ) -> Result<(ScanCounters, usize, i64), DirAbort> {
        let mut c = ScanCounters {
            dirs_done: 1,
            ..Default::default()
        };
        let mut rows = 0usize;
        let mut enqueued = 0i64;
        let root = &self.roots[root_index];
        let root_dev = self.root_devs[root_index];

        if !self.limiter.acquire(1.0, self.control) {
            return Err(DirAbort::Stop);
        }
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                c.skipped_errors += 1;
                self.last_error = Some(format!("{}: {e}", dir.display()));
                return Ok((c, rows, enqueued));
            }
        };

        let now = now_ns();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    c.skipped_errors += 1;
                    self.last_error = Some(format!("{}: {e}", dir.display()));
                    continue;
                }
            };
            c.entries_seen += 1;
            let path = entry.path();
            let Some(path_str) = path.to_str() else {
                // Paths are stored as TEXT and must round-trip exactly for
                // phase 3, so non-UTF-8 names are left alone.
                c.skipped_errors += 1;
                self.last_error = Some(format!(
                    "{}: name is not valid UTF-8, skipped",
                    path.display()
                ));
                continue;
            };
            let relative = path.strip_prefix(root).unwrap_or(&path);
            // `file_type` comes from `d_type` when the filesystem provides it
            // (ZFS does), so symlinks and excluded entries cost no `stat`.
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(e) => {
                    c.skipped_errors += 1;
                    self.last_error = Some(format!("{path_str}: {e}"));
                    continue;
                }
            };
            if file_type.is_symlink() {
                continue;
            }
            let is_dir = file_type.is_dir();
            if !is_dir && !file_type.is_file() {
                continue;
            }
            if self.matcher.is_excluded(relative, is_dir) {
                c.skipped_excluded += 1;
                continue;
            }
            if !self.limiter.acquire(1.0, self.control) {
                return Err(DirAbort::Stop);
            }
            // `symlink_metadata` = lstat: never follows a link swapped in
            // since `readdir`.
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(e) => {
                    c.skipped_errors += 1;
                    self.last_error = Some(format!("{path_str}: {e}"));
                    continue;
                }
            };
            let id = ids(&meta);
            if id.dev != root_dev {
                c.skipped_other_fs += 1;
                continue;
            }
            if meta.is_dir() {
                let inserted = self.conn.execute(
                    "INSERT OR IGNORE INTO scan_queue (run_id, root_index, path) VALUES (?1, ?2, ?3)",
                    params![self.run_id, root_index as i64, path_str],
                )?;
                rows += inserted;
                enqueued += inserted as i64;
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            if id.size < self.min_size {
                c.skipped_small += 1;
                continue;
            }
            if now - id.mtime_ns < self.min_age_ns {
                c.skipped_recent += 1;
                continue;
            }
            self.conn.execute(
                "INSERT INTO files (path, root, dev, ino, size, mtime_ns, ctime_ns, nlink, uid, gid,
                                    mode, seen_gen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(path) DO UPDATE SET
                    root = excluded.root, dev = excluded.dev, ino = excluded.ino,
                    size = excluded.size, mtime_ns = excluded.mtime_ns,
                    ctime_ns = excluded.ctime_ns, nlink = excluded.nlink, uid = excluded.uid,
                    gid = excluded.gid, mode = excluded.mode, seen_gen = excluded.seen_gen,
                    gone_at = NULL, updated_at = CURRENT_TIMESTAMP",
                params![
                    path_str,
                    root.to_string_lossy(),
                    id.dev as i64,
                    id.ino as i64,
                    id.size as i64,
                    id.mtime_ns,
                    id.ctime_ns,
                    id.nlink as i64,
                    id.uid as i64,
                    id.gid as i64,
                    id.mode as i64,
                    self.generation,
                ],
            )?;
            rows += 1;
            c.files_recorded += 1;
            c.bytes_recorded += id.size as i64;
        }
        Ok((c, rows, enqueued))
    }
}

fn save_counters(
    conn: &Connection,
    run_id: i64,
    totals: &ScanCounters,
    elapsed: f64,
    last_error: &Option<String>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE scan_runs SET dirs_done = ?2, entries_seen = ?3, files_recorded = ?4,
            bytes_recorded = ?5, skipped_small = ?6, skipped_recent = ?7, skipped_excluded = ?8,
            skipped_other_fs = ?9, skipped_errors = ?10, elapsed_secs = ?11, last_error = ?12,
            updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1",
        params![
            run_id,
            totals.dirs_done,
            totals.entries_seen,
            totals.files_recorded,
            totals.bytes_recorded,
            totals.skipped_small,
            totals.skipped_recent,
            totals.skipped_excluded,
            totals.skipped_other_fs,
            totals.skipped_errors,
            elapsed,
            last_error,
        ],
    )?;
    Ok(())
}

fn set_live(live: &Mutex<LiveProgress>, update: impl FnOnce(&mut LiveProgress)) {
    let mut guard = live.lock().unwrap_or_else(|e| e.into_inner());
    update(&mut guard);
}

/// Pick the run to work on: resume the unfinished one when its roots still
/// match (and `fresh` is false), otherwise abandon it and start a new one.
fn prepare_run(
    conn: &Connection,
    roots_json: &str,
    roots: &[PathBuf],
    trigger: &str,
    fresh: bool,
) -> rusqlite::Result<(store::ScanRunRow, bool)> {
    if let Some(run) = store::latest_unfinished_run(conn)? {
        let same_roots = serde_json::to_string(&run.roots).unwrap_or_default() == roots_json;
        if !fresh && same_roots && (run.pending_dirs > 0 || run.stage == "hash") {
            conn.execute(
                "UPDATE scan_runs SET state = 'running', updated_at = CURRENT_TIMESTAMP
                 WHERE id = ?1",
                params![run.id],
            )?;
            let run = store::get_run(conn, run.id)?.expect("run just updated");
            return Ok((run, true));
        }
        conn.execute(
            "UPDATE scan_runs SET state = 'abandoned', finished_at = CURRENT_TIMESTAMP,
                last_error = COALESCE(last_error, ?2)
             WHERE id = ?1",
            params![
                run.id,
                if same_roots {
                    "Restarted from scratch"
                } else {
                    "Dedupe roots changed"
                }
            ],
        )?;
        conn.execute("DELETE FROM scan_queue WHERE run_id = ?1", params![run.id])?;
    }
    let generation: i64 = conn.query_row(
        "SELECT COALESCE(MAX(generation), 0) + 1 FROM scan_runs",
        [],
        |row| row.get(0),
    )?;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    conn.execute(
        "INSERT INTO scan_runs (generation, roots, state, trigger) VALUES (?1, ?2, 'running', ?3)",
        params![generation, roots_json, trigger],
    )?;
    let run_id = conn.last_insert_rowid();
    // Seed in reverse so the walk (newest queue entry first) starts with the
    // first root.
    for (index, root) in roots.iter().enumerate().rev() {
        conn.execute(
            "INSERT OR IGNORE INTO scan_queue (run_id, root_index, path) VALUES (?1, ?2, ?3)",
            params![run_id, index as i64, root.to_string_lossy()],
        )?;
    }
    conn.execute_batch("COMMIT")?;
    let run = store::get_run(conn, run_id)?.expect("run just inserted");
    Ok((run, false))
}

/// Run (or resume) a scan until it completes or `control` stops it.
///
/// Blocking; meant for a dedicated thread. Pausing commits the current batch
/// and blocks inside this function until resumed or stopped.
pub fn run_scan(
    db_path: &Path,
    settings: &DedupeSettings,
    trigger: &str,
    fresh: bool,
    control: &RunControl,
    live: &Mutex<LiveProgress>,
) -> Result<ScanOutcome, String> {
    if cfg!(not(unix)) {
        return Err("Duplicate scanning is only supported on Linux/Unix".into());
    }
    let roots = effective_roots(&settings.roots);
    if roots.is_empty() {
        return Err("No dedupe roots configured".into());
    }
    let root_devs = root_devices(&roots)?;
    let matcher = ExcludeMatcher::new(&settings.excludes)?;
    let conn = open_connection(db_path).map_err(|e| format!("Cannot open dedupe.db: {e}"))?;
    let roots_json = serde_json::to_string(
        &roots
            .iter()
            .map(|r| r.to_string_lossy().to_string())
            .collect::<Vec<_>>(),
    )
    .unwrap_or_default();
    let db_err = |e: rusqlite::Error| format!("dedupe.db error: {e}");

    let (run, resumed) = prepare_run(&conn, &roots_json, &roots, trigger, fresh).map_err(db_err)?;
    let io_idle = set_idle_io_priority();
    let mut totals = ScanCounters::from_run(&run);
    let mut pending = run.pending_dirs;
    let mut elapsed_before = run.elapsed_secs;
    let mut session_started = Instant::now();
    set_live(live, |l| {
        *l = LiveProgress {
            run_id: run.id,
            generation: run.generation,
            resumed,
            pending_dirs: pending,
            totals,
            session_base: totals,
            session_started: Some(session_started),
            elapsed_before,
            io_priority_idle: io_idle,
            last_error: run.last_error.clone(),
            stage: "walk".into(),
            ..Default::default()
        }
    });

    let mut walker = Walker {
        conn: &conn,
        run_id: run.id,
        generation: run.generation,
        roots,
        root_devs,
        matcher,
        min_size: settings.min_size_bytes,
        min_age_ns: (settings.min_age_secs as i64).saturating_mul(1_000_000_000),
        limiter: RateLimiter::new(settings.stat_rate as f64),
        control,
        last_error: run.last_error.clone(),
    };

    let elapsed = |started: Instant, before: f64| before + started.elapsed().as_secs_f64();

    conn.execute_batch("BEGIN IMMEDIATE").map_err(db_err)?;
    let mut batch_rows = 0usize;
    let mut batch_started = Instant::now();

    let outcome = loop {
        if control.pause_requested() {
            elapsed_before = elapsed(session_started, elapsed_before);
            save_counters(&conn, run.id, &totals, elapsed_before, &walker.last_error)
                .map_err(db_err)?;
            conn.execute_batch("COMMIT").map_err(db_err)?;
            set_live(live, |l| {
                l.paused = true;
                l.elapsed_before = elapsed_before;
            });
            let keep_going = control.wait_while_paused();
            session_started = Instant::now();
            set_live(live, |l| {
                l.paused = false;
                l.session_base = totals;
                l.session_started = Some(session_started);
                l.elapsed_before = elapsed_before;
            });
            conn.execute_batch("BEGIN IMMEDIATE").map_err(db_err)?;
            batch_rows = 0;
            batch_started = Instant::now();
            if !keep_going {
                break ScanOutcome::Stopped { run_id: run.id };
            }
        }
        if control.stop_requested() {
            break ScanOutcome::Stopped { run_id: run.id };
        }

        let next: Option<(i64, i64, String)> = conn
            .query_row(
                "SELECT id, root_index, path FROM scan_queue WHERE run_id = ?1
                 ORDER BY id DESC LIMIT 1",
                params![run.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(db_err)?;
        let Some((queue_id, root_index, dir)) = next else {
            // Frontier empty: the walk is complete. Anything this generation
            // did not see is gone (or no longer eligible).
            conn.execute(
                "UPDATE files SET gone_at = CURRENT_TIMESTAMP
                 WHERE gone_at IS NULL AND seen_gen < ?1",
                params![run.generation],
            )
            .map_err(db_err)?;
            break ScanOutcome::Completed { run_id: run.id };
        };
        set_live(live, |l| l.current_dir = dir.clone());

        conn.execute_batch("SAVEPOINT dir").map_err(db_err)?;
        match walker.process_dir(root_index as usize, Path::new(&dir)) {
            Ok((counters, rows, enqueued)) => {
                conn.execute("DELETE FROM scan_queue WHERE id = ?1", params![queue_id])
                    .map_err(db_err)?;
                conn.execute_batch("RELEASE dir").map_err(db_err)?;
                totals.add(&counters);
                pending += enqueued - 1;
                batch_rows += rows + 1;
                let last_error = walker.last_error.clone();
                set_live(live, |l| {
                    l.totals = totals;
                    l.pending_dirs = pending;
                    l.last_error = last_error;
                });
            }
            Err(DirAbort::Stop) => {
                // Drop the half-walked directory; it stays queued and is
                // walked again in full on resume.
                conn.execute_batch("ROLLBACK TO dir; RELEASE dir")
                    .map_err(db_err)?;
                break ScanOutcome::Stopped { run_id: run.id };
            }
            Err(DirAbort::Db(e)) => {
                let _ = conn.execute_batch("ROLLBACK TO dir; RELEASE dir; ROLLBACK");
                let _ = conn.execute(
                    "UPDATE scan_runs SET state = 'interrupted', last_error = ?2 WHERE id = ?1",
                    params![run.id, format!("dedupe.db error: {e}")],
                );
                return Err(db_err(e));
            }
        }

        if batch_rows >= BATCH_ROWS || batch_started.elapsed() >= BATCH_MAX_AGE {
            save_counters(
                &conn,
                run.id,
                &totals,
                elapsed(session_started, elapsed_before),
                &walker.last_error,
            )
            .map_err(db_err)?;
            conn.execute_batch("COMMIT; BEGIN IMMEDIATE")
                .map_err(db_err)?;
            batch_rows = 0;
            batch_started = Instant::now();
        }
    };

    let walk_done = matches!(outcome, ScanOutcome::Completed { .. });
    save_counters(
        &conn,
        run.id,
        &totals,
        elapsed(session_started, elapsed_before),
        &walker.last_error,
    )
    .map_err(db_err)?;
    // A finished walk moves the run to its hash stage; it only becomes
    // `completed` once hashing and grouping are done too.
    conn.execute(
        "UPDATE scan_runs SET state = ?2, stage = CASE WHEN ?3 THEN 'hash' ELSE stage END
         WHERE id = ?1",
        params![
            run.id,
            if walk_done { "running" } else { "interrupted" },
            walk_done
        ],
    )
    .map_err(db_err)?;
    conn.execute_batch("COMMIT").map_err(db_err)?;
    if !walk_done {
        return Ok(outcome);
    }

    // ---- Phase 2: hashing, then groups ----
    let mut hash_counters = HashCounters::default();
    let mut last_error = walker.last_error.clone();
    let hash_started = Instant::now();
    let hashed = run_hash(
        &conn,
        settings.read_rate_mbps,
        control,
        live,
        &mut hash_counters,
        &mut last_error,
    );
    conn.execute(
        "UPDATE scan_runs SET hash_files = hash_files + ?2, hash_bytes = hash_bytes + ?3,
            hash_errors = hash_errors + ?4, hash_secs = hash_secs + ?5, last_error = ?6,
            updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1",
        params![
            run.id,
            hash_counters.files,
            hash_counters.bytes,
            hash_counters.errors,
            hash_started.elapsed().as_secs_f64(),
            last_error,
        ],
    )
    .map_err(db_err)?;
    match hashed {
        Ok(HashOutcome::Completed) => {}
        Ok(HashOutcome::Stopped) => {
            conn.execute(
                "UPDATE scan_runs SET state = 'interrupted' WHERE id = ?1",
                params![run.id],
            )
            .map_err(db_err)?;
            return Ok(ScanOutcome::Stopped { run_id: run.id });
        }
        Err(e) => {
            let _ = conn.execute(
                "UPDATE scan_runs SET state = 'interrupted', last_error = ?2 WHERE id = ?1",
                params![run.id, e],
            );
            return Err(e);
        }
    }

    set_live(live, |l| {
        l.stage = "groups".into();
        l.current_dir.clear();
    });
    let roots = effective_roots(&settings.roots);
    rebuild_groups(&conn, &roots).map_err(db_err)?;
    conn.execute(
        "UPDATE scan_runs SET state = 'completed', finished_at = CURRENT_TIMESTAMP
         WHERE id = ?1",
        params![run.id],
    )
    .map_err(db_err)?;
    Ok(outcome)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::Arc;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("ms-dedupe-scan-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn settings_for(root: &Path) -> DedupeSettings {
        DedupeSettings {
            roots: vec![root.to_path_buf()],
            min_size_bytes: 1,
            min_age_secs: 0,
            stat_rate: 0,
            ..Default::default()
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn recorded(db: &Path) -> Vec<(String, i64, bool)> {
        let conn = open_connection(db).unwrap();
        let mut stmt = conn
            .prepare("SELECT path, size, gone_at IS NOT NULL FROM files ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn scan(db: &Path, settings: &DedupeSettings) -> ScanOutcome {
        let control = RunControl::new();
        let live = Mutex::new(LiveProgress::default());
        run_scan(db, settings, "manual", false, &control, &live).unwrap()
    }

    fn build_tree(root: &Path) {
        for d in 0..6 {
            for f in 0..5 {
                write(
                    &root.join(format!("d{d}/sub{}/f{f}.mkv", f % 2)),
                    format!("{d}-{f}").repeat(10).as_bytes(),
                );
            }
        }
        write(&root.join("top.mkv"), b"top-level file");
    }

    #[test]
    fn records_files_and_respects_filters() {
        let tmp = TempDir::new();
        let root = tmp.0.join("data");
        write(&root.join("Torrents/a.mkv"), &[1u8; 100]);
        write(&root.join("Torrents/small.nfo"), &[1u8; 5]);
        write(&root.join("Torrent-work/partial.mkv"), &[1u8; 100]);
        write(&root.join("Torrents/b.mkv.part"), &[1u8; 100]);
        // Symlinks are neither followed nor recorded.
        symlink(root.join("Torrents"), root.join("link-dir")).unwrap();
        symlink(root.join("Torrents/a.mkv"), root.join("link.mkv")).unwrap();
        let db = tmp.0.join("dedupe.db");
        let mut settings = settings_for(&root);
        settings.min_size_bytes = 10;
        assert!(matches!(
            scan(&db, &settings),
            ScanOutcome::Completed { .. }
        ));
        let rows = recorded(&db);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].0.ends_with("Torrents/a.mkv"));

        let conn = open_connection(&db).unwrap();
        let run = store::latest_completed_run(&conn).unwrap().unwrap();
        assert_eq!(run.skipped_small, 1);
        assert_eq!(run.skipped_excluded, 2);
        assert_eq!(run.pending_dirs, 0);
    }

    #[test]
    fn recent_files_are_skipped() {
        let tmp = TempDir::new();
        let root = tmp.0.join("data");
        write(&root.join("new.mkv"), b"fresh");
        let db = tmp.0.join("dedupe.db");
        let mut settings = settings_for(&root);
        settings.min_age_secs = 3600;
        scan(&db, &settings);
        assert!(recorded(&db).is_empty());
    }

    #[test]
    fn hardlinked_paths_share_one_inode_row_identity() {
        let tmp = TempDir::new();
        let root = tmp.0.join("data");
        write(&root.join("Torrents/ep.mkv"), b"episode");
        fs::create_dir_all(root.join("Tv Show")).unwrap();
        fs::hard_link(root.join("Torrents/ep.mkv"), root.join("Tv Show/ep.mkv")).unwrap();
        let db = tmp.0.join("dedupe.db");
        scan(&db, &settings_for(&root));
        let conn = open_connection(&db).unwrap();
        let (paths, inodes, nlink): (i64, i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COUNT(DISTINCT ino), MIN(nlink) FROM files",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((paths, inodes, nlink), (2, 1, 2));
    }

    #[test]
    fn removed_files_are_marked_gone_and_kept_rows_survive() {
        let tmp = TempDir::new();
        let root = tmp.0.join("data");
        write(&root.join("a.mkv"), b"aaaa");
        write(&root.join("b.mkv"), b"bbbb");
        let db = tmp.0.join("dedupe.db");
        scan(&db, &settings_for(&root));
        let conn = open_connection(&db).unwrap();
        let id_before: i64 = conn
            .query_row("SELECT id FROM files WHERE path LIKE '%a.mkv'", [], |r| {
                r.get(0)
            })
            .unwrap();
        fs::remove_file(root.join("b.mkv")).unwrap();
        scan(&db, &settings_for(&root));
        let rows = recorded(&db);
        assert_eq!(rows.len(), 2);
        assert!(!rows[0].2, "a.mkv still present");
        assert!(rows[1].2, "b.mkv marked gone");
        let id_after: i64 = conn
            .query_row("SELECT id FROM files WHERE path LIKE '%a.mkv'", [], |r| {
                r.get(0)
            })
            .unwrap();
        // Same row (upsert): phase-2 data attached to it is not lost.
        assert_eq!(id_before, id_after);
    }

    #[test]
    fn interrupted_scan_resumes_to_the_same_result() {
        let tmp = TempDir::new();
        let root = tmp.0.join("data");
        build_tree(&root);

        // Reference: one uninterrupted scan.
        let reference_db = tmp.0.join("reference.db");
        scan(&reference_db, &settings_for(&root));
        let reference = recorded(&reference_db);
        assert_eq!(reference.len(), 31);

        // Interrupted: stop after a few directories, several times, then
        // finish. Each resume must pick up the queue rather than restart.
        let db = tmp.0.join("dedupe.db");
        let mut settings = settings_for(&root);
        settings.stat_rate = 400; // slow enough to stop mid-walk
        let mut stops = 0;
        loop {
            let control = Arc::new(RunControl::new());
            let live = Arc::new(Mutex::new(LiveProgress::default()));
            let (c2, l2, db2, s2) = (control.clone(), live.clone(), db.clone(), settings.clone());
            let handle = std::thread::spawn(move || run_scan(&db2, &s2, "manual", false, &c2, &l2));
            std::thread::sleep(Duration::from_millis(40));
            control.stop();
            match handle.join().unwrap().unwrap() {
                ScanOutcome::Completed { .. } => break,
                ScanOutcome::Stopped { .. } => stops += 1,
            }
            assert!(stops < 200, "scan never completed");
        }
        assert!(stops >= 2, "test did not interrupt the scan ({stops})");
        assert_eq!(recorded(&db), reference);

        let conn = open_connection(&db).unwrap();
        let runs: i64 = conn
            .query_row("SELECT COUNT(*) FROM scan_runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(runs, 1, "resumes reuse the same run");
        let run = store::latest_completed_run(&conn).unwrap().unwrap();
        // 1 root + 6 dN + 12 subN directories, each counted exactly once.
        assert_eq!(run.dirs_done, 19);
    }

    #[test]
    fn changed_roots_abandon_the_unfinished_run() {
        let tmp = TempDir::new();
        let root = tmp.0.join("data");
        build_tree(&root);
        let db = tmp.0.join("dedupe.db");
        let conn = open_connection(&db).unwrap();
        let (run, resumed) = prepare_run(
            &conn,
            "[\"/elsewhere\"]",
            &[PathBuf::from("/elsewhere")],
            "manual",
            false,
        )
        .unwrap();
        assert!(!resumed);
        conn.execute(
            "UPDATE scan_runs SET state = 'interrupted' WHERE id = ?1",
            params![run.id],
        )
        .unwrap();
        scan(&db, &settings_for(&root));
        let old = store::get_run(&conn, run.id).unwrap().unwrap();
        assert_eq!(old.state, "abandoned");
        assert_eq!(recorded(&db).len(), 31);
    }

    #[test]
    fn pause_and_resume_complete_the_walk() {
        let tmp = TempDir::new();
        let root = tmp.0.join("data");
        build_tree(&root);
        let db = tmp.0.join("dedupe.db");
        let mut settings = settings_for(&root);
        settings.stat_rate = 300;
        let control = Arc::new(RunControl::new());
        let live = Arc::new(Mutex::new(LiveProgress::default()));
        let (c2, l2, db2) = (control.clone(), live.clone(), db.clone());
        let handle =
            std::thread::spawn(move || run_scan(&db2, &settings, "manual", false, &c2, &l2));
        std::thread::sleep(Duration::from_millis(30));
        control.pause();
        std::thread::sleep(Duration::from_millis(400));
        assert!(live.lock().unwrap().paused);
        control.resume();
        let outcome = handle.join().unwrap().unwrap();
        assert!(matches!(outcome, ScanOutcome::Completed { .. }));
        assert_eq!(recorded(&db).len(), 31);
    }

    fn groups(db: &Path) -> Vec<(i64, i64, i64)> {
        let conn = open_connection(db).unwrap();
        let mut stmt = conn
            .prepare("SELECT size, inode_count, reclaimable FROM dup_groups ORDER BY size")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn last_run(db: &Path) -> store::ScanRunRow {
        let conn = open_connection(db).unwrap();
        store::latest_completed_run(&conn).unwrap().unwrap()
    }

    const MIB: usize = 1024 * 1024;

    #[test]
    fn identical_files_form_a_group_and_near_copies_do_not() {
        let tmp = TempDir::new();
        let root = tmp.0.join("data");
        // Large identical pair (partial + full pass).
        let big: Vec<u8> = (0..8 * MIB).map(|i| (i % 251) as u8).collect();
        write(&root.join("Films/a.mkv"), &big);
        write(&root.join("Movies/a.mkv"), &big);
        // Same size, differs only in the last byte: partial hashes differ.
        let mut last = big.clone();
        *last.last_mut().unwrap() ^= 1;
        write(&root.join("Movies/last-byte.mkv"), &last);
        // Same size, differs at 1.5 MiB (outside the partial windows):
        // partial collides, the full hash tells them apart.
        let mut middle = big.clone();
        middle[MIB + MIB / 2] ^= 1;
        write(&root.join("Tv/middle.mkv"), &middle);
        // Small identical pair (hashed whole in the partial pass).
        write(&root.join("Tv/s.mkv"), b"small identical content");
        write(&root.join("Tv Show/s.mkv"), b"small identical content");
        // Already hardlinked: one inode, never a group on its own.
        write(
            &root.join("Torrents/linked.mkv"),
            b"linked once, two names!",
        );
        fs::hard_link(root.join("Torrents/linked.mkv"), root.join("Tv/linked.mkv")).unwrap();

        let db = tmp.0.join("dedupe.db");
        assert!(matches!(
            scan(&db, &settings_for(&root)),
            ScanOutcome::Completed { .. }
        ));
        let big_size = big.len() as i64;
        assert_eq!(
            groups(&db),
            vec![(23, 2, 23), (big_size, 2, big_size)],
            "only the true duplicate pairs"
        );
        let conn = open_connection(&db).unwrap();
        let labels: Vec<String> = conn
            .prepare("SELECT pair_label FROM dup_groups ORDER BY size")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(labels, vec!["Tv ↔ Tv Show", "Films ↔ Movies"]);
        let page = |filter: super::super::groups::GroupsFilter| {
            super::super::groups::list_groups(&conn, &filter, Some((100000, 100000))).unwrap()
        };
        let all = page(super::super::groups::GroupsFilter {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(all.total, 2);
        assert_eq!(all.summary.reclaimable, big_size + 23);
        // Sorted by reclaimable space, keeper first in each group.
        assert_eq!(all.groups[0].size, big_size);
        assert!(all.groups[0].members[0].keep);
        assert_eq!(all.groups[0].members[0].paths.len(), 1);
        let films = page(super::super::groups::GroupsFilter {
            folder: Some(root.join("Films").display().to_string()),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(films.total, 1);
        let pair = page(super::super::groups::GroupsFilter {
            pair: Some("Tv ↔ Tv Show".into()),
            limit: 10,
            ..Default::default()
        });
        assert_eq!((pair.total, pair.total_reclaimable), (1, 23));
        let big_only = page(super::super::groups::GroupsFilter {
            min_size: 1024,
            limit: 10,
            ..Default::default()
        });
        assert_eq!(big_only.total, 1);
        // The full pass only read the inodes whose partial hash collided.
        let full_hashed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM hashes WHERE full IS NOT NULL AND size > ?1",
                params![super::super::hash::SMALL_FILE as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(full_hashed, 3, "a, Movies/a and middle; not last-byte");
    }

    #[test]
    fn unchanged_files_are_not_rehashed() {
        let tmp = TempDir::new();
        let root = tmp.0.join("data");
        let content = vec![5u8; 4 * MIB];
        write(&root.join("A/x.mkv"), &content);
        write(&root.join("B/x.mkv"), &content);
        let db = tmp.0.join("dedupe.db");
        let settings = settings_for(&root);
        scan(&db, &settings);
        let first = last_run(&db);
        assert_eq!(first.hash_files, 4, "2 partial + 2 full");
        scan(&db, &settings);
        let second = last_run(&db);
        assert_ne!(first.id, second.id);
        assert_eq!(second.hash_files, 0);
        assert_eq!(second.hash_bytes, 0);
        assert_eq!(groups(&db).len(), 1);

        // Touching one file (new mtime) re-hashes only that inode.
        let later = filetime::FileTime::from_unix_time(1_500_000_000, 0);
        filetime::set_file_mtime(root.join("B/x.mkv"), later).unwrap();
        scan(&db, &settings);
        assert_eq!(last_run(&db).hash_files, 2, "partial + full of B only");
        assert_eq!(groups(&db).len(), 1);

        // Diverging content removes the group.
        write(&root.join("B/x.mkv"), &vec![6u8; 4 * MIB]);
        scan(&db, &settings);
        assert!(groups(&db).is_empty());
    }

    #[test]
    fn stopped_hashing_resumes_without_rewalking() {
        let tmp = TempDir::new();
        let root = tmp.0.join("data");
        for i in 0..4 {
            let content = vec![i as u8; 4 * MIB];
            write(&root.join(format!("A/{i}.mkv")), &content);
            write(&root.join(format!("B/{i}.mkv")), &content);
        }
        let db = tmp.0.join("dedupe.db");
        let mut settings = settings_for(&root);
        settings.read_rate_mbps = 40.0; // ~32 MiB to read: about a second
        let control = Arc::new(RunControl::new());
        let live = Arc::new(Mutex::new(LiveProgress::default()));
        let (c2, l2, db2, s2) = (control.clone(), live.clone(), db.clone(), settings.clone());
        let handle = std::thread::spawn(move || run_scan(&db2, &s2, "manual", false, &c2, &l2));
        // Wait until hashing is under way, then stop.
        for _ in 0..200 {
            if live.lock().unwrap().stage == "full" {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        control.stop();
        assert!(matches!(
            handle.join().unwrap().unwrap(),
            ScanOutcome::Stopped { .. }
        ));
        let conn = open_connection(&db).unwrap();
        let unfinished = store::latest_unfinished_run(&conn).unwrap().unwrap();
        assert_eq!(unfinished.stage, "hash");
        let dirs_before = unfinished.dirs_done;

        settings.read_rate_mbps = 0.0;
        scan(&db, &settings);
        let done = last_run(&db);
        assert_eq!(done.id, unfinished.id, "same run resumed");
        assert_eq!(done.dirs_done, dirs_before, "the walk was not repeated");
        assert_eq!(groups(&db).len(), 4);
    }

    #[test]
    fn nested_roots_are_collapsed() {
        let roots = vec![
            PathBuf::from("/mnt/data/Torrents"),
            PathBuf::from("/mnt/data"),
            PathBuf::from("/mnt/data2"),
        ];
        assert_eq!(
            effective_roots(&roots),
            vec![PathBuf::from("/mnt/data"), PathBuf::from("/mnt/data2")]
        );
    }
}
