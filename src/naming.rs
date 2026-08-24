use once_cell::sync::Lazy;
use regex::Regex;
use std::path::{Path, PathBuf};

static INVALID_CHARS: Lazy<Regex> = Lazy::new(|| Regex::new(r#"[<>:"/\\|?*\x00-\x1f]"#).unwrap());
static WHITESPACE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

/// Maximum bytes in a single path component. ext4, APFS, NTFS and SMB all cap
/// at 255; exceeding it fails the import with a bare `ENAMETOOLONG`, and
/// provider-supplied titles are not length-bounded.
const MAX_COMPONENT_BYTES: usize = 255;

/// Truncate to at most `max_bytes`, never splitting a UTF-8 character.
fn truncate_bytes(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub fn sanitize_component(value: &str) -> String {
    let cleaned = INVALID_CHARS.replace_all(value, " ");
    let cleaned = WHITESPACE_RE.replace_all(&cleaned, " ");
    let cleaned = cleaned.trim();
    let cleaned = cleaned.trim_end_matches([' ', '.']).trim();
    // Truncating can re-expose a trailing space or dot, so re-trim after.
    let cleaned = truncate_bytes(cleaned, MAX_COMPONENT_BYTES)
        .trim_end_matches([' ', '.'])
        .trim();
    if cleaned.is_empty() {
        "Unknown".to_string()
    } else {
        cleaned.to_string()
    }
}

/// Cap a built filename at [`MAX_COMPONENT_BYTES`] while keeping its extension,
/// which is what the destination filesystem and media scanners key on.
fn cap_filename(name: &str, extension: &str) -> String {
    if name.len() <= MAX_COMPONENT_BYTES {
        return name.to_string();
    }
    let stem = name.strip_suffix(extension).unwrap_or(name);
    let budget = MAX_COMPONENT_BYTES.saturating_sub(extension.len());
    let stem = truncate_bytes(stem, budget)
        .trim_end_matches([' ', '.'])
        .trim();
    format!("{stem}{extension}")
}

pub fn show_folder_name(title: &str, year: Option<i64>) -> String {
    let safe_title = sanitize_component(title);
    match year {
        Some(year) => {
            // Reserve room for the ` (year)` suffix: it disambiguates two shows
            // with the same name, so it is the part that must survive.
            let suffix = format!(" ({year})");
            let budget = MAX_COMPONENT_BYTES.saturating_sub(suffix.len());
            let stem = truncate_bytes(&safe_title, budget)
                .trim_end_matches([' ', '.'])
                .trim();
            format!("{stem}{suffix}")
        }
        None => safe_title,
    }
}

fn extension(source_path: &Path) -> String {
    source_path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default()
}

pub fn episode_filename(
    title: &str,
    year: Option<i64>,
    season: i64,
    episode: i64,
    episode_title: &str,
    quality: &str,
    extension: &str,
) -> String {
    let show = show_folder_name(title, year);
    let safe_episode_title = sanitize_component(episode_title);
    let safe_quality = sanitize_component(quality);
    let name = format!(
        "{show} - S{season:02}E{episode:02} - {safe_episode_title} - {safe_quality}{extension}"
    );
    cap_filename(&name, extension)
}

#[allow(clippy::too_many_arguments)]
pub fn destination_path(
    output_root: &Path,
    title: &str,
    year: Option<i64>,
    season: i64,
    episode: i64,
    episode_title: &str,
    quality: &str,
    source_path: &Path,
) -> PathBuf {
    let show = show_folder_name(title, year);
    let season_dir = format!("Season {season:02}");
    let filename = episode_filename(
        title,
        year,
        season,
        episode,
        episode_title,
        quality,
        &extension(source_path),
    );
    output_root.join(show).join(season_dir).join(filename)
}

pub fn film_destination_path(
    output_root: &Path,
    title: &str,
    year: Option<i64>,
    quality: &str,
    source_path: &Path,
) -> PathBuf {
    let show = show_folder_name(title, year);
    let safe_quality = sanitize_component(quality);
    let ext = extension(source_path);
    let filename = cap_filename(&format!("{show} - {safe_quality}{ext}"), &ext);
    output_root.join(filename)
}

/// Build the destination for an audio track:
/// `<output_root>/<Artist>/<Album> (<Year>)/<original filename>`. The album
/// folder drops the ` (Year)` suffix when no year is known. The track keeps its
/// ORIGINAL filename unchanged (only the directory components are sanitized).
pub fn music_destination_path(
    output_root: &Path,
    artist: &str,
    album: &str,
    year: Option<i64>,
    source_path: &Path,
) -> PathBuf {
    let artist_dir = sanitize_component(artist);
    let album_dir = show_folder_name(album, year);
    let filename = source_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown".to_string());
    output_root.join(artist_dir).join(album_dir).join(filename)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn builds_tv_destination() {
        let path = destination_path(
            &PathBuf::from("/out/TV"),
            "Fringe",
            Some(2008),
            1,
            1,
            "Pilot",
            "1080p",
            &PathBuf::from("a.mkv"),
        );
        assert_eq!(
            path,
            PathBuf::from(
                "/out/TV/Fringe (2008)/Season 01/Fringe (2008) - S01E01 - Pilot - 1080p.mkv"
            )
        );
    }

    #[test]
    fn builds_film_destination() {
        let path = film_destination_path(
            &PathBuf::from("/out/Films"),
            "Blade Runner 2049",
            Some(2017),
            "2160p",
            &PathBuf::from("br.mkv"),
        );
        assert_eq!(
            path,
            PathBuf::from("/out/Films/Blade Runner 2049 (2017) - 2160p.mkv")
        );
    }

    #[test]
    fn builds_music_destination_with_year() {
        let path = music_destination_path(
            &PathBuf::from("/out/Music"),
            "Daft Punk",
            "Discovery",
            Some(2001),
            &PathBuf::from("/in/Daft Punk/Discovery/01 - One More Time.flac"),
        );
        assert_eq!(
            path,
            PathBuf::from("/out/Music/Daft Punk/Discovery (2001)/01 - One More Time.flac")
        );
    }

    #[test]
    fn builds_music_destination_without_year() {
        let path = music_destination_path(
            &PathBuf::from("/out/Music"),
            "Unknown Artist",
            "Singles",
            None,
            &PathBuf::from("track.mp3"),
        );
        assert_eq!(
            path,
            PathBuf::from("/out/Music/Unknown Artist/Singles/track.mp3")
        );
    }

    #[test]
    fn sanitizes_invalid_chars() {
        assert_eq!(sanitize_component("a/b:c?"), "a b c");
        assert_eq!(sanitize_component("   "), "Unknown");
    }

    #[test]
    fn caps_filenames_at_the_filesystem_limit() {
        let long_title = "A".repeat(400);
        let name = episode_filename("Show", Some(2020), 1, 1, &long_title, "1080p", ".mkv");
        assert!(
            name.len() <= 255,
            "filename was {} bytes, ext4/APFS/NTFS cap at 255",
            name.len()
        );
        assert!(name.ends_with(".mkv"), "extension must survive truncation");
    }

    #[test]
    fn caps_folder_components_at_the_filesystem_limit() {
        let folder = show_folder_name(&"B".repeat(400), Some(1999));
        assert!(folder.len() <= 255, "folder was {} bytes", folder.len());
    }

    #[test]
    fn truncation_never_splits_a_utf8_character() {
        // 4-byte characters straddling the 255-byte boundary.
        let name = episode_filename("Show", None, 1, 1, &"\u{1f600}".repeat(200), "", ".mkv");
        assert!(name.len() <= 255);
        assert!(name.is_char_boundary(name.len()));
    }

    #[test]
    fn caps_film_filenames_too() {
        let path = film_destination_path(
            &PathBuf::from("/out/Films"),
            &"C".repeat(400),
            Some(2017),
            "2160p",
            &PathBuf::from("br.mkv"),
        );
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.len() <= 255, "film filename was {} bytes", name.len());
        assert!(name.ends_with(".mkv"));
    }
}
