use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::db::{Database, DiscoveredFile};
use crate::filesystem::{
    canonical_or_normalized, collect_canonical_files, extension_in, size_and_mtime,
    AUDIO_EXTENSIONS, VIDEO_EXTENSIONS,
};

/// Discover existing media files under each configured output root and
/// reconcile the library table, marking removed files as missing.
///
/// Cost is kept to one `readdir` per directory plus one `stat` per media file
/// (the size and mtime the table records), all taken before the database lock.
/// The previous walk stat'd every file twice, canonicalized each one (a
/// `readlink` per path component) and stat'd it a third time under the lock,
/// then stat'd every library row again to find the missing ones. On a library
/// sitting on a network mount every one of those calls is a round trip.
pub fn rescan_outputs(db: &Database, roots: &HashMap<String, PathBuf>) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    // Every file seen on disk this pass. A library row whose path is in here is
    // present by construction, so only the rest need checking for absence.
    let mut seen: HashSet<String> = HashSet::new();
    for (media_type, root) in roots {
        counts.insert(media_type.clone(), 0usize);
        if root.as_os_str().is_empty() || !root.exists() {
            continue;
        }
        // Music output roots hold audio files; everything else is video.
        let extensions = if media_type == "music" {
            AUDIO_EXTENSIONS
        } else {
            VIDEO_EXTENSIONS
        };
        let mut files = Vec::new();
        collect_canonical_files(&canonical_or_normalized(root), &mut files);
        let discovered: Vec<DiscoveredFile> = files
            .into_iter()
            .filter(|path| extension_in(path, extensions))
            .filter_map(|path| {
                let (size, mtime) = size_and_mtime(&path)?;
                Some(DiscoveredFile { path, size, mtime })
            })
            .collect();
        seen.extend(
            discovered
                .iter()
                .map(|f| f.path.to_string_lossy().to_string()),
        );
        counts.insert(media_type.clone(), discovered.len());
        db.upsert_discovered_files(media_type, &discovered);
    }
    db.mark_missing_outside(roots, &seen);
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};

    #[test]
    fn rescan_discovers_media_and_marks_removed_files_missing() {
        let base = std::env::temp_dir().join(format!("tvsorter-lib-{}", uuid::Uuid::new_v4()));
        let root = base.join("tv");
        fs::create_dir_all(root.join("Show/Season 01")).unwrap();
        File::create(root.join("Show/Season 01/e1.mkv")).unwrap();
        File::create(root.join("Show/Season 01/e2.mkv")).unwrap();
        File::create(root.join("Show/Season 01/notes.txt")).unwrap();
        let db = Database::open(&base.join("test.db")).unwrap();
        let roots = HashMap::from([("tv".to_string(), root.clone())]);

        let counts = rescan_outputs(&db, &roots);
        assert_eq!(counts["tv"], 2, "only video files are counted");
        let files = db.list_library_files();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.present));
        for f in &files {
            // Stored canonical, so they match what the next rescan sees.
            assert_eq!(
                canonical_or_normalized(std::path::Path::new(&f.output_path)).to_string_lossy(),
                f.output_path
            );
        }

        fs::remove_file(root.join("Show/Season 01/e2.mkv")).unwrap();
        rescan_outputs(&db, &roots);
        let files = db.list_library_files();
        let present: Vec<bool> = files.iter().map(|f| f.present).collect();
        assert_eq!(files.len(), 2);
        assert_eq!(present.iter().filter(|p| **p).count(), 1);
        assert!(files
            .iter()
            .any(|f| f.output_path.ends_with("e2.mkv") && !f.present));

        // Coming back flips it present again.
        File::create(root.join("Show/Season 01/e2.mkv")).unwrap();
        rescan_outputs(&db, &roots);
        assert!(db.list_library_files().iter().all(|f| f.present));

        fs::remove_dir_all(&base).ok();
    }

    /// A symlink loop under an output root used to be walked round and round
    /// until the kernel gave up (ELOOP), counting the same file once per lap; the
    /// canonical walk visits each directory once.
    #[test]
    #[cfg(unix)]
    fn rescan_survives_a_symlink_loop() {
        let base = std::env::temp_dir().join(format!("tvsorter-lib-{}", uuid::Uuid::new_v4()));
        let root = base.join("tv");
        fs::create_dir_all(root.join("Show")).unwrap();
        File::create(root.join("Show/e1.mkv")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("Show/loop")).unwrap();
        let db = Database::open(&base.join("test.db")).unwrap();
        let roots = HashMap::from([("tv".to_string(), root.clone())]);

        let counts = rescan_outputs(&db, &roots);
        assert_eq!(counts["tv"], 1);

        fs::remove_dir_all(&base).ok();
    }
}
