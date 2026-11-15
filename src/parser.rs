use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use std::path::Path;

static QUALITY_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(2160p|1080p|720p|480p)\b").unwrap());
static RELEASE_TRAIL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(2160p|1080p|720p|480p|multi|vff|vfq|vf2|vf|truefrench|french|hdrip|web[ ._-]?dl|webrip|hdtv|bluray|bdrip|brrip|dvdrip|x264|x265|h[ ._-]?264|h[ ._-]?265|hevc|aac|ac3|ddp?5?[ ._-]?1|proper|repack)\b.*$",
    )
    .unwrap()
});
/// Leading `[Group]` / `[Tags]` blocks, as in "[Tsundere-Raws] Show - 01".
static BRACKET_BLOCK_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[[^\]]*\]").unwrap());
/// First scene/release tag in a separator-normalised folder name. Everything
/// from it onward (season marker, language, source, codecs, group, platform)
/// is release noise, never part of the show title. Matched on lowercased text
/// whose `._-` separators are already spaces, so "H.264" reads "h 264" and
/// "E-AC-3" reads "e ac 3".
static RELEASE_TAG_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:^|\s)(?:s\d{1,2}(?:\s?e\d{1,3})?|(?:season|saison)\s\d{1,2}|(?:19|20)\d{2}|2160p|1080p|720p|576p|480p|4k|uhd|multi|vostfr|vost|subfrench|truefrench|french|vff|vfq|vfi|vf2|vf|web(?:\s?dl|\s?rip)?|hdtv|blu\s?ray|bd\s?rip|br\s?rip|dvd\s?rip|bd|remux|[xh]\s?26[45]|hevc|avc|av1|aac|e?\s?ac\s?3|ddp?\d?|dts|flac|opus|10\s?bits?|8\s?bits?|hdr(?:10)?|int[ée]grale|complete|proper|repack|dsnp|amzn|nf|cr|adn|dual\s?audio)(?:\s|$)",
    )
    .unwrap()
});
static YEAR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:^|[\s._(-])((?:19|20)\d{2})(?:$|[\s._)-])").unwrap());
static YEAR_TOKEN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?:19|20)\d{2}").unwrap());
// `(?:v\d{1,2})?` tolerates anime version suffixes glued to the episode number
// (e.g. "05v2"), so the right episode is detected instead of falling through.
static SXXEYY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\bS(?P<season>\d{1,2})E(?P<episode>\d{1,3})(?:v\d{1,2})?\b").unwrap()
});
static ONE_X_TWO_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?P<season>\d{1,2})x(?P<episode>\d{1,3})(?:v\d{1,2})?\b").unwrap()
});
static EYY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:^|[\s._-])E(?P<episode>\d{1,3})(?:v\d{1,2})?(?:$|[\s._-])").unwrap()
});
static SEASON_EP_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\bseason[\s._-]*(?P<season>\d{1,2})[\s._-]*episode[\s._-]*(?P<episode>\d{1,3})\b",
    )
    .unwrap()
});
static BRACKETS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\[\](){}]").unwrap());
static SEPARATORS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[._-]+").unwrap());
static WHITESPACE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

// Season detection from folder names: a whole segment like "Season 1", "Saison 02",
// "Series 3", "S01" — or an inline "season 2" inside a longer segment.
static SEASON_SEGMENT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^(?:season|saison|series|s)[\s._-]*0*(\d{1,2})$").unwrap());
static SEASON_INLINE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(?:season|saison|series)[\s._-]*0*(\d{1,2})\b").unwrap());

// Permissive episode markers for files that only carry a number, e.g.
// "12 - Title", "Episode 05", "Ep 7", "OVA 3", or a trailing "- 07".
static EP_KEYWORD_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:episode|épisode|ep|e|ova|oav|oad|#)[\s._-]*0*(?P<episode>\d{1,3})(?P<ver>v\d{1,2})?\b")
        .unwrap()
});
static LEADING_NUM_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[\s._-]*0*(?P<episode>\d{1,3})(?P<ver>v\d{1,2})?(?:$|[\s._-])").unwrap()
});
// A standalone number, optionally with a version suffix ("05v2") so the "v"
// doesn't block the match.
static STANDALONE_NUM_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:^|[\s._-])0*(?P<episode>\d{1,3})(?P<ver>v\d{1,2})?(?:$|[\s._-])").unwrap()
});

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ParsedMedia {
    pub source_name: String,
    pub title: String,
    pub year: Option<i64>,
    pub season: i64,
    pub episode: i64,
    pub episode_title: String,
    pub quality: String,
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default()
}

struct EpisodeMatch {
    season: i64,
    episode: i64,
    start: Option<usize>,
    end: Option<usize>,
}

fn find_episode(stem: &str) -> EpisodeMatch {
    for re in [&*SXXEYY_RE, &*ONE_X_TWO_RE, &*SEASON_EP_RE] {
        if let Some(caps) = re.captures(stem) {
            let m = caps.get(0).unwrap();
            return EpisodeMatch {
                season: caps["season"].parse().unwrap_or(1),
                episode: caps["episode"].parse().unwrap_or(1),
                start: Some(m.start()),
                end: Some(m.end()),
            };
        }
    }
    if let Some(caps) = EYY_RE.captures(stem) {
        let m = caps.get(0).unwrap();
        return EpisodeMatch {
            season: 1,
            episode: caps["episode"].parse().unwrap_or(1),
            start: Some(m.start()),
            end: Some(m.end()),
        };
    }
    EpisodeMatch {
        season: 1,
        episode: 1,
        start: None,
        end: None,
    }
}

/// Like `find_episode`, but for files that only carry an episode number with no
/// season marker (common when the show/season lives in the folder structure).
/// Falls back through keyword markers, a leading number, then any standalone
/// 1-3 digit number that is not a 4-digit year.
fn find_episode_loose(stem: &str) -> EpisodeMatch {
    let structured = find_episode(stem);
    if structured.start.is_some() {
        return structured;
    }
    for re in [&*EP_KEYWORD_RE, &*LEADING_NUM_RE, &*STANDALONE_NUM_RE] {
        if let Some(caps) = re.captures(stem) {
            let group = caps.name("episode").unwrap();
            // Skip the version suffix ("v2") so it doesn't leak into the title.
            let end = caps
                .name("ver")
                .map(|m| m.end())
                .unwrap_or_else(|| group.end());
            return EpisodeMatch {
                season: 1,
                episode: group.as_str().parse().unwrap_or(1),
                start: Some(group.start()),
                end: Some(end),
            };
        }
    }
    structured
}

/// Detect a season number from the directory segments leading up to a file
/// (deepest folder first). Returns `None` when no segment looks like a season.
pub fn season_from_segments(segments: &[&str]) -> Option<i64> {
    for segment in segments.iter().rev() {
        if let Some(caps) = SEASON_SEGMENT_RE.captures(segment) {
            return caps[1].parse().ok();
        }
        if let Some(caps) = SEASON_INLINE_RE.captures(segment) {
            return caps[1].parse().ok();
        }
    }
    None
}

/// Build a `ParsedMedia` for an episode whose show identity comes from the
/// enclosing folder (`show_title`) rather than the filename. `season_hint` is the
/// season derived from the folder structure, when available; the filename can
/// still override it (e.g. an explicit S02E03).
pub fn parse_folder_episode(
    path: &Path,
    show_title: &str,
    season_hint: Option<i64>,
) -> ParsedMedia {
    let source_name = file_name(path);
    let stem = file_stem(path);
    let episode = find_episode_loose(&stem);
    let quality = detect_quality(&stem);

    // A season explicitly present in the filename wins; otherwise use the folder
    // hint; otherwise default to 1.
    let filename_season = if find_episode(&stem).start.is_some() {
        Some(episode.season)
    } else {
        None
    };
    let season = filename_season.or(season_hint).unwrap_or(1);

    let episode_title_source = match (episode.end, episode.start) {
        (Some(end), _) => &stem[end..],
        (None, _) => stem.as_str(),
    };
    let episode_title = clean_episode_title(episode_title_source);

    ParsedMedia {
        source_name,
        title: if show_title.is_empty() {
            "Unknown Show".to_string()
        } else {
            show_title.to_string()
        },
        year: None,
        season,
        episode: episode.episode,
        episode_title: if episode_title.is_empty() {
            "Episode".to_string()
        } else {
            episode_title
        },
        quality,
    }
}

pub fn detect_quality(value: &str) -> String {
    QUALITY_RE
        .captures(value)
        .map(|caps| caps[1].to_lowercase())
        .unwrap_or_else(|| "Unknown".to_string())
}

fn find_year(stem: &str) -> Option<i64> {
    YEAR_RE
        .captures(stem)
        .and_then(|caps| caps.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

fn extract_film_year(stem: &str) -> (Option<i64>, String) {
    let matches: Vec<_> = YEAR_TOKEN_RE.find_iter(stem).collect();
    match matches.last() {
        None => (None, stem.to_string()),
        Some(m) => {
            let title_stem = format!("{} {}", &stem[..m.start()], &stem[m.end()..]);
            (m.as_str().parse().ok(), title_stem)
        }
    }
}

fn clean_tokens(value: &str) -> String {
    let value = BRACKETS_RE.replace_all(value, " ");
    let value = SEPARATORS_RE.replace_all(&value, " ");
    let value = WHITESPACE_RE.replace_all(&value, " ");
    title_case(value.trim())
}

/// Mimics Python's str.title(): uppercase the first letter of each run of
/// alphabetic characters, lowercase the rest.
fn title_case(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut prev_alpha = false;
    for ch in value.chars() {
        if ch.is_alphabetic() {
            if prev_alpha {
                result.extend(ch.to_lowercase());
            } else {
                result.extend(ch.to_uppercase());
            }
            prev_alpha = true;
        } else {
            result.push(ch);
            prev_alpha = false;
        }
    }
    result
}

fn clean_title(value: &str) -> String {
    let value = YEAR_RE.replace_all(value, " ");
    clean_tokens(&value)
}

fn clean_film_title(value: &str) -> String {
    let value = RELEASE_TRAIL_RE.replace_all(value, " ");
    clean_tokens(&value)
}

fn clean_episode_title(value: &str) -> String {
    let value = RELEASE_TRAIL_RE.replace_all(value, " ");
    clean_tokens(&value)
}

/// Derive a clean show title and optional year from a folder name, e.g.
/// "Black Lagoon (2006)" -> ("Black Lagoon", Some(2006)).
pub fn show_title_from_folder(name: &str) -> (String, Option<i64>) {
    let year = find_year(name);
    let title = clean_title(name);
    let title = if title.is_empty() {
        name.trim().to_string()
    } else {
        title
    };
    (title, year)
}

/// Like [`show_title_from_folder`], but for release-named show/anime folders:
/// "Code.Geass.Rozé.of.the.Recapture.S01.MULTi.1080p.WEB.H.264.E-AC-3-Tsundere-Raws (DSNP)"
/// -> ("Code Geass Rozé Of The Recapture", None). This is the string sent to
/// the metadata providers, which return nothing for the raw release name.
pub fn show_title_from_release(name: &str) -> (String, Option<i64>) {
    let year = find_year(name);
    let without_groups = BRACKET_BLOCK_RE.replace_all(name, " ");
    let spaced = BRACKETS_RE.replace_all(&without_groups, " ");
    let spaced = SEPARATORS_RE.replace_all(&spaced, " ");
    let spaced = WHITESPACE_RE.replace_all(&spaced, " ");
    let spaced = spaced.trim();
    // Cut at the first release tag, but never at the very start: a title can
    // legitimately begin with a year ("1917") or a short word.
    let head = RELEASE_TAG_RE
        .find_iter(spaced)
        .find(|m| m.start() > 0)
        .map(|m| &spaced[..m.start()])
        .unwrap_or(spaced);
    let title = clean_title(head);
    let title = if title.is_empty() {
        name.trim().to_string()
    } else {
        title
    };
    (title, year)
}

/// Artist/album/year/title derived for an audio track. Used as the fallback
/// when ffprobe tags are missing (see `parse_music_file`).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ParsedMusic {
    pub source_name: String,
    pub artist: String,
    pub album: String,
    pub year: Option<i64>,
    pub title: String,
}

/// Derive artist/album/year/title for an audio track from its FILENAME, falling
/// back to the enclosing FOLDER structure. The folder convention recognised is
/// `<Artist>/<Album>` (album is the deepest folder, artist its parent) or a lone
/// `<Album>` folder. ffprobe tags, when present, take precedence over this
/// parser at the call site (the match route).
pub fn parse_music_file(
    path: &Path,
    folder_artist: Option<&str>,
    folder_album: Option<&str>,
) -> ParsedMusic {
    let source_name = file_name(path);
    let stem = file_stem(path);

    // A leading track number ("01 - Title", "01. Title", "1 Title") is noise for
    // the track title; strip it so the cleaned title reads naturally.
    let title = clean_tokens(&strip_leading_track_number(&stem));
    let title = if title.is_empty() {
        stem.clone()
    } else {
        title
    };

    // The album folder may carry a trailing year, e.g. "Discovery (2001)".
    let (album_from_folder, year_from_folder) = match folder_album {
        Some(name) => {
            let (title, year) = show_title_from_folder(name);
            (Some(title), year)
        }
        None => (None, None),
    };

    let artist = folder_artist
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| "Unknown Artist".to_string());
    let album = album_from_folder
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| "Unknown Album".to_string());

    ParsedMusic {
        source_name,
        artist,
        album,
        year: year_from_folder,
        title,
    }
}

/// Remove a leading track-number prefix such as "01 - ", "01. ", or "1 " from a
/// track filename stem.
fn strip_leading_track_number(stem: &str) -> String {
    static TRACK_NUM_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^\s*\d{1,3}\s*[-._)]*\s+").unwrap());
    TRACK_NUM_RE.replace(stem, "").to_string()
}

pub fn parse_film_filename(path: &Path) -> ParsedMedia {
    let source_name = file_name(path);
    let stem = file_stem(path);
    let quality = detect_quality(&stem);
    let (year, title_stem) = extract_film_year(&stem);
    let title = clean_film_title(&title_stem);
    ParsedMedia {
        source_name,
        title: if title.is_empty() {
            "Unknown Film".to_string()
        } else {
            title
        },
        year,
        season: 0,
        episode: 0,
        episode_title: "Film".to_string(),
        quality,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn folder_episode_reads_embedded_sxxeyy_and_title() {
        let parsed = parse_folder_episode(
            &PathBuf::from("Fringe.S01E01.Pilot.1080p.mkv"),
            "Fringe",
            None,
        );
        assert_eq!(parsed.title, "Fringe");
        assert_eq!(parsed.season, 1);
        assert_eq!(parsed.episode, 1);
        assert_eq!(parsed.episode_title, "Pilot");
        assert_eq!(parsed.quality, "1080p");
    }

    #[test]
    fn folder_episode_version_suffix() {
        let parsed = parse_folder_episode(
            &PathBuf::from("[I-R]Yakitate_Japan_05v2_xvid_vostfr.avi"),
            "Yakitate Japan",
            None,
        );
        assert_eq!(parsed.episode, 5);
        assert_eq!(parsed.title, "Yakitate Japan");
    }

    #[test]
    fn folder_episode_sxxeyy_version_suffix() {
        let parsed = parse_folder_episode(&PathBuf::from("Show.S02E07v2.mkv"), "Show", None);
        assert_eq!(parsed.season, 2);
        assert_eq!(parsed.episode, 7);
    }

    #[test]
    fn folder_episode_e_only() {
        let parsed = parse_folder_episode(
            &PathBuf::from("[Group] Cowboy Bebop - E05 [720p].mkv"),
            "Cowboy Bebop",
            None,
        );
        assert_eq!(parsed.season, 1);
        assert_eq!(parsed.episode, 5);
        assert_eq!(parsed.quality, "720p");
    }

    #[test]
    fn parses_film_year() {
        let parsed = parse_film_filename(&PathBuf::from("Blade Runner 2049 2017 2160p.mkv"));
        assert_eq!(parsed.title, "Blade Runner 2049");
        assert_eq!(parsed.year, Some(2017));
        assert_eq!(parsed.quality, "2160p");
    }

    #[test]
    fn season_from_folder_segments() {
        assert_eq!(season_from_segments(&["Black Lagoon", "Season 2"]), Some(2));
        assert_eq!(
            season_from_segments(&["Black Lagoon", "Saison 03"]),
            Some(3)
        );
        assert_eq!(season_from_segments(&["Show", "S01"]), Some(1));
        assert_eq!(season_from_segments(&["Show", "Extras"]), None);
    }

    #[test]
    fn folder_episode_uses_folder_title_and_season() {
        let parsed = parse_folder_episode(
            &PathBuf::from("12 - The Black Lagoon.mkv"),
            "Black Lagoon",
            Some(2),
        );
        assert_eq!(parsed.title, "Black Lagoon");
        assert_eq!(parsed.season, 2);
        assert_eq!(parsed.episode, 12);
        assert_eq!(parsed.episode_title, "The Black Lagoon");
    }

    #[test]
    fn folder_episode_filename_season_overrides_hint() {
        let parsed =
            parse_folder_episode(&PathBuf::from("S03E04 - Title.mkv"), "Some Show", Some(1));
        assert_eq!(parsed.season, 3);
        assert_eq!(parsed.episode, 4);
    }

    #[test]
    fn folder_title_strips_year() {
        let (title, year) = show_title_from_folder("Black Lagoon (2006)");
        assert_eq!(title, "Black Lagoon");
        assert_eq!(year, Some(2006));
    }

    #[test]
    fn release_title_strips_scene_tags() {
        let cases = [
            (
                "Code.Geass.Rozé.of.the.Recapture.S01.MULTi.1080p.WEB.H.264.E-AC-3-Tsundere-Raws (DSNP)",
                "Code Geass Rozé Of The Recapture",
                None,
            ),
            (
                "[Tsundere-Raws] Frieren - INTEGRALE (BluRay 1080p 10bits AV1)",
                "Frieren",
                None,
            ),
            ("Black Lagoon (2006)", "Black Lagoon", Some(2006)),
            ("Spy x Family 2022 S02 VOSTFR 1080p", "Spy X Family", Some(2022)),
            ("Vinland Saga Saison 2 MULTi BluRay x265", "Vinland Saga", None),
            ("Mushishi", "Mushishi", None),
            ("1917", "1917", Some(1917)),
        ];
        for (input, title, year) in cases {
            assert_eq!(
                show_title_from_release(input),
                (title.to_string(), year),
                "{input}"
            );
        }
    }

    #[test]
    fn parse_music_uses_folder_artist_album_and_year() {
        let parsed = parse_music_file(
            &PathBuf::from("01 - One More Time.flac"),
            Some("Daft Punk"),
            Some("Discovery (2001)"),
        );
        assert_eq!(parsed.artist, "Daft Punk");
        assert_eq!(parsed.album, "Discovery");
        assert_eq!(parsed.year, Some(2001));
        assert_eq!(parsed.title, "One More Time");
        assert_eq!(parsed.source_name, "01 - One More Time.flac");
    }

    #[test]
    fn parse_music_defaults_when_no_folder() {
        let parsed = parse_music_file(&PathBuf::from("track.mp3"), None, None);
        assert_eq!(parsed.artist, "Unknown Artist");
        assert_eq!(parsed.album, "Unknown Album");
        assert_eq!(parsed.year, None);
    }

    #[test]
    fn unknown_quality_defaults() {
        let parsed = parse_folder_episode(&PathBuf::from("Show.S02E03.mkv"), "Show", None);
        assert_eq!(parsed.quality, "Unknown");
        assert_eq!(parsed.season, 2);
        assert_eq!(parsed.episode, 3);
    }
}
