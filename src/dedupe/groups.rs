//! Phase 2: duplicate groups.
//!
//! A group is two or more inodes on the same device with the same size and the
//! same full BLAKE3 hash, for their current size/mtime. Paths already sharing
//! an inode are one member (with several paths). Reclaimable space is
//! `size × (inodes − 1)`.
//!
//! Groups are materialised in `dup_groups` / `dup_group_members` at the end of
//! each hash stage so the UI can sort and page them cheaply.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use serde::Serialize;

use super::hash::hex;

/// Folder label used in the per-pair summary ("Films ↔ Movies").
///
/// With several roots the root's own name is the natural label
/// (`/mnt/data/Torrents` → `Torrents`). With a single root (the whole data
/// mount), the first folder below it is.
pub fn folder_label(path: &Path, root: &Path, multi_root: bool) -> String {
    if multi_root {
        if let Some(name) = root.file_name() {
            return name.to_string_lossy().to_string();
        }
    }
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut components = relative.components();
    match (components.next(), components.next()) {
        (Some(first), Some(_)) => first.as_os_str().to_string_lossy().to_string(),
        // A file directly in the root.
        _ => root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "/".into()),
    }
}

fn pair_label(labels: &BTreeSet<String>) -> String {
    labels.iter().cloned().collect::<Vec<_>>().join(" ↔ ")
}

/// Rebuild the groups from the current inodes and hashes. One transaction:
/// readers see the old groups until it commits.
pub fn rebuild_groups(conn: &Connection, roots: &[PathBuf]) -> rusqlite::Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         DELETE FROM dup_group_members;
         DELETE FROM dup_groups;
         DROP TABLE IF EXISTS temp.cur_inodes;
         CREATE TEMP TABLE cur_inodes AS
            SELECT f.dev, f.ino, f.size, MAX(f.mtime_ns) AS mtime_ns, MAX(f.nlink) AS nlink,
                   MIN(f.uid) AS uid, MIN(f.gid) AS gid, h.full AS full_hash
            FROM files f
            JOIN hashes h ON h.dev = f.dev AND h.ino = f.ino
            WHERE f.gone_at IS NULL AND h.full IS NOT NULL
            GROUP BY f.dev, f.ino
            HAVING MAX(f.mtime_ns) = MIN(h.mtime_ns) AND f.size = MIN(h.size);
         INSERT INTO dup_groups (dev, size, full_hash, inode_count, reclaimable)
            SELECT dev, size, full_hash, COUNT(*), size * (COUNT(*) - 1)
            FROM cur_inodes
            GROUP BY dev, size, full_hash
            HAVING COUNT(*) > 1;
         INSERT INTO dup_group_members (group_id, dev, ino, size, mtime_ns, nlink, uid, gid)
            SELECT g.id, c.dev, c.ino, c.size, c.mtime_ns, c.nlink, c.uid, c.gid
            FROM cur_inodes c
            JOIN dup_groups g ON g.dev = c.dev AND g.size = c.size AND g.full_hash = c.full_hash;
         DROP TABLE temp.cur_inodes;",
    )?;

    let result = (|| {
        let multi_root = roots.len() > 1;
        let mut labels: Vec<(i64, String, i64)> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT m.group_id, f.path, f.root
                 FROM dup_group_members m
                 JOIN files f ON f.dev = m.dev AND f.ino = m.ino AND f.gone_at IS NULL
                 ORDER BY m.group_id",
            )?;
            let mut rows = stmt.query([])?;
            let mut current: Option<(i64, BTreeSet<String>, i64)> = None;
            while let Some(row) = rows.next()? {
                let group_id: i64 = row.get(0)?;
                let path: String = row.get(1)?;
                let root: String = row.get(2)?;
                let label = folder_label(Path::new(&path), Path::new(&root), multi_root);
                match current.as_mut() {
                    Some((id, set, count)) if *id == group_id => {
                        set.insert(label);
                        *count += 1;
                    }
                    _ => {
                        if let Some((id, set, count)) = current.take() {
                            labels.push((id, pair_label(&set), count));
                        }
                        current = Some((group_id, BTreeSet::from([label]), 1));
                    }
                }
            }
            if let Some((id, set, count)) = current.take() {
                labels.push((id, pair_label(&set), count));
            }
        }
        let mut update =
            conn.prepare("UPDATE dup_groups SET pair_label = ?2, path_count = ?3 WHERE id = ?1")?;
        for (id, label, count) in labels {
            update.execute(params![id, label, count])?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT"),
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Keeper choice
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct MemberView {
    pub ino: i64,
    pub nlink: i64,
    pub uid: i64,
    pub gid: i64,
    pub mtime: i64,
    pub keep: bool,
    pub paths: Vec<String>,
}

/// The inode to keep, by the documented rule: most links first (it is already
/// what the library points at), then the preferred owner, then the oldest
/// mtime, then the lowest inode number (stable tie-break).
pub fn choose_keeper(members: &[MemberView], preferred_owner: Option<(u32, u32)>) -> Option<i64> {
    members
        .iter()
        .min_by_key(|m| {
            let preferred = preferred_owner
                .map(|(u, g)| m.uid == u as i64 && m.gid == g as i64)
                .unwrap_or(false);
            (
                std::cmp::Reverse(m.nlink),
                std::cmp::Reverse(preferred),
                m.mtime,
                m.ino,
            )
        })
        .map(|m| m.ino)
}

pub const KEEPER_RULE: &str = "most hard links, then the preferred owner, then the oldest \
    modification time";

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct PairSummary {
    pub label: String,
    pub groups: i64,
    pub reclaimable: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct GroupsSummary {
    pub groups: i64,
    pub inodes: i64,
    pub reclaimable: i64,
    pub built_at: Option<String>,
    pub by_pair: Vec<PairSummary>,
}

#[derive(Clone, Debug, Serialize)]
pub struct GroupView {
    pub id: i64,
    pub size: i64,
    pub hash: String,
    pub inode_count: i64,
    pub path_count: i64,
    pub reclaimable: i64,
    pub pair_label: String,
    pub keeper_ino: Option<i64>,
    /// The members do not all share one owner: after linking, every path
    /// gets the kept inode's owner and mode.
    pub owners_differ: bool,
    pub members: Vec<MemberView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct GroupsPage {
    pub summary: GroupsSummary,
    pub keeper_rule: &'static str,
    /// Groups matching the filters.
    pub total: i64,
    pub total_reclaimable: i64,
    pub groups: Vec<GroupView>,
}

#[derive(Clone, Debug, Default)]
pub struct GroupsFilter {
    pub pair: Option<String>,
    pub min_size: i64,
    /// Only groups with a path under this folder.
    pub folder: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

pub fn summary(conn: &Connection) -> rusqlite::Result<GroupsSummary> {
    let mut summary = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(inode_count), 0), COALESCE(SUM(reclaimable), 0),
                MAX(built_at)
         FROM dup_groups",
        [],
        |row| {
            Ok(GroupsSummary {
                groups: row.get(0)?,
                inodes: row.get(1)?,
                reclaimable: row.get(2)?,
                built_at: row.get(3)?,
                by_pair: Vec::new(),
            })
        },
    )?;
    let mut stmt = conn.prepare(
        "SELECT pair_label, COUNT(*), SUM(reclaimable) FROM dup_groups
         GROUP BY pair_label ORDER BY SUM(reclaimable) DESC",
    )?;
    summary.by_pair = stmt
        .query_map([], |row| {
            Ok(PairSummary {
                label: row.get(0)?,
                groups: row.get(1)?,
                reclaimable: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(summary)
}

pub fn list_groups(
    conn: &Connection,
    filter: &GroupsFilter,
    preferred_owner: Option<(u32, u32)>,
) -> rusqlite::Result<GroupsPage> {
    let summary = summary(conn)?;
    let (folder_low, folder_high) = match &filter.folder {
        Some(folder) => {
            let (low, high) = super::store::path_range(Path::new(folder));
            (Some(low), Some(high))
        }
        None => (None, None),
    };
    let where_clause = "WHERE (?1 IS NULL OR g.pair_label = ?1) AND g.size >= ?2
        AND (?3 IS NULL OR EXISTS (
            SELECT 1 FROM dup_group_members m
            JOIN files f ON f.dev = m.dev AND f.ino = m.ino AND f.gone_at IS NULL
            WHERE m.group_id = g.id AND f.path >= ?3 AND f.path < ?4))";
    let (total, total_reclaimable) = conn.query_row(
        &format!(
            "SELECT COUNT(*), COALESCE(SUM(g.reclaimable), 0) FROM dup_groups g {where_clause}"
        ),
        params![filter.pair, filter.min_size, folder_low, folder_high],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut stmt = conn.prepare(&format!(
        "SELECT g.id, g.size, g.full_hash, g.inode_count, g.path_count, g.reclaimable,
                g.pair_label
         FROM dup_groups g {where_clause}
         ORDER BY g.reclaimable DESC, g.id
         LIMIT ?5 OFFSET ?6"
    ))?;
    let mut groups: Vec<GroupView> = stmt
        .query_map(
            params![
                filter.pair,
                filter.min_size,
                folder_low,
                folder_high,
                filter.limit,
                filter.offset
            ],
            |row| {
                let hash: Vec<u8> = row.get(2)?;
                Ok(GroupView {
                    id: row.get(0)?,
                    size: row.get(1)?,
                    hash: hex(&hash),
                    inode_count: row.get(3)?,
                    path_count: row.get(4)?,
                    reclaimable: row.get(5)?,
                    pair_label: row.get(6)?,
                    keeper_ino: None,
                    owners_differ: false,
                    members: Vec::new(),
                })
            },
        )?
        .collect::<rusqlite::Result<_>>()?;

    let mut members_stmt = conn.prepare(
        "SELECT ino, nlink, uid, gid, mtime_ns, dev FROM dup_group_members
         WHERE group_id = ?1 ORDER BY ino",
    )?;
    let mut paths_stmt = conn.prepare(
        "SELECT path FROM files WHERE dev = ?1 AND ino = ?2 AND gone_at IS NULL ORDER BY path",
    )?;
    for group in &mut groups {
        let rows: Vec<(i64, i64, i64, i64, i64, i64)> = members_stmt
            .query_map(params![group.id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        for (ino, nlink, uid, gid, mtime_ns, dev) in rows {
            let paths: Vec<String> = paths_stmt
                .query_map(params![dev, ino], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            group.members.push(MemberView {
                ino,
                nlink,
                uid,
                gid,
                mtime: mtime_ns.div_euclid(1_000_000_000),
                keep: false,
                paths,
            });
        }
        group.keeper_ino = choose_keeper(&group.members, preferred_owner);
        for member in &mut group.members {
            member.keep = Some(member.ino) == group.keeper_ino;
        }
        group.owners_differ = group
            .members
            .windows(2)
            .any(|w| (w[0].uid, w[0].gid) != (w[1].uid, w[1].gid));
        // Keeper first, then the others.
        group.members.sort_by_key(|m| (!m.keep, m.ino));
    }

    Ok(GroupsPage {
        summary,
        keeper_rule: KEEPER_RULE,
        total,
        total_reclaimable,
        groups,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(ino: i64, nlink: i64, owner: (i64, i64), mtime: i64) -> MemberView {
        MemberView {
            ino,
            nlink,
            uid: owner.0,
            gid: owner.1,
            mtime,
            keep: false,
            paths: vec![],
        }
    }

    #[test]
    fn keeper_prefers_links_then_owner_then_age() {
        let pref = Some((100000, 100000));
        let most_links = [member(1, 1, (100000, 100000), 1), member(2, 2, (0, 0), 9)];
        assert_eq!(choose_keeper(&most_links, pref), Some(2));
        let owner = [member(1, 1, (0, 0), 1), member(2, 1, (100000, 100000), 9)];
        assert_eq!(choose_keeper(&owner, pref), Some(2));
        let oldest = [member(1, 1, (0, 0), 9), member(2, 1, (0, 0), 3)];
        assert_eq!(choose_keeper(&oldest, pref), Some(2));
        assert_eq!(choose_keeper(&owner, None), Some(1));
    }

    #[test]
    fn folder_labels() {
        let root = Path::new("/mnt/data");
        assert_eq!(
            folder_label(Path::new("/mnt/data/Films/a.mkv"), root, false),
            "Films"
        );
        assert_eq!(
            folder_label(Path::new("/mnt/data/a.mkv"), root, false),
            "data"
        );
        assert_eq!(
            folder_label(
                Path::new("/mnt/data/Torrents/x/a.mkv"),
                Path::new("/mnt/data/Torrents"),
                true
            ),
            "Torrents"
        );
        let set: BTreeSet<String> = ["Tv Show".into(), "Torrents".into()].into();
        assert_eq!(pair_label(&set), "Torrents ↔ Tv Show");
    }
}
