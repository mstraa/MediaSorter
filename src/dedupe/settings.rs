//! Dedupe configuration, stored in the main `settings` table under `dedupe_*`
//! keys (like every other setting), plus the exclusion matcher and the
//! optional time window for automatic scans.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::db::Database;

pub const KEY_ROOTS: &str = "dedupe_roots";
pub const KEY_TORRENT_ROOTS: &str = "dedupe_torrent_roots";
pub const KEY_EXCLUDES: &str = "dedupe_excludes";
pub const KEY_MIN_SIZE_MIB: &str = "dedupe_min_size_mib";
pub const KEY_MIN_AGE_MINUTES: &str = "dedupe_min_age_minutes";
pub const KEY_STAT_RATE: &str = "dedupe_stat_rate";
pub const KEY_READ_RATE_MBPS: &str = "dedupe_read_rate_mbps";
pub const KEY_SCHEDULE: &str = "dedupe_schedule";
pub const KEY_PREFERRED_OWNER: &str = "dedupe_preferred_owner";

/// Default exclusions: in-progress downloads (qBittorrent's `Torrent-work/`
/// and partial-file suffixes), trash folders, and this module's own temporary
/// links so a scan never records a replacement half-way through.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "Torrent-work/",
    "*.!qB",
    "*.part",
    "*.tmp",
    ".Trash-*",
    ".ms-dedupe-*",
];
pub const DEFAULT_MIN_SIZE_MIB: u64 = 50;
pub const DEFAULT_MIN_AGE_MINUTES: u64 = 60;
pub const DEFAULT_STAT_RATE: u32 = 200;
pub const DEFAULT_READ_RATE_MBPS: f64 = 40.0;
/// qBittorrent's files as seen from the host when it runs in an unprivileged
/// LXC; the owner most of the library already shares.
pub const DEFAULT_PREFERRED_OWNER: &str = "100000:100000";

/// Resolved dedupe settings.
#[derive(Clone, Debug, PartialEq)]
pub struct DedupeSettings {
    pub roots: Vec<PathBuf>,
    pub torrent_roots: Vec<PathBuf>,
    pub excludes: Vec<String>,
    pub min_size_bytes: u64,
    pub min_age_secs: u64,
    /// Maximum `lstat`/`readdir` calls per second during the walk; 0 = unlimited.
    pub stat_rate: u32,
    /// Maximum hashing read throughput in MB/s; 0 = unlimited.
    pub read_rate_mbps: f64,
    pub schedule: Option<TimeWindow>,
    pub preferred_owner: Option<(u32, u32)>,
}

impl Default for DedupeSettings {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            torrent_roots: Vec::new(),
            excludes: DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect(),
            min_size_bytes: DEFAULT_MIN_SIZE_MIB * 1024 * 1024,
            min_age_secs: DEFAULT_MIN_AGE_MINUTES * 60,
            stat_rate: DEFAULT_STAT_RATE,
            read_rate_mbps: DEFAULT_READ_RATE_MBPS,
            schedule: None,
            preferred_owner: parse_owner(DEFAULT_PREFERRED_OWNER),
        }
    }
}

/// Wire format for `GET/PUT /api/dedupe/settings`: plain strings so the form
/// round-trips exactly what the user typed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DedupeSettingsPayload {
    pub roots: Vec<String>,
    pub torrent_roots: Vec<String>,
    pub excludes: Vec<String>,
    pub min_size_mib: String,
    pub min_age_minutes: String,
    pub stat_rate: String,
    pub read_rate_mbps: String,
    pub schedule: String,
    pub preferred_owner: String,
}

fn lines(value: &str) -> Vec<String> {
    value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Strip trailing slashes (but keep `/`) so roots compare and prefix-match
/// consistently.
pub fn normalize_root(value: &str) -> PathBuf {
    let trimmed = value.trim();
    let stripped = trimmed.trim_end_matches('/');
    if stripped.is_empty() && trimmed.starts_with('/') {
        PathBuf::from("/")
    } else {
        PathBuf::from(stripped)
    }
}

/// Torrent folders guessed from the roots when the user never set them: any
/// root whose last component is `Torrents` (any case).
fn guess_torrent_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter(|root| {
            root.file_name()
                .map(|name| name.to_string_lossy().eq_ignore_ascii_case("torrents"))
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

pub fn parse_owner(value: &str) -> Option<(u32, u32)> {
    let (uid, gid) = value.trim().split_once(':')?;
    Some((uid.trim().parse().ok()?, gid.trim().parse().ok()?))
}

impl DedupeSettings {
    pub fn load(db: &Database) -> Self {
        let defaults = Self::default();
        let roots: Vec<PathBuf> = lines(&db.get_setting(KEY_ROOTS, ""))
            .iter()
            .map(|r| normalize_root(r))
            .collect();
        // `None` (never saved) means "guess"; an explicitly empty value means
        // the user cleared it.
        let torrent_roots = match db.get_setting_opt(KEY_TORRENT_ROOTS) {
            Some(value) => lines(&value).iter().map(|r| normalize_root(r)).collect(),
            None => guess_torrent_roots(&roots),
        };
        let excludes = match db.get_setting_opt(KEY_EXCLUDES) {
            Some(value) => lines(&value),
            None => defaults.excludes.clone(),
        };
        let min_size_bytes = db
            .get_setting(KEY_MIN_SIZE_MIB, "")
            .trim()
            .parse::<u64>()
            .map(|mib| mib * 1024 * 1024)
            .unwrap_or(defaults.min_size_bytes);
        let min_age_secs = db
            .get_setting(KEY_MIN_AGE_MINUTES, "")
            .trim()
            .parse::<u64>()
            .map(|minutes| minutes * 60)
            .unwrap_or(defaults.min_age_secs);
        let stat_rate = db
            .get_setting(KEY_STAT_RATE, "")
            .trim()
            .parse::<u32>()
            .unwrap_or(defaults.stat_rate);
        let read_rate_mbps = db
            .get_setting(KEY_READ_RATE_MBPS, "")
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(defaults.read_rate_mbps);
        let schedule = TimeWindow::parse(&db.get_setting(KEY_SCHEDULE, ""));
        let preferred_owner = match db.get_setting_opt(KEY_PREFERRED_OWNER) {
            Some(value) => parse_owner(&value),
            None => defaults.preferred_owner,
        };
        Self {
            roots,
            torrent_roots,
            excludes,
            min_size_bytes,
            min_age_secs,
            stat_rate,
            read_rate_mbps,
            schedule,
            preferred_owner,
        }
    }

    pub fn to_payload(&self) -> DedupeSettingsPayload {
        DedupeSettingsPayload {
            roots: self.roots.iter().map(|p| p.display().to_string()).collect(),
            torrent_roots: self
                .torrent_roots
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            excludes: self.excludes.clone(),
            min_size_mib: (self.min_size_bytes / (1024 * 1024)).to_string(),
            min_age_minutes: (self.min_age_secs / 60).to_string(),
            stat_rate: self.stat_rate.to_string(),
            read_rate_mbps: format_number(self.read_rate_mbps),
            schedule: self.schedule.map(|w| w.to_string()).unwrap_or_default(),
            preferred_owner: self
                .preferred_owner
                .map(|(u, g)| format!("{u}:{g}"))
                .unwrap_or_default(),
        }
    }

    /// Validate a payload and persist it. Returns the user-facing reason when
    /// a field is invalid; nothing is written in that case.
    pub fn save(db: &Database, payload: &DedupeSettingsPayload) -> Result<Self, String> {
        let roots: Vec<PathBuf> = payload
            .roots
            .iter()
            .filter(|r| !r.trim().is_empty())
            .map(|r| normalize_root(r))
            .collect();
        for root in &roots {
            if !root.is_absolute() {
                return Err(format!("Dedupe root must be absolute: {}", root.display()));
            }
        }
        let torrent_roots: Vec<PathBuf> = payload
            .torrent_roots
            .iter()
            .filter(|r| !r.trim().is_empty())
            .map(|r| normalize_root(r))
            .collect();
        for torrent_root in &torrent_roots {
            if !roots.iter().any(|root| torrent_root.starts_with(root)) {
                return Err(format!(
                    "Torrent folder {} is not inside a dedupe root",
                    torrent_root.display()
                ));
            }
        }
        let excludes: Vec<String> = payload
            .excludes
            .iter()
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .collect();
        ExcludeMatcher::new(&excludes)?;
        let min_size_mib = parse_u64_field(&payload.min_size_mib, "Minimum size")?;
        let min_age_minutes = parse_u64_field(&payload.min_age_minutes, "Minimum age")?;
        let stat_rate = parse_u64_field(&payload.stat_rate, "Stat rate")?;
        let stat_rate = u32::try_from(stat_rate).map_err(|_| "Stat rate is too large")?;
        let read_rate: f64 = payload
            .read_rate_mbps
            .trim()
            .parse()
            .ok()
            .filter(|v: &f64| v.is_finite() && *v >= 0.0)
            .ok_or("Read rate must be a number ≥ 0 (0 = unlimited)")?;
        let schedule_text = payload.schedule.trim();
        let schedule = if schedule_text.is_empty() {
            None
        } else {
            Some(
                TimeWindow::parse(schedule_text)
                    .ok_or("Scan window must look like 01:00-07:00 (or be empty)")?,
            )
        };
        let owner_text = payload.preferred_owner.trim();
        let preferred_owner = if owner_text.is_empty() {
            None
        } else {
            Some(parse_owner(owner_text).ok_or("Preferred owner must look like uid:gid")?)
        };

        let join = |paths: &[PathBuf]| {
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        db.set_setting(KEY_ROOTS, &join(&roots));
        db.set_setting(KEY_TORRENT_ROOTS, &join(&torrent_roots));
        db.set_setting(KEY_EXCLUDES, &excludes.join("\n"));
        db.set_setting(KEY_MIN_SIZE_MIB, &min_size_mib.to_string());
        db.set_setting(KEY_MIN_AGE_MINUTES, &min_age_minutes.to_string());
        db.set_setting(KEY_STAT_RATE, &stat_rate.to_string());
        db.set_setting(KEY_READ_RATE_MBPS, &format_number(read_rate));
        db.set_setting(
            KEY_SCHEDULE,
            &schedule.map(|w| w.to_string()).unwrap_or_default(),
        );
        db.set_setting(
            KEY_PREFERRED_OWNER,
            &preferred_owner
                .map(|(u, g)| format!("{u}:{g}"))
                .unwrap_or_default(),
        );
        Ok(Self::load(db))
    }

    /// True when `path` is inside one of the configured torrent folders.
    /// Lexical (component-wise) on purpose: scan paths are already absolute
    /// and un-symlinked, and this runs per row without touching the disk.
    pub fn is_torrent_path(&self, path: &Path) -> bool {
        self.torrent_roots.iter().any(|root| path.starts_with(root))
    }
}

fn parse_u64_field(value: &str, label: &str) -> Result<u64, String> {
    value
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("{label} must be a whole number ≥ 0"))
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

// ---------------------------------------------------------------------------
// Exclusions
// ---------------------------------------------------------------------------

/// Compiled exclusion patterns.
///
/// - A pattern ending in `/` only matches directories (`Torrent-work/`).
/// - A pattern containing `/` is matched against the path relative to the
///   dedupe root (`Photos/raw/*`); `*` does not cross `/`, `**` does.
/// - Any other pattern is matched against the entry name alone (`*.part`).
pub struct ExcludeMatcher {
    any_name: GlobSet,
    any_path: GlobSet,
    dir_name: GlobSet,
    dir_path: GlobSet,
}

impl ExcludeMatcher {
    pub fn new(patterns: &[String]) -> Result<Self, String> {
        let mut any_name = GlobSetBuilder::new();
        let mut any_path = GlobSetBuilder::new();
        let mut dir_name = GlobSetBuilder::new();
        let mut dir_path = GlobSetBuilder::new();
        for raw in patterns {
            let pattern = raw.trim();
            if pattern.is_empty() {
                continue;
            }
            let dir_only = pattern.ends_with('/');
            let body = pattern.trim_end_matches('/').trim_start_matches('/');
            if body.is_empty() {
                return Err(format!("Invalid exclusion pattern: {raw}"));
            }
            let glob = build_glob(body).map_err(|e| format!("Invalid pattern {raw}: {e}"))?;
            match (dir_only, body.contains('/')) {
                (false, false) => any_name.add(glob),
                (false, true) => any_path.add(glob),
                (true, false) => dir_name.add(glob),
                (true, true) => dir_path.add(glob),
            };
        }
        let build = |b: GlobSetBuilder| b.build().map_err(|e| e.to_string());
        Ok(Self {
            any_name: build(any_name)?,
            any_path: build(any_path)?,
            dir_name: build(dir_name)?,
            dir_path: build(dir_path)?,
        })
    }

    /// `relative` is the entry's path relative to its dedupe root.
    pub fn is_excluded(&self, relative: &Path, is_dir: bool) -> bool {
        let name = match relative.file_name() {
            Some(name) => Path::new(name),
            None => return false,
        };
        if self.any_name.is_match(name) || self.any_path.is_match(relative) {
            return true;
        }
        is_dir && (self.dir_name.is_match(name) || self.dir_path.is_match(relative))
    }
}

fn build_glob(pattern: &str) -> Result<Glob, globset::Error> {
    GlobBuilder::new(pattern).literal_separator(true).build()
}

// ---------------------------------------------------------------------------
// Time window
// ---------------------------------------------------------------------------

/// Daily window in local time, in minutes since midnight. `start > end` wraps
/// past midnight (`22:00-06:00`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeWindow {
    pub start: u32,
    pub end: u32,
}

impl TimeWindow {
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        // Accept "-", the en dash and the em dash users paste from docs.
        let normalized = value.replace(['–', '—'], "-");
        let (start, end) = normalized.split_once('-')?;
        let start = parse_hhmm(start)?;
        let end = parse_hhmm(end)?;
        if start == end {
            return None;
        }
        Some(Self { start, end })
    }

    pub fn contains(&self, minute_of_day: u32) -> bool {
        if self.start < self.end {
            minute_of_day >= self.start && minute_of_day < self.end
        } else {
            minute_of_day >= self.start || minute_of_day < self.end
        }
    }

    pub fn contains_now(&self) -> bool {
        self.contains(local_minute_of_day())
    }
}

impl std::fmt::Display for TimeWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:02}:{:02}-{:02}:{:02}",
            self.start / 60,
            self.start % 60,
            self.end / 60,
            self.end % 60
        )
    }
}

fn parse_hhmm(value: &str) -> Option<u32> {
    let (h, m) = value.trim().split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h > 24 || m > 59 || (h == 24 && m != 0) {
        return None;
    }
    Some((h * 60 + m) % (24 * 60))
}

/// Minutes since local midnight (the container's timezone). Falls back to UTC
/// where `localtime_r` is not available.
pub fn local_minute_of_day() -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    #[cfg(unix)]
    {
        let t: libc::time_t = now as libc::time_t;
        // SAFETY: `tm` is plain old data, fully written by `localtime_r`.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
        if ok {
            return (tm.tm_hour as u32) * 60 + tm.tm_min as u32;
        }
    }
    ((now.rem_euclid(86_400)) / 60) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(patterns: &[&str]) -> ExcludeMatcher {
        ExcludeMatcher::new(&patterns.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn default_excludes_match_partial_downloads_and_work_dir() {
        let m = matcher(DEFAULT_EXCLUDES);
        assert!(m.is_excluded(Path::new("Torrent-work"), true));
        assert!(m.is_excluded(Path::new("sub/Torrent-work"), true));
        // Directory-only: a file named like it is kept.
        assert!(!m.is_excluded(Path::new("Torrent-work"), false));
        assert!(m.is_excluded(Path::new("Show/ep.mkv.!qB"), false));
        assert!(m.is_excluded(Path::new("Show/ep.mkv.part"), false));
        assert!(m.is_excluded(Path::new("Show/.ms-dedupe-1234.tmp"), false));
        assert!(m.is_excluded(Path::new(".Trash-1000"), true));
        assert!(!m.is_excluded(Path::new("Show/ep.mkv"), false));
    }

    #[test]
    fn slash_patterns_match_relative_paths() {
        let m = matcher(&["Photos/raw/*", "Audio/"]);
        assert!(m.is_excluded(Path::new("Photos/raw/a.cr2"), false));
        assert!(!m.is_excluded(Path::new("Photos/raw/sub/a.cr2"), false));
        assert!(!m.is_excluded(Path::new("Other/Photos/raw/a.cr2"), false));
        assert!(m.is_excluded(Path::new("Audio"), true));
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        assert!(ExcludeMatcher::new(&["/".to_string()]).is_err());
        assert!(ExcludeMatcher::new(&["[".to_string()]).is_err());
    }

    #[test]
    fn time_windows_parse_and_wrap() {
        let w = TimeWindow::parse("01:00–07:00").unwrap();
        assert_eq!(w.to_string(), "01:00-07:00");
        assert!(w.contains(60));
        assert!(w.contains(6 * 60 + 59));
        assert!(!w.contains(7 * 60));
        let night = TimeWindow::parse("22:30-06:00").unwrap();
        assert!(night.contains(23 * 60));
        assert!(night.contains(5 * 60));
        assert!(!night.contains(12 * 60));
        assert!(TimeWindow::parse("bogus").is_none());
        assert!(TimeWindow::parse("05:00-05:00").is_none());
        assert!(TimeWindow::parse("").is_none());
    }

    #[test]
    fn owner_parses() {
        assert_eq!(parse_owner("100000:100000"), Some((100000, 100000)));
        assert_eq!(parse_owner(" 0 : 0 "), Some((0, 0)));
        assert_eq!(parse_owner("root"), None);
    }

    #[test]
    fn settings_round_trip_and_guess_torrent_roots() {
        let path = std::env::temp_dir().join(format!("ms-dedupe-set-{}.db", uuid::Uuid::new_v4()));
        let db = Database::open(&path).unwrap();
        db.set_setting(KEY_ROOTS, "/mnt/data/Torrents/\n/mnt/data/Tv Show");
        let loaded = DedupeSettings::load(&db);
        assert_eq!(
            loaded.torrent_roots,
            vec![PathBuf::from("/mnt/data/Torrents")]
        );
        assert_eq!(loaded.min_size_bytes, 50 * 1024 * 1024);
        assert_eq!(loaded.preferred_owner, Some((100000, 100000)));

        let mut payload = loaded.to_payload();
        payload.torrent_roots = vec!["/elsewhere".into()];
        assert!(DedupeSettings::save(&db, &payload).is_err());
        payload.torrent_roots = vec![];
        payload.schedule = "01:00-07:00".into();
        payload.read_rate_mbps = "0".into();
        let saved = DedupeSettings::save(&db, &payload).unwrap();
        // Explicitly cleared: no guessing any more.
        assert!(saved.torrent_roots.is_empty());
        assert_eq!(saved.schedule, TimeWindow::parse("01:00-07:00"));
        assert_eq!(saved.read_rate_mbps, 0.0);
        let _ = std::fs::remove_file(&path);
    }
}
