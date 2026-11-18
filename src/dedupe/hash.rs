//! Phase 2: content hashing, one file at a time, in large sequential reads
//! throttled to the configured MB/s.
//!
//! Two passes over the candidates, i.e. inodes that share their `(dev, size)`
//! with at least one other inode (paths already hardlinked together are one
//! inode, so one candidate):
//!
//! 1. **partial**: BLAKE3 over the size plus 1 MiB at the head, middle and
//!    tail. Cheap (3 MiB per file) and rules out nearly every false candidate.
//!    Files up to 3 MiB are read whole, so their partial hash is also full.
//! 2. **full**: BLAKE3 over the whole content, only for inodes whose
//!    `(dev, size, partial)` is still shared.
//!
//! Hashes are stored per `(dev, ino)` with the size and mtime they were taken
//! at, and reused while those match: an unchanged file is never re-read.
//! Before and after reading, the open file is `fstat`ed; if it is not the
//! inode the scan recorded, or changed meanwhile, the result is discarded.
//!
//! The hash is only a finder. Phase 3 still compares bytes before linking.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Mutex;
use std::time::Instant;

use rusqlite::{params, Connection};

use super::scan::LiveProgress;
use super::throttle::{RateLimiter, RunControl};

/// Bytes hashed at each of the head, middle and tail for the partial hash.
pub const PARTIAL_CHUNK: u64 = 1024 * 1024;
/// Files up to this size are hashed whole in the partial pass.
pub const SMALL_FILE: u64 = 3 * PARTIAL_CHUNK;
/// Read size for full hashes: large sequential reads suit HDDs.
const FULL_CHUNK: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HashCounters {
    pub files: i64,
    pub bytes: i64,
    pub errors: i64,
    /// Files skipped because they changed since the walk.
    pub changed: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HashOutcome {
    Completed,
    Stopped,
}

/// A file to hash, as recorded by the walk.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub dev: i64,
    pub ino: i64,
    pub size: i64,
    pub mtime_ns: i64,
    pub path: String,
}

enum Abort {
    Stop,
    /// The file is not the inode/size/mtime the walk recorded any more.
    Changed,
    Io(String),
}

impl From<std::io::Error> for Abort {
    fn from(e: std::io::Error) -> Self {
        Abort::Io(e.to_string())
    }
}

#[cfg(unix)]
fn identity(meta: &std::fs::Metadata) -> (i64, i64, i64, i64) {
    use std::os::unix::fs::MetadataExt;
    (
        meta.dev() as i64,
        meta.ino() as i64,
        meta.size() as i64,
        meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
    )
}

#[cfg(not(unix))]
fn identity(meta: &std::fs::Metadata) -> (i64, i64, i64, i64) {
    (0, 0, meta.len() as i64, 0)
}

fn matches(file: &File, c: &Candidate) -> Result<bool, Abort> {
    let meta = file.metadata()?;
    Ok(meta.is_file() && identity(&meta) == (c.dev, c.ino, c.size, c.mtime_ns))
}

/// Shared reader: throttles, honours pause/stop between chunks, and reports
/// progress.
struct Reader<'a> {
    limiter: &'a mut RateLimiter,
    control: &'a RunControl,
    live: &'a Mutex<LiveProgress>,
    bytes: i64,
}

impl Reader<'_> {
    fn read_into(
        &mut self,
        file: &mut File,
        hasher: &mut blake3::Hasher,
        buf: &mut [u8],
        mut remaining: u64,
    ) -> Result<(), Abort> {
        while remaining > 0 {
            if self.control.pause_requested() {
                set_live(self.live, |l| l.paused = true);
                let keep_going = self.control.wait_while_paused();
                set_live(self.live, |l| {
                    l.paused = false;
                    l.hash_session_base = l.hash_bytes_done;
                    l.hash_session_started = Some(Instant::now());
                });
                if !keep_going {
                    return Err(Abort::Stop);
                }
            }
            let want = (buf.len() as u64).min(remaining) as usize;
            let n = file.read(&mut buf[..want])?;
            if n == 0 {
                // Shorter than recorded: it changed under us.
                return Err(Abort::Changed);
            }
            hasher.update(&buf[..n]);
            remaining -= n as u64;
            self.bytes += n as i64;
            set_live(self.live, |l| l.hash_bytes_done += n as i64);
            if !self.limiter.acquire(n as f64, self.control) {
                return Err(Abort::Stop);
            }
        }
        Ok(())
    }
}

/// Partial hash: size + head + middle + tail. Returns `(partial, full)`;
/// `full` is set when the file was small enough to be read whole.
fn hash_partial(
    c: &Candidate,
    reader: &mut Reader<'_>,
) -> Result<([u8; 32], Option<[u8; 32]>), Abort> {
    let mut file = File::open(&c.path)?;
    if !matches(&file, c)? {
        return Err(Abort::Changed);
    }
    let size = c.size as u64;
    let mut buf = vec![0u8; PARTIAL_CHUNK as usize];
    let result = if size <= SMALL_FILE {
        let mut whole = blake3::Hasher::new();
        reader.read_into(&mut file, &mut whole, &mut buf, size)?;
        let full = *whole.finalize().as_bytes();
        (partial_from_full(size, &full), Some(full))
    } else {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"ms-dedupe-partial-v1");
        hasher.update(&size.to_le_bytes());
        for offset in [0, size / 2 - PARTIAL_CHUNK / 2, size - PARTIAL_CHUNK] {
            file.seek(SeekFrom::Start(offset))?;
            reader.read_into(&mut file, &mut hasher, &mut buf, PARTIAL_CHUNK)?;
        }
        (*hasher.finalize().as_bytes(), None)
    };
    if !matches(&file, c)? {
        return Err(Abort::Changed);
    }
    Ok(result)
}

/// For small files the partial "hash" is derived from the full one, so two
/// identical small files share a partial value as well.
fn partial_from_full(size: u64, full: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"ms-dedupe-small-v1");
    hasher.update(&size.to_le_bytes());
    hasher.update(full);
    *hasher.finalize().as_bytes()
}

fn hash_full(c: &Candidate, reader: &mut Reader<'_>) -> Result<[u8; 32], Abort> {
    let mut file = File::open(&c.path)?;
    if !matches(&file, c)? {
        return Err(Abort::Changed);
    }
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; FULL_CHUNK];
    reader.read_into(&mut file, &mut hasher, &mut buf, c.size as u64)?;
    if !matches(&file, c)? {
        return Err(Abort::Changed);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn set_live(live: &Mutex<LiveProgress>, update: impl FnOnce(&mut LiveProgress)) {
    let mut guard = live.lock().unwrap_or_else(|e| e.into_inner());
    update(&mut guard);
}

/// Current inodes (one row per `(dev, ino)` among files still present).
const CURRENT_INODES: &str = "SELECT dev, ino, size, MAX(mtime_ns) AS mtime_ns, MIN(path) AS path
     FROM files WHERE gone_at IS NULL GROUP BY dev, ino";

/// Inodes that need a partial hash: their `(dev, size)` is shared and they
/// have no hash for their current size/mtime.
pub fn partial_candidates(conn: &Connection) -> rusqlite::Result<Vec<Candidate>> {
    load_candidates(
        conn,
        &format!(
            "WITH inodes AS ({CURRENT_INODES}),
                  multi AS (SELECT dev, size FROM inodes GROUP BY dev, size HAVING COUNT(*) > 1)
             SELECT i.dev, i.ino, i.size, i.mtime_ns, i.path
             FROM inodes i
             JOIN multi m ON m.dev = i.dev AND m.size = i.size
             LEFT JOIN hashes h ON h.dev = i.dev AND h.ino = i.ino
                               AND h.size = i.size AND h.mtime_ns = i.mtime_ns
             WHERE h.dev IS NULL
             ORDER BY i.dev, i.size DESC, i.ino"
        ),
    )
}

/// Inodes that need a full hash: valid partial hash shared by another inode
/// of the same `(dev, size)`, no full hash yet, no recorded error.
pub fn full_candidates(conn: &Connection) -> rusqlite::Result<Vec<Candidate>> {
    load_candidates(
        conn,
        &format!(
            "WITH inodes AS ({CURRENT_INODES}),
                  valid AS (
                    SELECT i.dev, i.ino, i.size, i.mtime_ns, i.path, h.partial, h.full, h.error
                    FROM inodes i
                    JOIN hashes h ON h.dev = i.dev AND h.ino = i.ino
                                 AND h.size = i.size AND h.mtime_ns = i.mtime_ns
                    WHERE h.partial IS NOT NULL),
                  multi AS (SELECT dev, size, partial FROM valid
                            GROUP BY dev, size, partial HAVING COUNT(*) > 1)
             SELECT v.dev, v.ino, v.size, v.mtime_ns, v.path
             FROM valid v
             JOIN multi m ON m.dev = v.dev AND m.size = v.size AND m.partial = v.partial
             WHERE v.full IS NULL AND v.error IS NULL
             ORDER BY v.dev, v.size DESC, v.ino"
        ),
    )
}

/// Candidate lists hold one small row per inode, not per path, and only for
/// inodes whose size is shared: bounded by the number of possible duplicates.
fn load_candidates(conn: &Connection, sql: &str) -> rusqlite::Result<Vec<Candidate>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(Candidate {
            dev: row.get(0)?,
            ino: row.get(1)?,
            size: row.get(2)?,
            mtime_ns: row.get(3)?,
            path: row.get(4)?,
        })
    })?;
    rows.collect()
}

fn store_partial(
    conn: &Connection,
    c: &Candidate,
    partial: &[u8; 32],
    full: Option<&[u8; 32]>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO hashes (dev, ino, size, mtime_ns, partial, full, error, hashed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, CURRENT_TIMESTAMP)",
        params![
            c.dev,
            c.ino,
            c.size,
            c.mtime_ns,
            partial.as_slice(),
            full.map(|f| f.as_slice())
        ],
    )?;
    Ok(())
}

fn store_error(conn: &Connection, c: &Candidate, error: &str) -> rusqlite::Result<()> {
    // Keep a partial hash taken earlier; only flag the inode.
    conn.execute(
        "INSERT INTO hashes (dev, ino, size, mtime_ns, error) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(dev, ino) DO UPDATE SET
            error = excluded.error, hashed_at = CURRENT_TIMESTAMP,
            partial = CASE WHEN hashes.size = excluded.size AND hashes.mtime_ns = excluded.mtime_ns
                           THEN hashes.partial ELSE NULL END,
            full = NULL, size = excluded.size, mtime_ns = excluded.mtime_ns",
        params![c.dev, c.ino, c.size, c.mtime_ns, error],
    )?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    Partial,
    Full,
}

/// What every pass shares.
struct PassEnv<'a> {
    conn: &'a Connection,
    control: &'a RunControl,
    live: &'a Mutex<LiveProgress>,
}

fn run_pass(
    env: &PassEnv<'_>,
    pass: Pass,
    candidates: Vec<Candidate>,
    limiter: &mut RateLimiter,
    counters: &mut HashCounters,
    last_error: &mut Option<String>,
) -> Result<HashOutcome, String> {
    let (conn, control, live) = (env.conn, env.control, env.live);
    let bytes_total: i64 = candidates
        .iter()
        .map(|c| match pass {
            Pass::Full => c.size,
            Pass::Partial => c.size.min(SMALL_FILE as i64),
        })
        .sum();
    set_live(live, |l| {
        l.stage = match pass {
            Pass::Partial => "partial",
            Pass::Full => "full",
        }
        .into();
        l.hash_files_total = candidates.len() as i64;
        l.hash_files_done = 0;
        l.hash_bytes_total = bytes_total;
        l.hash_bytes_done = 0;
        l.hash_session_base = 0;
        l.hash_session_started = Some(Instant::now());
    });
    let db_err = |e: rusqlite::Error| format!("dedupe.db error: {e}");
    for c in candidates {
        if control.stop_requested() {
            return Ok(HashOutcome::Stopped);
        }
        set_live(live, |l| l.current_dir = c.path.clone());
        let mut reader = Reader {
            limiter,
            control,
            live,
            bytes: 0,
        };
        let result = match pass {
            Pass::Partial => hash_partial(&c, &mut reader).map(|(p, f)| (Some(p), f)),
            Pass::Full => hash_full(&c, &mut reader).map(|f| (None, Some(f))),
        };
        counters.bytes += reader.bytes;
        match result {
            Ok((partial, full)) => {
                match (partial, full) {
                    (Some(partial), full) => {
                        store_partial(conn, &c, &partial, full.as_ref()).map_err(db_err)?
                    }
                    (None, Some(full)) => {
                        conn.execute(
                            "UPDATE hashes SET full = ?3, hashed_at = CURRENT_TIMESTAMP
                             WHERE dev = ?1 AND ino = ?2",
                            params![c.dev, c.ino, full.as_slice()],
                        )
                        .map_err(db_err)?;
                    }
                    (None, None) => {}
                }
                counters.files += 1;
            }
            Err(Abort::Stop) => return Ok(HashOutcome::Stopped),
            Err(Abort::Changed) => {
                // Left alone: the next walk records its new identity.
                counters.changed += 1;
            }
            Err(Abort::Io(e)) => {
                counters.errors += 1;
                *last_error = Some(format!("{}: {e}", c.path));
                store_error(conn, &c, &e).map_err(db_err)?;
            }
        }
        let errors = counters.errors;
        let le = last_error.clone();
        set_live(live, |l| {
            l.hash_files_done += 1;
            l.hash_errors = errors;
            l.last_error = le;
        });
    }
    Ok(HashOutcome::Completed)
}

/// Run both passes. Resumable for free: each finished file is stored at once,
/// and candidates are recomputed from what is missing.
pub fn run_hash(
    conn: &Connection,
    read_rate_mbps: f64,
    control: &RunControl,
    live: &Mutex<LiveProgress>,
    counters: &mut HashCounters,
    last_error: &mut Option<String>,
) -> Result<HashOutcome, String> {
    let db_err = |e: rusqlite::Error| format!("dedupe.db error: {e}");
    let mut limiter = RateLimiter::new(read_rate_mbps * 1_000_000.0);
    let env = PassEnv {
        conn,
        control,
        live,
    };
    let partial = partial_candidates(conn).map_err(db_err)?;
    if run_pass(
        &env,
        Pass::Partial,
        partial,
        &mut limiter,
        counters,
        last_error,
    )? == HashOutcome::Stopped
    {
        return Ok(HashOutcome::Stopped);
    }
    let full = full_candidates(conn).map_err(db_err)?;
    run_pass(&env, Pass::Full, full, &mut limiter, counters, last_error)
}

/// Hex form of a hash for the API.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// True when the two files have exactly the same content (streamed). Used by
/// tests here; phase 3 does its own verified comparison.
#[cfg(test)]
fn same_content(a: &std::path::Path, b: &std::path::Path) -> bool {
    std::fs::read(a).ok() == std::fs::read(b).ok()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn partial_hash_ignores_untouched_middle_bytes() {
        // Same size, same head/middle/tail, different byte elsewhere: the
        // partial hashes collide, which is exactly why the full pass exists.
        let dir = std::env::temp_dir().join(format!("ms-dedupe-hash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let size = 8 * PARTIAL_CHUNK as usize;
        let a = vec![7u8; size];
        let mut b = a.clone();
        b[2 * PARTIAL_CHUNK as usize] = 8;
        std::fs::write(dir.join("a"), &a).unwrap();
        std::fs::write(dir.join("b"), &b).unwrap();
        assert!(!same_content(&dir.join("a"), &dir.join("b")));

        let control = RunControl::new();
        let live = Mutex::new(LiveProgress::default());
        let mut limiter = RateLimiter::new(0.0);
        let cand = |name: &str| {
            let meta = std::fs::metadata(dir.join(name)).unwrap();
            let (dev, ino, size, mtime_ns) = identity(&meta);
            Candidate {
                dev,
                ino,
                size,
                mtime_ns,
                path: dir.join(name).to_string_lossy().into(),
            }
        };
        let mut reader = Reader {
            limiter: &mut limiter,
            control: &control,
            live: &live,
            bytes: 0,
        };
        let (pa, fa) = hash_partial(&cand("a"), &mut reader).ok().unwrap();
        let (pb, _) = hash_partial(&cand("b"), &mut reader).ok().unwrap();
        assert_eq!(pa, pb);
        assert!(fa.is_none());
        assert_eq!(reader.bytes, 6 * PARTIAL_CHUNK as i64);
        let full_a = hash_full(&cand("a"), &mut reader).ok().unwrap();
        let full_b = hash_full(&cand("b"), &mut reader).ok().unwrap();
        assert_ne!(full_a, full_b);

        // A changed file is detected, not hashed.
        let mut stale = cand("a");
        stale.mtime_ns -= 1;
        assert!(matches!(
            hash_partial(&stale, &mut reader),
            Err(Abort::Changed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
