use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Wall-clock budget for one ffprobe invocation. A probe against a stalled
/// network mount or a malformed file otherwise never returns, and each one
/// holds a `spawn_blocking` thread for the lifetime of the process.
const FFPROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Run ffprobe with the given arguments, returning its stdout.
///
/// Returns `None` when ffprobe is missing, exits non-zero, or exceeds
/// [`FFPROBE_TIMEOUT`] — in which case the child is killed and reaped so it
/// does not linger. `path` is passed as a separate argument (never through a
/// shell), and `-i` separates it from the option list so a filename that starts
/// with `-` is not read as a flag.
fn run_ffprobe(args: &[&str], path: &Path) -> Option<Vec<u8>> {
    let mut child = Command::new("ffprobe")
        .args(args)
        .arg("-i")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + FFPROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::warn!(
                        "ffprobe timed out after {FFPROBE_TIMEOUT:?}: {}",
                        path.display()
                    );
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    };

    if !status.success() {
        return None;
    }

    // Output here is a few bytes to a few KB, well under the pipe buffer, so
    // reading after the child exits cannot deadlock.
    let mut stdout = Vec::new();
    child.stdout.as_mut()?.read_to_end(&mut stdout).ok()?;
    Some(stdout)
}

/// Audio metadata read from a file's container/format tags via ffprobe.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MusicTags {
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<i64>,
}

/// Probe a video file's vertical resolution with `ffprobe` and map it to a
/// quality label. Returns `None` when ffprobe is unavailable or the height
/// cannot be determined. This is the V1 "ffprobe resolution fallback" gap.
pub fn probe_quality(path: &Path) -> Option<String> {
    let stdout = run_ffprobe(
        &[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=height",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ],
        path,
    )?;
    let text = String::from_utf8_lossy(&stdout);
    let height: i64 = text.lines().next()?.trim().parse().ok()?;
    Some(quality_from_height(height))
}

pub fn quality_from_height(height: i64) -> String {
    // Map by nearest standard tier using inclusive lower bounds.
    if height >= 1800 {
        "2160p".to_string()
    } else if height >= 900 {
        "1080p".to_string()
    } else if height >= 600 {
        "720p".to_string()
    } else if height >= 380 {
        "480p".to_string()
    } else {
        "Unknown".to_string()
    }
}

/// Probe an audio file's `format` tags with `ffprobe` and extract artist,
/// album, and year. Returns all-`None` fields when ffprobe is unavailable or
/// the tags are absent. Never panics: every failure degrades to `None`.
pub fn probe_music_tags(path: &Path) -> MusicTags {
    let stdout = run_ffprobe(
        &[
            "-v",
            "error",
            "-show_entries",
            // Tag keys are case-insensitive in practice; request both common
            // casings plus album_artist so we can fall back on it.
            "format_tags=artist,album,date,year,album_artist,ARTIST,ALBUM,DATE,YEAR,ALBUM_ARTIST",
            "-of",
            "json",
        ],
        path,
    );
    match stdout {
        Some(stdout) => parse_music_tags_json(&String::from_utf8_lossy(&stdout)),
        None => MusicTags::default(),
    }
}

/// Parse the ffprobe `-of json` output for `format_tags`. Split out so it can be
/// unit-tested without invoking ffprobe.
fn parse_music_tags_json(text: &str) -> MusicTags {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(_) => return MusicTags::default(),
    };
    let tags = match value.get("format").and_then(|f| f.get("tags")) {
        Some(tags) => tags,
        None => return MusicTags::default(),
    };

    // Case-insensitive lookup over the tag object: returns the first non-empty
    // string value whose key matches any of `keys` (compared lowercased).
    let lookup = |keys: &[&str]| -> Option<String> {
        let object = tags.as_object()?;
        for (key, value) in object {
            let lower = key.to_lowercase();
            if keys.contains(&lower.as_str()) {
                if let Some(text) = value.as_str() {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
        }
        None
    };

    let artist = lookup(&["artist"]).or_else(|| lookup(&["album_artist"]));
    let album = lookup(&["album"]);
    let year = lookup(&["date", "year"]).and_then(|d| first_year(&d));
    MusicTags {
        artist,
        album,
        year,
    }
}

/// Extract the first 4-digit run from a date/year tag (e.g. "2001-05-04" -> 2001).
fn first_year(value: &str) -> Option<i64> {
    let mut digits = String::new();
    for ch in value.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            if digits.len() == 4 {
                return digits.parse().ok();
            }
        } else {
            digits.clear();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_heights() {
        assert_eq!(quality_from_height(2160), "2160p");
        assert_eq!(quality_from_height(1080), "1080p");
        assert_eq!(quality_from_height(720), "720p");
        assert_eq!(quality_from_height(480), "480p");
        assert_eq!(quality_from_height(120), "Unknown");
    }

    #[test]
    fn parses_music_tags_case_insensitive() {
        let json = r#"{
            "format": {
                "tags": {
                    "ARTIST": "Daft Punk",
                    "Album": "Discovery",
                    "date": "2001-03-12"
                }
            }
        }"#;
        let tags = parse_music_tags_json(json);
        assert_eq!(tags.artist.as_deref(), Some("Daft Punk"));
        assert_eq!(tags.album.as_deref(), Some("Discovery"));
        assert_eq!(tags.year, Some(2001));
    }

    #[test]
    fn parses_music_tags_album_artist_fallback() {
        let json = r#"{"format":{"tags":{"album_artist":"Various","album":"Mix","year":"1999"}}}"#;
        let tags = parse_music_tags_json(json);
        assert_eq!(tags.artist.as_deref(), Some("Various"));
        assert_eq!(tags.year, Some(1999));
    }

    #[test]
    fn music_tags_empty_when_no_tags() {
        assert_eq!(parse_music_tags_json("{}"), MusicTags::default());
        assert_eq!(parse_music_tags_json("not json"), MusicTags::default());
    }

    #[test]
    fn first_year_from_date() {
        assert_eq!(first_year("2001-05-04"), Some(2001));
        assert_eq!(first_year("May 2010"), Some(2010));
        assert_eq!(first_year("no year"), None);
    }
}
