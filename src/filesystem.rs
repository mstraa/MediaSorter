use std::fs;
use std::path::{Component, Path, PathBuf};

pub const VIDEO_EXTENSIONS: &[&str] = &[
    "avi", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "ts", "webm", "wmv",
];

pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "m4a", "aac", "ogg", "opus", "wav", "wma", "alac",
];

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("Path is outside the configured root")]
    OutsideRoot,
    #[error("Not a directory: {0}")]
    NotADirectory(String),
    #[error("{0}")]
    Io(String),
}

#[derive(Clone, Debug)]
pub struct BrowserEntry {
    pub name: String,
    pub relative_path: String,
    pub absolute_path: PathBuf,
    pub is_dir: bool,
    pub size: Option<i64>,
    pub is_video: bool,
    /// True when the file has more than one hard link (nlink > 1 on Unix).
    /// On its own this does not mean the file is linked into a library —
    /// seeding tools, *arr imports, and dedup jobs also hardlink sources.
    /// Pair it with [`BrowserEntry::dev_inode`] against recorded imports.
    pub has_extra_links: bool,
    /// `(st_dev, st_ino)` for files on Unix, `None` for directories and on
    /// other platforms. Matches the `source_device`/`source_inode` columns
    /// recorded on imports.
    pub dev_inode: Option<(i64, i64)>,
}

/// Lexically normalize an absolute-ish path, resolving `.` and `..` without
/// touching the filesystem. Used as a fallback when `canonicalize` fails
/// (e.g. the path does not yet exist).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}

pub fn canonical_or_normalized(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| lexical_normalize(path))
}

pub fn is_relative_to(path: &Path, root: &Path) -> bool {
    let path = canonical_or_normalized(path);
    let root = canonical_or_normalized(root);
    path.starts_with(&root)
}

pub fn resolve_under_root(root: &Path, relative_path: &str) -> Result<PathBuf, FsError> {
    let root_c = canonical_or_normalized(root);
    let joined = root.join(relative_path);
    let target = canonical_or_normalized(&joined);
    if !target.starts_with(&root_c) {
        return Err(FsError::OutsideRoot);
    }
    Ok(target)
}

#[cfg(unix)]
fn hard_link_count(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}

#[cfg(not(unix))]
fn hard_link_count(_meta: &fs::Metadata) -> u64 {
    1
}

#[cfg(unix)]
fn dev_inode(meta: &fs::Metadata) -> Option<(i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev() as i64, meta.ino() as i64))
}

#[cfg(not(unix))]
fn dev_inode(_meta: &fs::Metadata) -> Option<(i64, i64)> {
    None
}

/// Modification time as seconds since the Unix epoch, matching how mtimes are
/// stored in the database. `None` when the platform can't report it.
pub fn mtime_secs(meta: &fs::Metadata) -> Option<f64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
}

/// Size in bytes and modification time for a file, or `None` if it can't be
/// stat'd (e.g. it no longer exists).
pub fn size_and_mtime(path: &Path) -> Option<(i64, Option<f64>)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.len() as i64, mtime_secs(&meta)))
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    if !path.is_file() {
        return false;
    }
    match path.extension() {
        Some(ext) => {
            let ext = ext.to_string_lossy().to_lowercase();
            extensions.contains(&ext.as_str())
        }
        None => false,
    }
}

pub fn is_video_file(path: &Path) -> bool {
    has_extension(path, VIDEO_EXTENSIONS)
}

pub fn is_audio_file(path: &Path) -> bool {
    has_extension(path, AUDIO_EXTENSIONS)
}

pub fn list_directory(root: &Path, relative_path: &str) -> Result<Vec<BrowserEntry>, FsError> {
    let directory = resolve_under_root(root, relative_path)?;
    if !directory.is_dir() {
        return Err(FsError::NotADirectory(directory.display().to_string()));
    }
    let root_c = canonical_or_normalized(root);
    let read = fs::read_dir(&directory).map_err(|e| FsError::Io(e.to_string()))?;

    // Stat each child exactly once and precompute the sort key. Sorting on
    // `Path::is_dir()` / `name_lower()` instead would re-stat and re-allocate
    // inside the comparator — O(n log n) syscalls over a directory that may sit
    // on a network mount, versus O(n) here.
    let mut children: Vec<(bool, String, PathBuf, fs::Metadata)> = read
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            // `metadata` (not `entry.file_type()`) so symlinks resolve, matching
            // how the entries are classified below.
            let meta = fs::metadata(&path).ok()?;
            Some((meta.is_dir(), name_lower(&path), path, meta))
        })
        .collect();
    // Folders first, then case-insensitive name.
    children.sort_by(|(a_dir, a_name, _, _), (b_dir, b_name, _, _)| {
        (!a_dir).cmp(&!b_dir).then_with(|| a_name.cmp(b_name))
    });

    let mut entries = Vec::with_capacity(children.len());
    for (_, _, child, metadata) in children {
        let child_c = canonical_or_normalized(&child);
        let relative = match child_c.strip_prefix(&root_c) {
            Ok(rel) => rel.to_string_lossy().to_string(),
            Err(_) => continue,
        };
        let is_dir = metadata.is_dir();
        let has_extra_links = !is_dir && hard_link_count(&metadata) > 1;
        entries.push(BrowserEntry {
            name: child
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            relative_path: relative,
            absolute_path: child.clone(),
            is_dir,
            size: if is_dir {
                None
            } else {
                Some(metadata.len() as i64)
            },
            is_video: is_video_file(&child),
            has_extra_links,
            dev_inode: if is_dir { None } else { dev_inode(&metadata) },
        });
    }
    Ok(entries)
}

fn name_lower(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Recursively collect files under `dir` for which `keep` returns true.
pub fn collect_files(dir: &Path, keep: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(_) => return,
    };
    for entry in read.flatten() {
        let path = entry.path();
        if is_dir_entry(&entry, &path) {
            collect_files(&path, keep, out);
        } else if keep(&path) {
            out.push(path);
        }
    }
}

/// Whether a directory entry is (or points at) a directory.
///
/// `readdir` already reports the type on Linux and macOS, so the common case
/// costs nothing; only a symlink needs an explicit stat to resolve. Calling
/// `Path::is_dir()` instead would stat every single entry, and this runs over
/// the whole tree under each Browse row.
fn is_dir_entry(entry: &fs::DirEntry, path: &Path) -> bool {
    match entry.file_type() {
        // Resolve symlinks so a symlinked show folder is still walked, matching
        // the previous `Path::is_dir()` behaviour.
        Ok(file_type) if file_type.is_symlink() => {
            fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
        }
        Ok(file_type) => file_type.is_dir(),
        Err(_) => path.is_dir(),
    }
}

fn expand_files(
    root: &Path,
    relative_paths: &[String],
    video_only: bool,
) -> Result<Vec<PathBuf>, FsError> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let root_c = canonical_or_normalized(root);
    for relative_path in relative_paths {
        let target = resolve_under_root(root, relative_path)?;
        let mut candidates: Vec<PathBuf> = Vec::new();
        if target.is_dir() {
            collect_files(&target, &|p| p.is_file(), &mut candidates);
        } else {
            candidates.push(target);
        }
        for candidate in candidates {
            if candidate.is_file() && (!video_only || is_video_file(&candidate)) {
                let resolved = canonical_or_normalized(&candidate);
                // A symlink inside the root can point outside it. Browse already
                // hides those (their canonical path will not strip_prefix the
                // root) and the import handler rejects them, so expanding them
                // here only produced a confusing whole-batch failure — and
                // following them would otherwise reach outside the input root.
                if !resolved.starts_with(&root_c) {
                    continue;
                }
                if seen.insert(resolved.clone()) {
                    files.push(resolved);
                }
            }
        }
    }
    files.sort();
    Ok(files)
}

pub fn expand_source_files(
    root: &Path,
    relative_paths: &[String],
) -> Result<Vec<PathBuf>, FsError> {
    expand_files(root, relative_paths, false)
}

/// A video file within a group, with the directory segments between the group
/// root and the file (filename excluded). Segments drive season detection.
#[derive(Debug, Clone)]
pub struct GroupedFile {
    pub path: PathBuf,
    pub relative_segments: Vec<String>,
}

/// A set of video files that share one show identity (one selected folder, or
/// the common parent of individually-selected files).
#[derive(Debug, Clone)]
pub struct FileGroup {
    pub group_key: String,
    pub group_name: String,
    pub files: Vec<GroupedFile>,
}

fn leaf_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Directory segments of `file` relative to `group_root`, excluding the filename.
fn dir_segments(group_root: &Path, file: &Path) -> Vec<String> {
    match file.strip_prefix(group_root) {
        Ok(rel) => {
            let mut segments: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect();
            segments.pop(); // drop the filename itself
            segments
        }
        Err(_) => Vec::new(),
    }
}

fn push_into_group(
    groups: &mut std::collections::HashMap<String, FileGroup>,
    order: &mut Vec<String>,
    key: &str,
    name: &str,
    file: GroupedFile,
) {
    let entry = groups.entry(key.to_string()).or_insert_with(|| {
        order.push(key.to_string());
        FileGroup {
            group_key: key.to_string(),
            group_name: name.to_string(),
            files: Vec::new(),
        }
    });
    entry.files.push(file);
}

/// Expand a selection into groups of video files keyed by show-bearing folder.
/// Selected directories become one group each (recursively); individually
/// selected files are grouped under their parent folder. Order of first
/// appearance is preserved; files within a group are sorted by path.
pub fn expand_grouped(root: &Path, relative_paths: &[String]) -> Result<Vec<FileGroup>, FsError> {
    expand_grouped_with(root, relative_paths, &is_video_file)
}

/// Like [`expand_grouped`], but collects audio files (for the "music" media
/// type) instead of video files.
pub fn expand_grouped_audio(
    root: &Path,
    relative_paths: &[String],
) -> Result<Vec<FileGroup>, FsError> {
    expand_grouped_with(root, relative_paths, &is_audio_file)
}

fn expand_grouped_with(
    root: &Path,
    relative_paths: &[String],
    keep: &dyn Fn(&Path) -> bool,
) -> Result<Vec<FileGroup>, FsError> {
    let root_c = canonical_or_normalized(root);
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, FileGroup> = std::collections::HashMap::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for relative_path in relative_paths {
        let target = resolve_under_root(root, relative_path)?;
        if target.is_dir() {
            let group_key = relative_path.trim_end_matches('/').to_string();
            let group_name = leaf_name(&target);
            let mut candidates: Vec<PathBuf> = Vec::new();
            collect_files(&target, &|p| p.is_file(), &mut candidates);
            candidates.sort();
            for candidate in candidates {
                if !keep(&candidate) {
                    continue;
                }
                let resolved = canonical_or_normalized(&candidate);
                if !seen.insert(resolved.clone()) {
                    continue;
                }
                let segments = dir_segments(&target, &resolved);
                push_into_group(
                    &mut groups,
                    &mut order,
                    &group_key,
                    &group_name,
                    GroupedFile {
                        path: resolved,
                        relative_segments: segments,
                    },
                );
            }
        } else if target.is_file() && keep(&target) {
            let resolved = canonical_or_normalized(&target);
            if !seen.insert(resolved.clone()) {
                continue;
            }
            let parent = resolved.parent().unwrap_or(&root_c).to_path_buf();
            let group_name = leaf_name(&parent);
            let group_key = parent
                .strip_prefix(&root_c)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| group_name.clone());
            push_into_group(
                &mut groups,
                &mut order,
                &group_key,
                &group_name,
                GroupedFile {
                    path: resolved,
                    relative_segments: Vec::new(),
                },
            );
        }
    }

    let mut result: Vec<FileGroup> = order
        .into_iter()
        .filter_map(|key| groups.remove(&key))
        .collect();
    for group in &mut result {
        group.files.sort_by(|a, b| a.path.cmp(&b.path));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};

    #[test]
    fn rejects_path_traversal() {
        let dir = std::env::temp_dir().join(format!("tvsorter-fs-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("sub")).unwrap();
        File::create(dir.join("sub/a.mkv")).unwrap();
        assert!(resolve_under_root(&dir, "../etc").is_err());
        assert!(resolve_under_root(&dir, "sub").is_ok());
        let groups = expand_grouped(&dir, &["sub".to_string()]).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].files.len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(unix)]
    fn reports_extra_links_and_inode_identity() {
        let dir = std::env::temp_dir().join(format!("tvsorter-ln-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        File::create(dir.join("plain.mkv")).unwrap();
        File::create(dir.join("linked.mkv")).unwrap();
        fs::hard_link(dir.join("linked.mkv"), dir.join("twin.mkv")).unwrap();

        let entries = list_directory(&dir, "").unwrap();
        let by_name = |name: &str| entries.iter().find(|e| e.name == name).unwrap();
        assert!(!by_name("plain.mkv").has_extra_links);
        assert!(by_name("linked.mkv").has_extra_links);
        assert!(by_name("twin.mkv").has_extra_links);
        // Both links share one inode; the plain file has its own.
        assert!(by_name("linked.mkv").dev_inode.is_some());
        assert_eq!(
            by_name("linked.mkv").dev_inode,
            by_name("twin.mkv").dev_inode
        );
        assert_ne!(
            by_name("plain.mkv").dev_inode,
            by_name("linked.mkv").dev_inode
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn groups_folder_with_season_subfolders() {
        let dir = std::env::temp_dir().join(format!("tvsorter-grp-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("Black Lagoon/Season 1")).unwrap();
        fs::create_dir_all(dir.join("Black Lagoon/Season 2")).unwrap();
        File::create(dir.join("Black Lagoon/Season 1/01 - The Black Lagoon.mkv")).unwrap();
        File::create(dir.join("Black Lagoon/Season 1/02 - Mangrove Heaven.mkv")).unwrap();
        File::create(dir.join("Black Lagoon/Season 2/01 - The Vampire Twins.mkv")).unwrap();

        let groups = expand_grouped(&dir, &["Black Lagoon".to_string()]).unwrap();
        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert_eq!(group.group_name, "Black Lagoon");
        assert_eq!(group.files.len(), 3);
        // Season folder must surface as a directory segment.
        assert!(group.files[0]
            .relative_segments
            .iter()
            .any(|s| s.eq_ignore_ascii_case("Season 1")));
        fs::remove_dir_all(&dir).ok();
    }

    /// A symlink inside an input root can point outside it. Expansion must not
    /// follow it out: Browse already hides such entries and the import handler
    /// rejects them, so following it only produced a confusing whole-batch
    /// failure — and it would otherwise reach outside the configured root.
    #[test]
    #[cfg(unix)]
    fn expansion_does_not_follow_symlinks_out_of_the_root() {
        let base = std::env::temp_dir().join(format!("tvsorter-sym-{}", uuid::Uuid::new_v4()));
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("inside")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        File::create(root.join("inside/a.mkv")).unwrap();
        File::create(outside.join("secret.mkv")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escaping")).unwrap();

        let files = expand_source_files(&root, &["".to_string()]).unwrap();
        assert!(
            files.iter().any(|p| p.ends_with("a.mkv")),
            "files genuinely inside the root must still be found"
        );
        assert!(
            !files.iter().any(|p| p.ends_with("secret.mkv")),
            "expansion escaped the input root through a symlink: {files:?}"
        );

        fs::remove_dir_all(&base).ok();
    }

    /// A symlinked directory that stays inside the root is still walked — that
    /// is a normal way to lay out a media library.
    #[test]
    #[cfg(unix)]
    fn expansion_follows_symlinks_that_stay_inside_the_root() {
        let root = std::env::temp_dir().join(format!("tvsorter-sym-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("real/Season 2")).unwrap();
        File::create(root.join("real/Season 2/ep.mkv")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("linked")).unwrap();

        let files = expand_source_files(&root, &["".to_string()]).unwrap();
        assert!(
            files.iter().any(|p| p.ends_with("ep.mkv")),
            "in-root symlinked folder should still be walked: {files:?}"
        );

        fs::remove_dir_all(&root).ok();
    }
}
