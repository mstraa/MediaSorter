//! `dedupe.db`: scan state, file metadata (and, later, hashes, groups and the
//! operation journal).
//!
//! It is a separate SQLite file from `tvsorter.db` on purpose: a full scan
//! writes hundreds of thousands of rows, and the main database is a single
//! connection behind a mutex that the UI and the importer share. Here the
//! scanner opens its own writer connection, and the HTTP handlers read through
//! a second one; WAL lets those reads proceed while the scanner writes.
//!
//! Never delete this file to "reset" the module once phase 3 exists: the
//! operation journal lives here too. Use [`DedupeStore::clear_scan_cache`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

pub type StoreResult<T> = Result<T, rusqlite::Error>;

const PRAGMAS: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 10000;
PRAGMA foreign_keys = ON;
"#;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS scan_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    generation INTEGER NOT NULL,
    -- JSON array of the root paths this run walks. A resumed run keeps its
    -- roots; changing the settings abandons it.
    roots TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('running', 'interrupted', 'completed', 'abandoned')),
    trigger TEXT NOT NULL DEFAULT 'manual',
    started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    finished_at TEXT,
    dirs_done INTEGER NOT NULL DEFAULT 0,
    entries_seen INTEGER NOT NULL DEFAULT 0,
    files_recorded INTEGER NOT NULL DEFAULT 0,
    bytes_recorded INTEGER NOT NULL DEFAULT 0,
    skipped_small INTEGER NOT NULL DEFAULT 0,
    skipped_recent INTEGER NOT NULL DEFAULT 0,
    skipped_excluded INTEGER NOT NULL DEFAULT 0,
    skipped_other_fs INTEGER NOT NULL DEFAULT 0,
    skipped_errors INTEGER NOT NULL DEFAULT 0,
    -- Wall-clock seconds spent walking, summed over resumes (pauses excluded).
    elapsed_secs REAL NOT NULL DEFAULT 0,
    last_error TEXT
);

-- Directories still to walk for an unfinished run: the persisted frontier.
-- A directory leaves the queue in the same transaction that records its
-- files and enqueues its subdirectories, so a crash replays at most the last
-- uncommitted batch.
CREATE TABLE IF NOT EXISTS scan_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id INTEGER NOT NULL REFERENCES scan_runs(id) ON DELETE CASCADE,
    root_index INTEGER NOT NULL,
    path TEXT NOT NULL,
    UNIQUE (run_id, path)
);

CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL UNIQUE,
    root TEXT NOT NULL,
    dev INTEGER NOT NULL,
    ino INTEGER NOT NULL,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    ctime_ns INTEGER NOT NULL,
    nlink INTEGER NOT NULL,
    uid INTEGER NOT NULL,
    gid INTEGER NOT NULL,
    mode INTEGER NOT NULL,
    -- Generation of the last scan run that saw this path.
    seen_gen INTEGER NOT NULL,
    first_seen_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- Set when a completed full scan no longer found the path.
    gone_at TEXT
);

-- Content hashes per inode (phase 2). Valid only while the inode's size and
-- mtime still match; a mismatch means the file changed and is re-hashed.
-- `partial` = BLAKE3 over size + head/middle/tail (1 MiB each; the whole
-- file when it is small, in which case `full` is set too).
CREATE TABLE IF NOT EXISTS hashes (
    dev INTEGER NOT NULL,
    ino INTEGER NOT NULL,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    partial BLOB,
    full BLOB,
    -- Why hashing failed (unreadable...). Not retried until size/mtime change.
    error TEXT,
    hashed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (dev, ino)
);

-- Duplicate groups (same dev, size and full hash, 2+ inodes), rebuilt at the
-- end of each hash stage.
CREATE TABLE IF NOT EXISTS dup_groups (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    dev INTEGER NOT NULL,
    size INTEGER NOT NULL,
    full_hash BLOB NOT NULL,
    inode_count INTEGER NOT NULL,
    path_count INTEGER NOT NULL DEFAULT 0,
    -- size × (inodes − 1)
    reclaimable INTEGER NOT NULL,
    -- Folders involved, e.g. "Films ↔ Movies".
    pair_label TEXT NOT NULL DEFAULT '',
    built_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS dup_group_members (
    group_id INTEGER NOT NULL REFERENCES dup_groups(id) ON DELETE CASCADE,
    dev INTEGER NOT NULL,
    ino INTEGER NOT NULL,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    nlink INTEGER NOT NULL,
    uid INTEGER NOT NULL,
    gid INTEGER NOT NULL,
    PRIMARY KEY (group_id, ino)
);

CREATE INDEX IF NOT EXISTS idx_files_size ON files(size);
CREATE INDEX IF NOT EXISTS idx_files_inode ON files(dev, ino);
CREATE INDEX IF NOT EXISTS idx_files_seen_gen ON files(seen_gen);
CREATE INDEX IF NOT EXISTS idx_scan_queue_run ON scan_queue(run_id, id);
CREATE INDEX IF NOT EXISTS idx_hashes_partial ON hashes(dev, size, partial);
CREATE INDEX IF NOT EXISTS idx_hashes_full ON hashes(full);
CREATE INDEX IF NOT EXISTS idx_dup_groups_reclaimable ON dup_groups(reclaimable DESC);
"#;

/// Columns added after phase 1 shipped: `(table, column, definition)`.
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    // walk → hash: a run is complete only once its hash stage is.
    ("scan_runs", "stage", "TEXT NOT NULL DEFAULT 'walk'"),
    ("scan_runs", "hash_files", "INTEGER NOT NULL DEFAULT 0"),
    ("scan_runs", "hash_bytes", "INTEGER NOT NULL DEFAULT 0"),
    ("scan_runs", "hash_errors", "INTEGER NOT NULL DEFAULT 0"),
    ("scan_runs", "hash_secs", "REAL NOT NULL DEFAULT 0"),
];

fn add_missing_columns(conn: &Connection) -> StoreResult<()> {
    for (table, column, definition) in ADDED_COLUMNS {
        let exists: bool = conn.query_row(
            &format!("SELECT COUNT(*) > 0 FROM pragma_table_info('{table}') WHERE name = ?1"),
            params![column],
            |row| row.get(0),
        )?;
        if !exists {
            conn.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition}"
            ))?;
        }
    }
    Ok(())
}

pub fn open_connection(path: &Path) -> StoreResult<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(PRAGMAS)?;
    conn.execute_batch(SCHEMA)?;
    add_missing_columns(&conn)?;
    Ok(conn)
}

#[derive(Clone, Debug, Serialize)]
pub struct ScanRunRow {
    pub id: i64,
    pub generation: i64,
    pub roots: Vec<String>,
    pub state: String,
    pub trigger: String,
    pub started_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    pub dirs_done: i64,
    pub entries_seen: i64,
    pub files_recorded: i64,
    pub bytes_recorded: i64,
    pub skipped_small: i64,
    pub skipped_recent: i64,
    pub skipped_excluded: i64,
    pub skipped_other_fs: i64,
    pub skipped_errors: i64,
    pub elapsed_secs: f64,
    pub last_error: Option<String>,
    /// "walk" or "hash".
    pub stage: String,
    pub hash_files: i64,
    pub hash_bytes: i64,
    pub hash_errors: i64,
    pub hash_secs: f64,
    /// Directories still queued (only meaningful for an unfinished run).
    pub pending_dirs: i64,
}

const RUN_COLUMNS: &str = "id, generation, roots, state, trigger, started_at, updated_at, \
    finished_at, dirs_done, entries_seen, files_recorded, bytes_recorded, skipped_small, \
    skipped_recent, skipped_excluded, skipped_other_fs, skipped_errors, elapsed_secs, last_error, \
    (SELECT COUNT(*) FROM scan_queue q WHERE q.run_id = scan_runs.id), stage, hash_files, \
    hash_bytes, hash_errors, hash_secs";

fn map_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScanRunRow> {
    let roots: String = row.get(2)?;
    Ok(ScanRunRow {
        id: row.get(0)?,
        generation: row.get(1)?,
        roots: serde_json::from_str(&roots).unwrap_or_default(),
        state: row.get(3)?,
        trigger: row.get(4)?,
        started_at: row.get(5)?,
        updated_at: row.get(6)?,
        finished_at: row.get(7)?,
        dirs_done: row.get(8)?,
        entries_seen: row.get(9)?,
        files_recorded: row.get(10)?,
        bytes_recorded: row.get(11)?,
        skipped_small: row.get(12)?,
        skipped_recent: row.get(13)?,
        skipped_excluded: row.get(14)?,
        skipped_other_fs: row.get(15)?,
        skipped_errors: row.get(16)?,
        elapsed_secs: row.get(17)?,
        last_error: row.get(18)?,
        pending_dirs: row.get(19)?,
        stage: row.get(20)?,
        hash_files: row.get(21)?,
        hash_bytes: row.get(22)?,
        hash_errors: row.get(23)?,
        hash_secs: row.get(24)?,
    })
}

pub fn latest_unfinished_run(conn: &Connection) -> StoreResult<Option<ScanRunRow>> {
    conn.query_row(
        &format!(
            "SELECT {RUN_COLUMNS} FROM scan_runs WHERE state IN ('running', 'interrupted')
             ORDER BY id DESC LIMIT 1"
        ),
        [],
        map_run,
    )
    .optional()
}

pub fn latest_completed_run(conn: &Connection) -> StoreResult<Option<ScanRunRow>> {
    conn.query_row(
        &format!(
            "SELECT {RUN_COLUMNS} FROM scan_runs WHERE state = 'completed'
             ORDER BY id DESC LIMIT 1"
        ),
        [],
        map_run,
    )
    .optional()
}

pub fn get_run(conn: &Connection, id: i64) -> StoreResult<Option<ScanRunRow>> {
    conn.query_row(
        &format!("SELECT {RUN_COLUMNS} FROM scan_runs WHERE id = ?1"),
        params![id],
        map_run,
    )
    .optional()
}

/// One recorded file, as the API returns it.
#[derive(Clone, Debug, Serialize)]
pub struct FileRow {
    pub path: String,
    pub dev: i64,
    pub ino: i64,
    pub size: i64,
    pub mtime: i64,
    pub nlink: i64,
    pub uid: i64,
    pub gid: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct UnusedTorrents {
    pub files: Vec<FileRow>,
    pub total_count: i64,
    pub total_bytes: i64,
}

/// `[low, high)` bounds matching every path strictly under `dir`: `dir/` up
/// to `dir0` (`'0'` is the byte after `'/'`). Uses the `path` index, unlike
/// `LIKE`.
pub fn path_range(dir: &Path) -> (String, String) {
    let dir = dir.to_string_lossy().trim_end_matches('/').to_string();
    (format!("{dir}/"), format!("{dir}0"))
}

/// Files under the torrent folders that nothing else links to (`nlink == 1`
/// at scan time), biggest first.
pub fn unused_torrents(
    conn: &Connection,
    torrent_roots: &[PathBuf],
    limit: i64,
    offset: i64,
) -> StoreResult<UnusedTorrents> {
    let mut files = Vec::new();
    let mut total_count = 0i64;
    let mut total_bytes = 0i64;
    if torrent_roots.is_empty() {
        return Ok(UnusedTorrents {
            files,
            total_count,
            total_bytes,
        });
    }
    // One OR-ed range per torrent root; a handful at most.
    let clause = torrent_roots
        .iter()
        .enumerate()
        .map(|(i, _)| format!("(path >= ?{} AND path < ?{})", 2 * i + 1, 2 * i + 2))
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut bounds: Vec<String> = Vec::new();
    for root in torrent_roots {
        let (low, high) = path_range(root);
        bounds.push(low);
        bounds.push(high);
    }
    let base = format!("FROM files WHERE gone_at IS NULL AND nlink = 1 AND ({clause})");
    conn.query_row(
        &format!("SELECT COUNT(*), COALESCE(SUM(size), 0) {base}"),
        rusqlite::params_from_iter(bounds.iter()),
        |row| {
            total_count = row.get(0)?;
            total_bytes = row.get(1)?;
            Ok(())
        },
    )?;
    let n = bounds.len();
    let mut stmt = conn.prepare(&format!(
        "SELECT path, dev, ino, size, mtime_ns, nlink, uid, gid {base}
         ORDER BY size DESC, path LIMIT ?{} OFFSET ?{}",
        n + 1,
        n + 2
    ))?;
    let mut values: Vec<rusqlite::types::Value> = bounds
        .into_iter()
        .map(rusqlite::types::Value::Text)
        .collect();
    values.push(limit.into());
    values.push(offset.into());
    let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), |row| {
        Ok(FileRow {
            path: row.get(0)?,
            dev: row.get(1)?,
            ino: row.get(2)?,
            size: row.get(3)?,
            mtime: row.get::<_, i64>(4)? / 1_000_000_000,
            nlink: row.get(5)?,
            uid: row.get(6)?,
            gid: row.get(7)?,
        })
    })?;
    for row in rows {
        files.push(row?);
    }
    Ok(UnusedTorrents {
        files,
        total_count,
        total_bytes,
    })
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct InventorySummary {
    pub files: i64,
    pub bytes: i64,
    pub inodes: i64,
}

pub fn inventory_summary(conn: &Connection) -> StoreResult<InventorySummary> {
    conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(size), 0),
                (SELECT COUNT(*) FROM (SELECT 1 FROM files WHERE gone_at IS NULL GROUP BY dev, ino))
         FROM files WHERE gone_at IS NULL",
        [],
        |row| {
            Ok(InventorySummary {
                files: row.get(0)?,
                bytes: row.get(1)?,
                inodes: row.get(2)?,
            })
        },
    )
}

/// Read-side handle used by the HTTP handlers. Short queries only; the
/// scanner never touches this connection.
#[derive(Clone)]
pub struct DedupeStore {
    path: PathBuf,
    conn: Arc<Mutex<Connection>>,
}

impl DedupeStore {
    pub fn open(path: &Path) -> StoreResult<Self> {
        let conn = open_connection(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mark runs left `running` by a crash or a restart as `interrupted`, so
    /// the next start resumes them from their queue.
    pub fn mark_running_interrupted(&self) -> StoreResult<usize> {
        self.lock().execute(
            "UPDATE scan_runs SET state = 'interrupted', updated_at = CURRENT_TIMESTAMP
             WHERE state = 'running'",
            [],
        )
    }

    /// Forget every scan result, hash, group and unfinished run. Keeps the operation
    /// journal (phase 3), which is why this exists instead of deleting the file.
    pub fn clear_scan_cache(&self) -> StoreResult<()> {
        self.lock().execute_batch(
            "BEGIN;
             DELETE FROM scan_queue;
             DELETE FROM scan_runs;
             DELETE FROM files;
             DELETE FROM hashes;
             DELETE FROM dup_group_members;
             DELETE FROM dup_groups;
             COMMIT;",
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::dedupe::scan::{run_scan, LiveProgress};
    use crate::dedupe::settings::DedupeSettings;
    use crate::dedupe::throttle::RunControl;

    #[test]
    fn unused_torrents_lists_single_link_files_under_torrent_roots() {
        let dir = std::env::temp_dir().join(format!("ms-dedupe-store-{}", uuid::Uuid::new_v4()));
        let root = dir.join("data");
        std::fs::create_dir_all(root.join("Torrents/pack")).unwrap();
        std::fs::create_dir_all(root.join("Tv Show")).unwrap();
        std::fs::write(root.join("Torrents/pack/unused.mkv"), vec![1u8; 300]).unwrap();
        std::fs::write(root.join("Torrents/pack/linked.mkv"), vec![2u8; 200]).unwrap();
        std::fs::hard_link(
            root.join("Torrents/pack/linked.mkv"),
            root.join("Tv Show/linked.mkv"),
        )
        .unwrap();
        // Single link, but outside the torrent folder: not listed.
        std::fs::write(root.join("Tv Show/other.mkv"), vec![3u8; 100]).unwrap();
        // A sibling whose name extends "Torrents" must not match the range.
        std::fs::create_dir_all(root.join("Torrents-old")).unwrap();
        std::fs::write(root.join("Torrents-old/x.mkv"), vec![4u8; 100]).unwrap();

        let db = dir.join("dedupe.db");
        let settings = DedupeSettings {
            roots: vec![root.clone()],
            torrent_roots: vec![root.join("Torrents")],
            min_size_bytes: 1,
            min_age_secs: 0,
            stat_rate: 0,
            ..Default::default()
        };
        run_scan(
            &db,
            &settings,
            "manual",
            false,
            &RunControl::new(),
            &Mutex::new(LiveProgress::default()),
        )
        .unwrap();

        let store = DedupeStore::open(&db).unwrap();
        let conn = store.lock();
        let result = unused_torrents(&conn, &settings.torrent_roots, 10, 0).unwrap();
        assert_eq!(result.total_count, 1);
        assert_eq!(result.total_bytes, 300);
        assert!(result.files[0].path.ends_with("Torrents/pack/unused.mkv"));
        assert_eq!(unused_torrents(&conn, &[], 10, 0).unwrap().total_count, 0);

        let summary = inventory_summary(&conn).unwrap();
        assert_eq!((summary.files, summary.inodes), (5, 4));
        drop(conn);
        store.clear_scan_cache().unwrap();
        assert_eq!(inventory_summary(&store.lock()).unwrap().files, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_run_left_running_by_a_crash_becomes_resumable() {
        let dir = std::env::temp_dir().join(format!("ms-dedupe-crash-{}", uuid::Uuid::new_v4()));
        let store = DedupeStore::open(&dir.join("dedupe.db")).unwrap();
        store
            .lock()
            .execute(
                "INSERT INTO scan_runs (generation, roots, state) VALUES (1, '[]', 'running')",
                [],
            )
            .unwrap();
        assert_eq!(store.mark_running_interrupted().unwrap(), 1);
        let run = latest_unfinished_run(&store.lock()).unwrap().unwrap();
        assert_eq!(run.state, "interrupted");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
