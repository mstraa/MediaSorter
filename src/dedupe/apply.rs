//! Phase 3: replace duplicates with hardlinks to one kept inode.
//!
//! Nothing here runs without an explicit user request, and simulation is the
//! default. For every duplicate path, at execution time:
//!
//! 1. `lstat` again: same dev, ino, size and mtime as the scan recorded,
//!    otherwise skip with the reason. Same for the kept file.
//! 2. Byte-for-byte streamed comparison with the kept file (once per
//!    duplicate inode). The hash found the candidates; it is not the proof.
//! 3. Atomic replacement, never deleting first:
//!    `link(kept, <dir>/.ms-dedupe-<uuid>.tmp)`, re-check both files,
//!    `rename(tmp, duplicate)` (the path is never missing), check the path now
//!    has the kept inode, `fsync` the directory.
//! 4. `EXDEV`, `EPERM`, `EACCES` or any detected change: no action, the reason
//!    is journaled. There is never a fallback to copying.
//!
//! A crash between `link` and `rename` leaves one extra link named
//! `.ms-dedupe-*.tmp`; its path is recorded in `pending_links` before the
//! `link`, and [`recover_pending_links`] removes it at the next start. The
//! data is never at risk: the duplicate still exists under its real name.
//!
//! Cancellation is honoured between files, never inside a replacement.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::groups::{choose_keeper, rebuild_groups, MemberView};
use super::hash::hex;
use super::settings::DedupeSettings;
use super::throttle::{RateLimiter, RunControl};

const TMP_PREFIX: &str = ".ms-dedupe-";
const TMP_SUFFIX: &str = ".tmp";
const COMPARE_CHUNK: usize = 4 * 1024 * 1024;
/// Results kept in memory for the live job view (the journal has them all).
const RECENT_RESULTS: usize = 300;

// ---------------------------------------------------------------------------
// Filesystem operations (an abstraction so tests can inject EXDEV or a crash)
// ---------------------------------------------------------------------------

pub trait FsOps: Send + Sync {
    fn hard_link(&self, original: &Path, link: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
}

pub struct RealFs;

impl FsOps for RealFs {
    fn hard_link(&self, original: &Path, link: &Path) -> io::Result<()> {
        fs::hard_link(original, link)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        File::open(dir)?.sync_all()
    }
}

/// Identity of a file as `lstat` sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ident {
    pub dev: i64,
    pub ino: i64,
    pub size: i64,
    pub mtime_ns: i64,
    pub nlink: i64,
    pub uid: i64,
    pub gid: i64,
    pub mode: i64,
    pub is_file: bool,
}

impl Ident {
    fn same_file(&self, other: &Ident) -> bool {
        (self.dev, self.ino, self.size, self.mtime_ns)
            == (other.dev, other.ino, other.size, other.mtime_ns)
    }
}

#[cfg(unix)]
pub fn lstat(path: &Path) -> io::Result<Ident> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::symlink_metadata(path)?;
    Ok(Ident {
        dev: m.dev() as i64,
        ino: m.ino() as i64,
        size: m.size() as i64,
        mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
        nlink: m.nlink() as i64,
        uid: m.uid() as i64,
        gid: m.gid() as i64,
        mode: m.mode() as i64,
        is_file: m.file_type().is_file(),
    })
}

#[cfg(not(unix))]
pub fn lstat(_path: &Path) -> io::Result<Ident> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "hardlink dedupe requires Unix",
    ))
}

/// Human reason for an I/O error, naming the errno the spec cares about.
pub fn describe_io(e: &io::Error) -> String {
    #[cfg(unix)]
    {
        let name = match e.raw_os_error() {
            Some(libc::EXDEV) => {
                Some("EXDEV: not the same filesystem/mount (mount the dataset once in the LXC)")
            }
            Some(libc::EPERM) => Some("EPERM: operation not permitted"),
            Some(libc::EACCES) => Some("EACCES: permission denied"),
            Some(libc::EROFS) => Some("EROFS: read-only filesystem"),
            Some(libc::EMLINK) => Some("EMLINK: too many links to the kept file"),
            _ => None,
        };
        if let Some(name) = name {
            return format!("{name} ({e})");
        }
    }
    e.to_string()
}

fn is_tmp_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with(TMP_PREFIX) && n.ends_with(TMP_SUFFIX))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Crash recovery
// ---------------------------------------------------------------------------

/// Remove `.ms-dedupe-*.tmp` links left by an interrupted replacement.
///
/// Safe by construction: such a file is an *extra* link to the kept inode.
/// As a guard it is only removed while it still has another link
/// (`nlink >= 2`); a lone one is kept and reported.
pub fn recover_pending_links(conn: &Connection, fs_ops: &dyn FsOps) -> rusqlite::Result<usize> {
    let paths: Vec<String> = conn
        .prepare("SELECT path FROM pending_links")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut removed = 0;
    for path in paths {
        let p = Path::new(&path);
        match lstat(p) {
            Ok(id) if is_tmp_name(p) && id.is_file && id.nlink >= 2 => {
                match fs_ops.remove_file(p) {
                    Ok(()) => {
                        removed += 1;
                        tracing::info!("dedupe: removed leftover temporary link {path}");
                        if let Some(dir) = p.parent() {
                            let _ = fs_ops.sync_dir(dir);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("dedupe: cannot remove leftover {path}: {e}");
                        continue;
                    }
                }
            }
            Ok(_) => {
                tracing::warn!(
                    "dedupe: leftover {path} is not a removable extra link; left in place"
                );
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!("dedupe: cannot inspect leftover {path}: {e}");
                continue;
            }
        }
        conn.execute("DELETE FROM pending_links WHERE path = ?1", params![path])?;
    }
    Ok(removed)
}

// ---------------------------------------------------------------------------
// Requests and plans
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Simulate,
    Apply,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Simulate => "simulate",
            Mode::Apply => "apply",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct GroupSelection {
    pub group_id: i64,
    /// Inode to keep; the documented rule picks one when absent.
    pub keeper_ino: Option<i64>,
    /// Duplicate inodes the user unticked.
    #[serde(default)]
    pub exclude_inos: Vec<i64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ApplyRequest {
    pub mode: Mode,
    pub groups: Vec<GroupSelection>,
}

#[derive(Clone, Debug)]
struct PathRow {
    path: String,
    mode: i64,
}

#[derive(Clone, Debug)]
struct MemberPlan {
    dev: i64,
    ino: i64,
    size: i64,
    mtime_ns: i64,
    nlink: i64,
    uid: i64,
    gid: i64,
    paths: Vec<PathRow>,
}

#[derive(Clone, Debug)]
struct GroupPlan {
    id: i64,
    hash: String,
    keeper: MemberPlan,
    duplicates: Vec<MemberPlan>,
}

fn load_plan(
    conn: &Connection,
    selection: &GroupSelection,
    settings: &DedupeSettings,
) -> Result<GroupPlan, String> {
    let db = |e: rusqlite::Error| format!("dedupe.db error: {e}");
    let hash: Option<Vec<u8>> = conn
        .query_row(
            "SELECT full_hash FROM dup_groups WHERE id = ?1",
            params![selection.group_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(db)?;
    let hash = hash.ok_or_else(|| {
        format!(
            "Group {} no longer exists (re-scan or refresh the page)",
            selection.group_id
        )
    })?;
    let mut members: Vec<MemberPlan> = conn
        .prepare(
            "SELECT dev, ino, size, mtime_ns, nlink, uid, gid FROM dup_group_members
             WHERE group_id = ?1 ORDER BY ino",
        )
        .map_err(db)?
        .query_map(params![selection.group_id], |row| {
            Ok(MemberPlan {
                dev: row.get(0)?,
                ino: row.get(1)?,
                size: row.get(2)?,
                mtime_ns: row.get(3)?,
                nlink: row.get(4)?,
                uid: row.get(5)?,
                gid: row.get(6)?,
                paths: Vec::new(),
            })
        })
        .map_err(db)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db)?;
    let mut paths_stmt = conn
        .prepare(
            "SELECT path, mode FROM files WHERE dev = ?1 AND ino = ?2 AND gone_at IS NULL
             ORDER BY path",
        )
        .map_err(db)?;
    for member in &mut members {
        member.paths = paths_stmt
            .query_map(params![member.dev, member.ino], |row| {
                Ok(PathRow {
                    path: row.get(0)?,
                    mode: row.get(1)?,
                })
            })
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)?;
        // Every path acted on must stay inside the dedupe roots.
        member.paths.retain(|p| {
            settings
                .roots
                .iter()
                .any(|root| Path::new(&p.path).starts_with(root))
        });
    }
    let keeper_ino = match selection.keeper_ino {
        Some(ino) => {
            if !members.iter().any(|m| m.ino == ino) {
                return Err(format!(
                    "Inode {ino} is not part of group {}",
                    selection.group_id
                ));
            }
            ino
        }
        None => {
            let views: Vec<MemberView> = members
                .iter()
                .map(|m| MemberView {
                    ino: m.ino,
                    nlink: m.nlink,
                    uid: m.uid,
                    gid: m.gid,
                    mtime: m.mtime_ns.div_euclid(1_000_000_000),
                    keep: false,
                    paths: Vec::new(),
                })
                .collect();
            choose_keeper(&views, settings.preferred_owner)
                .ok_or_else(|| format!("Group {} has no members", selection.group_id))?
        }
    };
    let keeper_index = members
        .iter()
        .position(|m| m.ino == keeper_ino)
        .expect("keeper is a member");
    let keeper = members.remove(keeper_index);
    let duplicates = members
        .into_iter()
        .filter(|m| !selection.exclude_inos.contains(&m.ino))
        .collect();
    Ok(GroupPlan {
        id: selection.group_id,
        hash: hex(&hash),
        keeper,
        duplicates,
    })
}

// ---------------------------------------------------------------------------
// Job progress
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct OpResult {
    pub group_id: i64,
    pub path: String,
    pub result: String,
    pub reason: Option<String>,
    pub freed_bytes: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ApplyProgress {
    pub id: String,
    pub mode: String,
    /// running | done | cancelled | failed
    pub state: String,
    pub groups_total: usize,
    pub groups_done: usize,
    pub paths_total: usize,
    pub paths_done: usize,
    pub bytes_to_compare: i64,
    pub bytes_compared: i64,
    pub freed_bytes: i64,
    /// Space a simulation expects to free.
    pub expected_freed_bytes: i64,
    pub simulated: usize,
    pub replaced: usize,
    pub skipped: usize,
    pub failed: usize,
    pub current: String,
    pub error: Option<String>,
    pub recent: Vec<OpResult>,
}

pub struct ApplyJob {
    pub id: String,
    pub control: RunControl,
    progress: Mutex<ApplyProgress>,
}

impl ApplyJob {
    pub fn snapshot(&self) -> ApplyProgress {
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn update(&self, f: impl FnOnce(&mut ApplyProgress)) {
        f(&mut self.progress.lock().unwrap_or_else(|e| e.into_inner()));
    }

    pub fn is_running(&self) -> bool {
        self.snapshot().state == "running"
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

struct Ctx<'a> {
    conn: &'a Connection,
    fs: &'a dyn FsOps,
    job: &'a ApplyJob,
    mode: Mode,
    limiter: RateLimiter,
}

/// One journal row.
struct OpRecord<'a> {
    group: &'a GroupPlan,
    member: &'a MemberPlan,
    path: &'a PathRow,
    keeper_path: Option<&'a str>,
    keeper: Option<&'a Ident>,
    after_ino: Option<i64>,
    result: &'a str,
    reason: Option<String>,
    freed: i64,
}

impl Ctx<'_> {
    fn journal(&self, r: OpRecord<'_>) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO dedupe_ops (job_id, mode, group_id, path, keeper_path, content_hash,
                    before_dev, before_ino, before_size, before_mtime_ns, before_uid, before_gid,
                    before_mode, keeper_ino, keeper_uid, keeper_gid, keeper_mode, after_ino,
                    result, reason, freed_bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                         ?17, ?18, ?19, ?20, ?21)",
                params![
                    self.job.id,
                    self.mode.as_str(),
                    r.group.id,
                    r.path.path,
                    r.keeper_path,
                    r.group.hash,
                    r.member.dev,
                    r.member.ino,
                    r.member.size,
                    r.member.mtime_ns,
                    r.member.uid,
                    r.member.gid,
                    r.path.mode,
                    r.group.keeper.ino,
                    r.keeper.map(|k| k.uid),
                    r.keeper.map(|k| k.gid),
                    r.keeper.map(|k| k.mode),
                    r.after_ino,
                    r.result,
                    r.reason,
                    r.freed,
                ],
            )
            .map_err(|e| format!("dedupe.db error: {e}"))?;
        let result = OpResult {
            group_id: r.group.id,
            path: r.path.path.clone(),
            result: r.result.to_string(),
            reason: r.reason,
            freed_bytes: r.freed,
        };
        self.job.update(|p| {
            p.paths_done += 1;
            match r.result {
                "simulated" => {
                    p.simulated += 1;
                    p.expected_freed_bytes += r.freed;
                }
                "replaced" => {
                    p.replaced += 1;
                    p.freed_bytes += r.freed;
                }
                "skipped" => p.skipped += 1,
                _ => p.failed += 1,
            }
            p.recent.push(result);
            if p.recent.len() > RECENT_RESULTS {
                let excess = p.recent.len() - RECENT_RESULTS;
                p.recent.drain(..excess);
            }
        });
        Ok(())
    }

    /// Stream both files and compare. `Ok(None)` = identical,
    /// `Ok(Some(reason))` = different, `Err` = stop requested or I/O error.
    fn compare(&mut self, a: &Path, b: &Path, size: i64) -> Result<Option<String>, CompareAbort> {
        let mut fa = File::open(a).map_err(|e| CompareAbort::Io(describe_io(&e)))?;
        let mut fb = File::open(b).map_err(|e| CompareAbort::Io(describe_io(&e)))?;
        let mut ba = vec![0u8; COMPARE_CHUNK];
        let mut bb = vec![0u8; COMPARE_CHUNK];
        let mut offset: i64 = 0;
        loop {
            if self.job.control.pause_requested() && !self.job.control.wait_while_paused() {
                return Err(CompareAbort::Stop);
            }
            let na = read_full(&mut fa, &mut ba).map_err(|e| CompareAbort::Io(describe_io(&e)))?;
            let nb = read_full(&mut fb, &mut bb).map_err(|e| CompareAbort::Io(describe_io(&e)))?;
            if na != nb {
                return Ok(Some(format!(
                    "content differs: one file ends early at byte {}",
                    offset + na.min(nb) as i64
                )));
            }
            if na == 0 {
                break;
            }
            if let Some(i) = ba[..na].iter().zip(&bb[..nb]).position(|(x, y)| x != y) {
                return Ok(Some(format!(
                    "content differs at byte {}",
                    offset + i as i64
                )));
            }
            offset += na as i64;
            self.job.update(|p| p.bytes_compared += 2 * na as i64);
            if !self.limiter.acquire(2.0 * na as f64, &self.job.control) {
                return Err(CompareAbort::Stop);
            }
        }
        if offset != size {
            return Ok(Some(format!(
                "size changed during comparison ({offset} bytes read, {size} expected)"
            )));
        }
        Ok(None)
    }

    /// The atomic swap for one path. Returns the journal result and reason.
    fn replace(
        &self,
        keeper_path: &Path,
        keeper: &Ident,
        dup_path: &Path,
        expected: &Ident,
    ) -> Result<(&'static str, Option<String>, Option<i64>), String> {
        let db = |e: rusqlite::Error| format!("dedupe.db error: {e}");
        let Some(dir) = dup_path.parent() else {
            return Ok(("skipped", Some("path has no parent folder".into()), None));
        };
        let tmp = dir.join(format!("{TMP_PREFIX}{}{TMP_SUFFIX}", uuid::Uuid::new_v4()));
        let tmp_str = tmp.to_string_lossy().to_string();
        // Recorded (and committed) before the link exists, so a crash at any
        // point after this leaves a row naming the leftover.
        self.conn
            .execute(
                "INSERT OR REPLACE INTO pending_links (path) VALUES (?1)",
                params![tmp_str],
            )
            .map_err(db)?;
        let forget_tmp = || {
            self.conn
                .execute(
                    "DELETE FROM pending_links WHERE path = ?1",
                    params![tmp_str],
                )
                .map(|_| ())
                .map_err(db)
        };
        let drop_tmp = || -> Result<(), String> {
            match self.fs.remove_file(&tmp) {
                Ok(()) => forget_tmp(),
                // Kept in pending_links: startup recovery retries it.
                Err(e) => {
                    tracing::warn!("dedupe: cannot remove {tmp_str}: {e}");
                    Ok(())
                }
            }
        };

        if let Err(e) = self.fs.hard_link(keeper_path, &tmp) {
            forget_tmp()?;
            let kind = if matches!(
                e.raw_os_error(),
                Some(code) if is_refusal(code)
            ) {
                "skipped"
            } else {
                "failed"
            };
            return Ok((kind, Some(format!("link: {}", describe_io(&e))), None));
        }

        // Last look before the swap: both files still exactly as verified.
        match lstat(dup_path) {
            Ok(now) if now.same_file(expected) => {}
            Ok(_) => {
                drop_tmp()?;
                return Ok((
                    "skipped",
                    Some("duplicate changed during the operation".into()),
                    None,
                ));
            }
            Err(e) => {
                drop_tmp()?;
                return Ok((
                    "skipped",
                    Some(format!("duplicate: {}", describe_io(&e))),
                    None,
                ));
            }
        }
        match lstat(keeper_path) {
            Ok(now) if now.same_file(keeper) => {}
            _ => {
                drop_tmp()?;
                return Ok((
                    "skipped",
                    Some("kept file changed during the operation".into()),
                    None,
                ));
            }
        }

        if let Err(e) = self.fs.rename(&tmp, dup_path) {
            drop_tmp()?;
            return Ok(("failed", Some(format!("rename: {}", describe_io(&e))), None));
        }
        forget_tmp()?;
        let after = lstat(dup_path).map_err(|e| format!("{}: {e}", dup_path.display()))?;
        if after.dev != keeper.dev || after.ino != keeper.ino {
            return Ok((
                "failed",
                Some(format!(
                    "after rename the path has inode {} instead of {}",
                    after.ino, keeper.ino
                )),
                Some(after.ino),
            ));
        }
        if let Err(e) = self.fs.sync_dir(dir) {
            // The rename happened; durability of the directory entry is
            // what failed. Report it, but the operation is done.
            return Ok((
                "replaced",
                Some(format!("fsync of the folder failed: {}", describe_io(&e))),
                Some(after.ino),
            ));
        }
        Ok(("replaced", None, Some(after.ino)))
    }

    fn run_group(&mut self, group: &GroupPlan) -> Result<bool, String> {
        // The kept file: first of its paths that is still exactly as scanned.
        let mut keeper_found: Option<(String, Ident)> = None;
        for p in &group.keeper.paths {
            if let Ok(id) = lstat(Path::new(&p.path)) {
                let expected = (
                    group.keeper.dev,
                    group.keeper.ino,
                    group.keeper.size,
                    group.keeper.mtime_ns,
                );
                if id.is_file && (id.dev, id.ino, id.size, id.mtime_ns) == expected {
                    keeper_found = Some((p.path.clone(), id));
                    break;
                }
            }
        }
        let Some((keeper_path, keeper)) = keeper_found else {
            for member in &group.duplicates {
                for path in &member.paths {
                    self.journal(OpRecord {
                        group,
                        member,
                        path,
                        keeper_path: None,
                        keeper: None,
                        after_ino: None,
                        result: "skipped",
                        reason: Some("the kept file changed or disappeared since the scan".into()),
                        freed: 0,
                    })?;
                }
            }
            return Ok(true);
        };

        for member in &group.duplicates {
            if self.job.control.stop_requested() {
                return Ok(false);
            }
            let expected = Ident {
                dev: member.dev,
                ino: member.ino,
                size: member.size,
                mtime_ns: member.mtime_ns,
                nlink: member.nlink,
                uid: member.uid,
                gid: member.gid,
                mode: 0,
                is_file: true,
            };
            // 1. Re-stat every path of this inode.
            let mut ok_paths: Vec<&PathRow> = Vec::new();
            let mut current_nlink = member.nlink;
            for path in &member.paths {
                let reason = match lstat(Path::new(&path.path)) {
                    Ok(id) if !id.is_file => Some("no longer a regular file".to_string()),
                    Ok(id) if id.dev != keeper.dev => {
                        Some("on another filesystem than the kept file".to_string())
                    }
                    Ok(id) if id.ino == keeper.ino => {
                        Some("already linked to the kept file".to_string())
                    }
                    Ok(id) if !id.same_file(&expected) => Some(format!(
                        "changed since the scan (inode {}→{}, size {}→{}, mtime changed: {})",
                        member.ino,
                        id.ino,
                        member.size,
                        id.size,
                        id.mtime_ns != member.mtime_ns
                    )),
                    Ok(id) => {
                        current_nlink = id.nlink;
                        None
                    }
                    Err(e) => Some(describe_io(&e)),
                };
                match reason {
                    None => ok_paths.push(path),
                    Some(reason) => self.journal(OpRecord {
                        group,
                        member,
                        path,
                        keeper_path: Some(&keeper_path),
                        keeper: Some(&keeper),
                        after_ino: None,
                        result: "skipped",
                        reason: Some(reason),
                        freed: 0,
                    })?,
                }
            }
            if ok_paths.is_empty() {
                continue;
            }
            // Space comes back only if every link of the inode is replaced.
            let other_links = current_nlink - ok_paths.len() as i64;
            let link_note = (other_links > 0).then(|| {
                format!(
                    "{other_links} other link(s) to this copy exist outside the selection: \
                     no space freed until they go too"
                )
            });

            if self.mode == Mode::Simulate {
                for (i, path) in ok_paths.iter().enumerate() {
                    let mut notes: Vec<String> = Vec::new();
                    if (member.uid, member.gid) != (keeper.uid, keeper.gid) {
                        notes.push(format!(
                            "owner becomes {}:{} (was {}:{})",
                            keeper.uid, keeper.gid, member.uid, member.gid
                        ));
                    }
                    if (path.mode & 0o7777) != (keeper.mode & 0o7777) {
                        notes.push(format!(
                            "mode becomes {:o} (was {:o})",
                            keeper.mode & 0o7777,
                            path.mode & 0o7777
                        ));
                    }
                    if let Some(note) = &link_note {
                        notes.push(note.clone());
                    }
                    if !dir_writable(Path::new(&path.path)) {
                        self.journal(OpRecord {
                            group,
                            member,
                            path,
                            keeper_path: Some(&keeper_path),
                            keeper: Some(&keeper),
                            after_ino: None,
                            result: "skipped",
                            reason: Some("folder is not writable by the service".into()),
                            freed: 0,
                        })?;
                        continue;
                    }
                    let last = i + 1 == ok_paths.len();
                    self.journal(OpRecord {
                        group,
                        member,
                        path,
                        keeper_path: Some(&keeper_path),
                        keeper: Some(&keeper),
                        after_ino: Some(keeper.ino),
                        result: "simulated",
                        reason: (!notes.is_empty()).then(|| notes.join("; ")),
                        freed: if last && other_links <= 0 {
                            member.size
                        } else {
                            0
                        },
                    })?;
                }
                continue;
            }

            // 2. Byte-for-byte comparison, once per inode.
            self.job
                .update(|p| p.current = format!("comparing {}", ok_paths[0].path));
            match self.compare(
                Path::new(&keeper_path),
                Path::new(&ok_paths[0].path),
                member.size,
            ) {
                Ok(None) => {}
                Ok(Some(reason)) | Err(CompareAbort::Io(reason)) => {
                    // A content mismatch is a normal "no action"; an I/O error
                    // while reading is a failure worth looking at.
                    let failed = !reason.starts_with("content differs")
                        && !reason.starts_with("size changed");
                    for path in &ok_paths {
                        self.journal(OpRecord {
                            group,
                            member,
                            path,
                            keeper_path: Some(&keeper_path),
                            keeper: Some(&keeper),
                            after_ino: None,
                            result: if failed { "failed" } else { "skipped" },
                            reason: Some(reason.clone()),
                            freed: 0,
                        })?;
                    }
                    continue;
                }
                Err(CompareAbort::Stop) => return Ok(false),
            }

            // 3. Replace each path. Cancellation is checked before each one,
            // never inside.
            let mut replaced_all = true;
            for (i, path) in ok_paths.iter().enumerate() {
                if self.job.control.stop_requested() {
                    return Ok(false);
                }
                self.job.update(|p| p.current = path.path.clone());
                let (result, reason, after) = self.replace(
                    Path::new(&keeper_path),
                    &keeper,
                    Path::new(&path.path),
                    &expected,
                )?;
                if result != "replaced" {
                    replaced_all = false;
                } else {
                    self.update_files_after(&path.path)?;
                }
                let last = i + 1 == ok_paths.len();
                let freed = if result == "replaced" && last && replaced_all && other_links <= 0 {
                    member.size
                } else {
                    0
                };
                let reason = match (reason, &link_note) {
                    (Some(r), _) => Some(r),
                    (None, Some(note)) if result == "replaced" && last => Some(note.clone()),
                    _ => None,
                };
                self.journal(OpRecord {
                    group,
                    member,
                    path,
                    keeper_path: Some(&keeper_path),
                    keeper: Some(&keeper),
                    after_ino: after,
                    result,
                    reason,
                    freed,
                })?;
            }
        }
        Ok(true)
    }

    /// Keep `files` in step with the disk so the groups rebuilt at the end
    /// of the job no longer show what was just merged.
    fn update_files_after(&self, path: &str) -> Result<(), String> {
        let db = |e: rusqlite::Error| format!("dedupe.db error: {e}");
        let id = match lstat(Path::new(path)) {
            Ok(id) => id,
            Err(_) => return Ok(()),
        };
        self.conn
            .execute(
                "UPDATE files SET dev = ?2, ino = ?3, size = ?4, mtime_ns = ?5, nlink = ?6,
                    uid = ?7, gid = ?8, mode = ?9, updated_at = CURRENT_TIMESTAMP
                 WHERE path = ?1",
                params![
                    path,
                    id.dev,
                    id.ino,
                    id.size,
                    id.mtime_ns,
                    id.nlink,
                    id.uid,
                    id.gid,
                    id.mode
                ],
            )
            .map_err(db)?;
        self.conn
            .execute(
                "UPDATE files SET nlink = ?3 WHERE dev = ?1 AND ino = ?2",
                params![id.dev, id.ino, id.nlink],
            )
            .map_err(db)?;
        Ok(())
    }
}

enum CompareAbort {
    Stop,
    Io(String),
}

fn read_full(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

#[cfg(unix)]
fn is_refusal(code: i32) -> bool {
    matches!(code, libc::EXDEV | libc::EPERM | libc::EACCES | libc::EROFS)
}

#[cfg(not(unix))]
fn is_refusal(_code: i32) -> bool {
    false
}

#[cfg(unix)]
fn dir_writable(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Some(dir) = path.parent() else {
        return false;
    };
    let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

#[cfg(not(unix))]
fn dir_writable(_path: &Path) -> bool {
    false
}

/// Run a job to completion (blocking; on its own thread).
pub fn run_job(
    db_path: &Path,
    settings: &DedupeSettings,
    request: &ApplyRequest,
    job: &ApplyJob,
    fs_ops: &dyn FsOps,
) -> Result<(), String> {
    let conn = super::store::open_connection(db_path)
        .map_err(|e| format!("Cannot open dedupe.db: {e}"))?;
    let mut plans = Vec::new();
    for selection in &request.groups {
        plans.push(load_plan(&conn, selection, settings)?);
    }
    let paths_total: usize = plans
        .iter()
        .flat_map(|g| g.duplicates.iter())
        .map(|m| m.paths.len())
        .sum();
    let bytes_to_compare: i64 = if request.mode == Mode::Apply {
        plans
            .iter()
            .flat_map(|g| g.duplicates.iter())
            .map(|m| 2 * m.size)
            .sum()
    } else {
        0
    };
    job.update(|p| {
        p.groups_total = plans.len();
        p.paths_total = paths_total;
        p.bytes_to_compare = bytes_to_compare;
    });
    let mut ctx = Ctx {
        conn: &conn,
        fs: fs_ops,
        job,
        mode: request.mode,
        limiter: RateLimiter::new(settings.read_rate_mbps * 1_000_000.0),
    };
    let mut stopped = false;
    for plan in &plans {
        if job.control.stop_requested() || !ctx.run_group(plan)? {
            stopped = true;
            break;
        }
        job.update(|p| p.groups_done += 1);
    }
    if request.mode == Mode::Apply && job.snapshot().replaced > 0 {
        let roots = super::scan::effective_roots(&settings.roots);
        rebuild_groups(&conn, &roots).map_err(|e| format!("dedupe.db error: {e}"))?;
    }
    job.update(|p| {
        p.current.clear();
        p.state = if stopped { "cancelled" } else { "done" }.into();
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Job manager
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct ApplyJobs {
    jobs: Arc<Mutex<Vec<Arc<ApplyJob>>>>,
}

impl ApplyJobs {
    pub fn running(&self) -> Option<Arc<ApplyJob>> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|j| j.is_running())
            .cloned()
    }

    pub fn get(&self, id: &str) -> Option<Arc<ApplyJob>> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|j| j.id == id)
            .cloned()
    }

    pub fn latest(&self) -> Option<Arc<ApplyJob>> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last()
            .cloned()
    }

    pub fn start(
        &self,
        db_path: PathBuf,
        settings: DedupeSettings,
        request: ApplyRequest,
    ) -> Result<Arc<ApplyJob>, String> {
        let job = Arc::new(ApplyJob {
            id: uuid::Uuid::new_v4().to_string(),
            control: RunControl::new(),
            progress: Mutex::new(ApplyProgress::default()),
        });
        job.update(|p| {
            p.id = job.id.clone();
            p.mode = request.mode.as_str().into();
            p.state = "running".into();
        });
        {
            let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
            if jobs.iter().any(|j| j.is_running()) {
                return Err("Another duplicate job is already running".into());
            }
            jobs.push(job.clone());
            // Keep a short in-memory history; the journal is the record.
            if jobs.len() > 20 {
                jobs.remove(0);
            }
        }
        let worker = job.clone();
        std::thread::Builder::new()
            .name("dedupe-apply".into())
            .spawn(move || {
                if let Err(e) = run_job(&db_path, &settings, &request, &worker, &RealFs) {
                    tracing::warn!("dedupe job failed: {e}");
                    worker.update(|p| {
                        p.state = "failed".into();
                        p.error = Some(e);
                    });
                }
            })
            .map_err(|e| format!("Cannot start the job thread: {e}"))?;
        Ok(job)
    }
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct HistoryRow {
    pub id: i64,
    pub job_id: String,
    pub mode: String,
    pub at: String,
    pub group_id: Option<i64>,
    pub path: String,
    pub keeper_path: Option<String>,
    pub content_hash: Option<String>,
    pub before_ino: Option<i64>,
    pub before_size: Option<i64>,
    pub before_owner: Option<String>,
    pub before_mode: Option<String>,
    pub keeper_ino: Option<i64>,
    pub keeper_owner: Option<String>,
    pub after_ino: Option<i64>,
    pub result: String,
    pub reason: Option<String>,
    pub freed_bytes: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct HistoryPage {
    pub rows: Vec<HistoryRow>,
    pub total: i64,
    pub total_freed: i64,
}

pub fn history(
    conn: &Connection,
    mode: Option<&str>,
    limit: i64,
    offset: i64,
) -> rusqlite::Result<HistoryPage> {
    let (total, total_freed) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(CASE WHEN result = 'replaced' THEN freed_bytes END), 0)
         FROM dedupe_ops WHERE (?1 IS NULL OR mode = ?1)",
        params![mode],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let rows = conn
        .prepare(
            "SELECT id, job_id, mode, at, group_id, path, keeper_path, content_hash, before_ino,
                    before_size, before_uid, before_gid, before_mode, keeper_ino, keeper_uid,
                    keeper_gid, after_ino, result, reason, freed_bytes
             FROM dedupe_ops WHERE (?1 IS NULL OR mode = ?1)
             ORDER BY id DESC LIMIT ?2 OFFSET ?3",
        )?
        .query_map(params![mode, limit, offset], |row| {
            let owner = |u: Option<i64>, g: Option<i64>| match (u, g) {
                (Some(u), Some(g)) => Some(format!("{u}:{g}")),
                _ => None,
            };
            Ok(HistoryRow {
                id: row.get(0)?,
                job_id: row.get(1)?,
                mode: row.get(2)?,
                at: row.get(3)?,
                group_id: row.get(4)?,
                path: row.get(5)?,
                keeper_path: row.get(6)?,
                content_hash: row.get(7)?,
                before_ino: row.get(8)?,
                before_size: row.get(9)?,
                before_owner: owner(row.get(10)?, row.get(11)?),
                before_mode: row
                    .get::<_, Option<i64>>(12)?
                    .map(|m| format!("{:o}", m & 0o7777)),
                keeper_ino: row.get(13)?,
                keeper_owner: owner(row.get(14)?, row.get(15)?),
                after_ino: row.get(16)?,
                result: row.get(17)?,
                reason: row.get(18)?,
                freed_bytes: row.get(19)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(HistoryPage {
        rows,
        total,
        total_freed,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::dedupe::scan::{run_scan, LiveProgress};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Env {
        dir: PathBuf,
        root: PathBuf,
        db: PathBuf,
        settings: DedupeSettings,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn env() -> Env {
        let dir = std::env::temp_dir().join(format!("ms-dedupe-apply-{}", uuid::Uuid::new_v4()));
        let root = dir.join("data");
        fs::create_dir_all(&root).unwrap();
        let settings = DedupeSettings {
            roots: vec![root.clone()],
            min_size_bytes: 1,
            min_age_secs: 0,
            stat_rate: 0,
            read_rate_mbps: 0.0,
            ..Default::default()
        };
        Env {
            db: dir.join("dedupe.db"),
            dir,
            root,
            settings,
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    impl Env {
        fn scan(&self) {
            run_scan(
                &self.db,
                &self.settings,
                "manual",
                false,
                &RunControl::new(),
                &Mutex::new(LiveProgress::default()),
            )
            .unwrap();
        }

        fn group_ids(&self) -> Vec<i64> {
            let conn = super::super::store::open_connection(&self.db).unwrap();
            let ids: Vec<i64> = conn
                .prepare("SELECT id FROM dup_groups ORDER BY id")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            ids
        }

        fn run(&self, mode: Mode, fs_ops: &dyn FsOps) -> ApplyProgress {
            let request = ApplyRequest {
                mode,
                groups: self
                    .group_ids()
                    .into_iter()
                    .map(|group_id| GroupSelection {
                        group_id,
                        keeper_ino: None,
                        exclude_inos: vec![],
                    })
                    .collect(),
            };
            let job = ApplyJob {
                id: uuid::Uuid::new_v4().to_string(),
                control: RunControl::new(),
                progress: Mutex::new(ApplyProgress::default()),
            };
            run_job(&self.db, &self.settings, &request, &job, fs_ops).unwrap();
            job.snapshot()
        }

        fn ino(&self, rel: &str) -> i64 {
            lstat(&self.root.join(rel)).unwrap().ino
        }

        fn leftovers(&self) -> Vec<String> {
            let mut out = Vec::new();
            for entry in walk(&self.root) {
                if is_tmp_name(&entry) {
                    out.push(entry.display().to_string());
                }
            }
            out
        }
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
        out
    }

    const CONTENT: &[u8] = b"the same episode, byte for byte";

    #[test]
    fn identical_files_end_up_on_one_inode_with_content_intact() {
        let e = env();
        write(&e.root.join("Films/a.mkv"), CONTENT);
        write(&e.root.join("Movies/a.mkv"), CONTENT);
        e.scan();
        assert_eq!(e.group_ids().len(), 1);
        assert_ne!(e.ino("Films/a.mkv"), e.ino("Movies/a.mkv"));

        let p = e.run(Mode::Apply, &RealFs);
        assert_eq!((p.replaced, p.skipped, p.failed), (1, 0, 0), "{p:?}");
        assert_eq!(p.freed_bytes, CONTENT.len() as i64);
        assert_eq!(e.ino("Films/a.mkv"), e.ino("Movies/a.mkv"));
        assert_eq!(lstat(&e.root.join("Films/a.mkv")).unwrap().nlink, 2);
        assert_eq!(fs::read(e.root.join("Films/a.mkv")).unwrap(), CONTENT);
        assert_eq!(fs::read(e.root.join("Movies/a.mkv")).unwrap(), CONTENT);
        assert!(e.leftovers().is_empty());
        // Groups rebuilt: nothing left to merge, journal has the operation.
        assert!(e.group_ids().is_empty());
        let conn = super::super::store::open_connection(&e.db).unwrap();
        let h = history(&conn, Some("apply"), 10, 0).unwrap();
        assert_eq!(h.total, 1);
        assert_eq!(h.rows[0].result, "replaced");
        assert!(h.rows[0].before_ino.is_some() && h.rows[0].after_ino.is_some());
        assert_ne!(h.rows[0].before_ino, h.rows[0].after_ino);
    }

    #[test]
    fn simulation_changes_nothing() {
        let e = env();
        write(&e.root.join("A/x.mkv"), CONTENT);
        write(&e.root.join("B/x.mkv"), CONTENT);
        e.scan();
        let before = (e.ino("A/x.mkv"), e.ino("B/x.mkv"));
        let p = e.run(Mode::Simulate, &RealFs);
        assert_eq!((p.simulated, p.replaced), (1, 0));
        assert_eq!(p.expected_freed_bytes, CONTENT.len() as i64);
        assert_eq!((e.ino("A/x.mkv"), e.ino("B/x.mkv")), before);
        assert_eq!(e.group_ids().len(), 1);
        assert!(e.leftovers().is_empty());
    }

    #[test]
    fn content_differing_in_the_last_byte_is_never_linked() {
        // Get a group, then change the duplicate's last byte while keeping
        // its size and mtime: only the byte comparison can catch this.
        let e = env();
        write(&e.root.join("A/x.mkv"), CONTENT);
        write(&e.root.join("B/x.mkv"), CONTENT);
        e.scan();
        let p = e.root.join("B/x.mkv");
        let mtime = filetime::FileTime::from_last_modification_time(&fs::metadata(&p).unwrap());
        let mut changed = CONTENT.to_vec();
        *changed.last_mut().unwrap() ^= 1;
        fs::write(&p, &changed).unwrap();
        filetime::set_file_mtime(&p, mtime).unwrap();
        let before = (e.ino("A/x.mkv"), e.ino("B/x.mkv"));

        let progress = e.run(Mode::Apply, &RealFs);
        assert_eq!(progress.replaced, 0);
        assert_eq!(progress.skipped, 1);
        assert!(progress.recent[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("content differs at byte"));
        assert_eq!((e.ino("A/x.mkv"), e.ino("B/x.mkv")), before);
        assert_eq!(fs::read(&p).unwrap(), changed);
    }

    #[test]
    fn files_changed_since_the_scan_are_skipped_with_a_reason() {
        let e = env();
        for d in ["A", "B", "C", "D"] {
            write(&e.root.join(format!("{d}/x.mkv")), CONTENT);
        }
        e.scan();
        // Keeper is the lowest inode (all equal otherwise): A. Change B's
        // mtime, C's size (same length rewrite would keep size, so append),
        // and D's inode (replace with a new file of identical content).
        let later = filetime::FileTime::from_unix_time(1_600_000_000, 0);
        filetime::set_file_mtime(e.root.join("B/x.mkv"), later).unwrap();
        let mut grown = CONTENT.to_vec();
        grown.push(b'!');
        fs::write(e.root.join("C/x.mkv"), &grown).unwrap();
        fs::remove_file(e.root.join("D/x.mkv")).unwrap();
        write(&e.root.join("D/x.mkv"), CONTENT);

        // Make sure A really is the keeper for this assertion.
        let keeper = ["A", "B", "C", "D"]
            .iter()
            .map(|d| (lstat(&e.root.join(format!("{d}/x.mkv"))).unwrap().ino, *d))
            .min()
            .unwrap()
            .1;
        if keeper != "A" {
            return; // inode order differs on this filesystem; covered elsewhere
        }
        let p = e.run(Mode::Apply, &RealFs);
        assert_eq!(p.replaced, 0, "{p:?}");
        assert_eq!(p.skipped, 3);
        for r in &p.recent {
            assert!(
                r.reason
                    .as_deref()
                    .unwrap()
                    .contains("changed since the scan"),
                "{r:?}"
            );
        }
        assert_eq!(fs::read(e.root.join("C/x.mkv")).unwrap(), grown);
    }

    /// Fails `link` with EXDEV, like two separate LXC mount points would.
    struct ExdevFs;
    impl FsOps for ExdevFs {
        fn hard_link(&self, _: &Path, _: &Path) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(libc::EXDEV))
        }
        fn rename(&self, a: &Path, b: &Path) -> io::Result<()> {
            RealFs.rename(a, b)
        }
        fn remove_file(&self, p: &Path) -> io::Result<()> {
            RealFs.remove_file(p)
        }
        fn sync_dir(&self, d: &Path) -> io::Result<()> {
            RealFs.sync_dir(d)
        }
    }

    #[test]
    fn exdev_means_no_action_and_an_explicit_reason() {
        let e = env();
        write(&e.root.join("A/x.mkv"), CONTENT);
        write(&e.root.join("B/x.mkv"), CONTENT);
        e.scan();
        let before = (e.ino("A/x.mkv"), e.ino("B/x.mkv"));
        let p = e.run(Mode::Apply, &ExdevFs);
        assert_eq!((p.replaced, p.skipped), (0, 1));
        assert!(p.recent[0].reason.as_deref().unwrap().contains("EXDEV"));
        assert_eq!((e.ino("A/x.mkv"), e.ino("B/x.mkv")), before);
        assert_eq!(fs::read(e.root.join("B/x.mkv")).unwrap(), CONTENT);
        assert!(e.leftovers().is_empty());
        let conn = super::super::store::open_connection(&e.db).unwrap();
        let pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending, 0);
    }

    #[test]
    fn real_cross_filesystem_pair_is_refused() {
        // /dev/shm is a tmpfs: a different st_dev from the temp dir.
        let shm = Path::new("/dev/shm");
        if !shm.is_dir() || lstat(shm).unwrap().dev == lstat(&std::env::temp_dir()).unwrap().dev {
            return;
        }
        let e = env();
        let other = shm.join(format!("ms-dedupe-{}", uuid::Uuid::new_v4()));
        write(&other.join("x.mkv"), CONTENT);
        write(&e.root.join("x.mkv"), CONTENT);
        let mut settings = e.settings.clone();
        settings.roots.push(other.clone());
        run_scan(
            &e.db,
            &settings,
            "manual",
            false,
            &RunControl::new(),
            &Mutex::new(LiveProgress::default()),
        )
        .unwrap();
        // Same content, different filesystems: never even grouped.
        assert!(e.group_ids().is_empty());
        let _ = fs::remove_dir_all(&other);
    }

    /// Simulates the process dying between `link` and `rename`.
    struct CrashFs {
        crashed: AtomicBool,
    }
    impl FsOps for CrashFs {
        fn hard_link(&self, a: &Path, b: &Path) -> io::Result<()> {
            RealFs.hard_link(a, b)
        }
        fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
            self.crashed.store(true, Ordering::SeqCst);
            panic!("simulated crash between link and rename");
        }
        fn remove_file(&self, p: &Path) -> io::Result<()> {
            RealFs.remove_file(p)
        }
        fn sync_dir(&self, d: &Path) -> io::Result<()> {
            RealFs.sync_dir(d)
        }
    }

    #[test]
    fn crash_between_link_and_rename_is_cleaned_up_at_startup() {
        let e = env();
        write(&e.root.join("A/x.mkv"), CONTENT);
        write(&e.root.join("B/x.mkv"), CONTENT);
        e.scan();
        let before = (e.ino("A/x.mkv"), e.ino("B/x.mkv"));
        let crash = CrashFs {
            crashed: AtomicBool::new(false),
        };
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.run(Mode::Apply, &crash)));
        assert!(result.is_err());
        assert!(crash.crashed.load(Ordering::SeqCst));
        // State after the "crash": an extra link next to the duplicate.
        let leftovers = e.leftovers();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");

        // "Restart": recovery removes it; both files untouched.
        let conn = super::super::store::open_connection(&e.db).unwrap();
        assert_eq!(recover_pending_links(&conn, &RealFs).unwrap(), 1);
        assert!(e.leftovers().is_empty());
        assert_eq!((e.ino("A/x.mkv"), e.ino("B/x.mkv")), before);
        assert_eq!(fs::read(e.root.join("A/x.mkv")).unwrap(), CONTENT);
        assert_eq!(fs::read(e.root.join("B/x.mkv")).unwrap(), CONTENT);
        assert_eq!(lstat(&e.root.join("A/x.mkv")).unwrap().nlink, 1);
        // Running again now succeeds.
        let p = e.run(Mode::Apply, &RealFs);
        assert_eq!(p.replaced, 1);
    }

    #[test]
    fn recovery_never_removes_a_lone_file() {
        let e = env();
        let lone = e.root.join(".ms-dedupe-0000.tmp");
        write(&lone, CONTENT);
        let conn = super::super::store::open_connection(&e.db).unwrap();
        conn.execute(
            "INSERT INTO pending_links (path) VALUES (?1)",
            params![lone.to_string_lossy()],
        )
        .unwrap();
        assert_eq!(recover_pending_links(&conn, &RealFs).unwrap(), 0);
        assert!(lone.exists(), "nlink 1: it may be the only copy");
    }

    #[test]
    fn excluded_duplicates_and_chosen_keeper_are_respected() {
        let e = env();
        for d in ["A", "B", "C"] {
            write(&e.root.join(format!("{d}/x.mkv")), CONTENT);
        }
        e.scan();
        let group_id = e.group_ids()[0];
        let (a, b, c) = (e.ino("A/x.mkv"), e.ino("B/x.mkv"), e.ino("C/x.mkv"));
        let request = ApplyRequest {
            mode: Mode::Apply,
            groups: vec![GroupSelection {
                group_id,
                keeper_ino: Some(c),
                exclude_inos: vec![b],
            }],
        };
        let job = ApplyJob {
            id: "t".into(),
            control: RunControl::new(),
            progress: Mutex::new(ApplyProgress::default()),
        };
        run_job(&e.db, &e.settings, &request, &job, &RealFs).unwrap();
        assert_eq!(job.snapshot().replaced, 1);
        assert_eq!(e.ino("A/x.mkv"), c);
        assert_eq!(e.ino("B/x.mkv"), b);
        assert_eq!(e.ino("C/x.mkv"), c);
        let _ = a;

        // A keeper outside the group is refused.
        let bad = ApplyRequest {
            mode: Mode::Apply,
            groups: vec![GroupSelection {
                group_id: e.group_ids()[0],
                keeper_ino: Some(123_456_789),
                exclude_inos: vec![],
            }],
        };
        let job = ApplyJob {
            id: "t2".into(),
            control: RunControl::new(),
            progress: Mutex::new(ApplyProgress::default()),
        };
        assert!(run_job(&e.db, &e.settings, &bad, &job, &RealFs).is_err());
    }
}
