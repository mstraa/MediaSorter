export type MediaType = "tv" | "anime" | "film" | "music";

export interface InputRoot {
  id: number;
  path: string;
}

export interface PermissionCheck {
  label: string;
  exists: boolean;
  read: boolean;
  write: boolean | null;
}

export interface Settings {
  input_roots: string[];
  tv_output_root: string;
  anime_output_root: string;
  film_output_root: string;
  music_output_root: string;
  copy_rate_limit_mbps: string;
  checks: PermissionCheck[];
}

export interface BrowseEntry {
  name: string;
  relative_path: string;
  is_dir: boolean;
  is_video: boolean;
  is_hardlink: boolean;
  size: number | null;
  size_human: string;
  status: string | null;
  status_key: string;
  manual_status: string;
  latest_import_result: string | null;
  source_count: number;
}

export interface BrowseResponse {
  roots: InputRoot[];
  active_root: InputRoot | null;
  current_path: string;
  parent_path: string;
  entries: BrowseEntry[];
  error: string | null;
}

export interface ParsedMedia {
  source_name: string;
  title: string;
  year: number | null;
  season: number;
  episode: number;
  episode_title: string;
  quality: string;
}

export interface ShowCandidate {
  provider: string;
  provider_id: string;
  title: string;
  year: number | null;
  summary: string;
}

export interface EpisodeCandidate {
  provider: string;
  provider_show_id: string;
  season: number;
  episode: number;
  title: string;
}

export interface MatchEpisode {
  source_path: string;
  source_name: string;
  parsed: ParsedMedia;
  season_number: number;
  episode_number: number;
  episode_title: string;
  quality: string;
}

export interface MatchGroup {
  group_key: string;
  group_name: string;
  show_title: string;
  show_year: number | null;
  provider: string;
  provider_show_id: string;
  candidates: ShowCandidate[];
  metadata_error: string | null;
  episodes: MatchEpisode[];
}

export interface MatchResponse {
  media_type: MediaType;
  output_root: string | null;
  groups: MatchGroup[];
}

export interface ImportItem {
  source_path: string;
  show_title: string;
  show_year: number | null;
  season_number: number;
  episode_number: number;
  episode_title: string;
  quality: string;
  provider: string | null;
  provider_show_id: string | null;
}

export interface ImportBatch {
  media_type: MediaType;
  action: string;
  conflict_policy: string;
  items: ImportItem[];
}

export interface PreviewResult {
  source_path: string;
  final_path: string;
  result: string;
  error: string | null;
}

export interface JobItem {
  index: number;
  name: string;
  source_path: string;
  destination: string;
  status: string;
  bytes: number;
  total: number;
  /** What bytes/total count: "bytes" (copy, move) or "items" (hardlink, test). */
  unit: string;
  error: string | null;
  /** Skipped because the destination already existed: can be forced (replaced). */
  forceable: boolean;
}

export interface JobSnapshot {
  id: string;
  seq: number;
  label: string;
  state: string;
  percent: number;
  completed: number;
  total: number;
  total_items: number;
  completed_items: number;
  failed_items: number;
  cancelled_items: number;
  active: boolean;
  /** "bytes", "items", or "mixed" when the job combines both. */
  unit: string;
  error: string | null;
  items: JobItem[];
}

export interface LibraryFile {
  media_type: string;
  output_path: string;
  present: boolean;
  size: number | null;
  size_human: string;
}

export interface ImportHistoryRow {
  id: number;
  source_path: string;
  output_path: string;
  media_type: string;
  show_title: string;
  show_year: number | null;
  season_number: number;
  episode_number: number;
  episode_title: string;
  quality: string;
  action: string;
  conflict_policy: string;
  result: string;
  error: string | null;
  imported_at: string;
}

export interface FolderEntry {
  name: string;
  path: string;
}

export interface FoldersResponse {
  path: string;
  parent: string | null;
  folders: FolderEntry[];
  roots: string[];
}

// ---- Duplicates (dedupe) ----

export interface DedupeSettings {
  roots: string[];
  torrent_roots: string[];
  excludes: string[];
  min_size_mib: string;
  min_age_minutes: string;
  stat_rate: string;
  read_rate_mbps: string;
  schedule: string;
  preferred_owner: string;
}

export interface DedupeRootCheck {
  path: string;
  exists: boolean;
  readable: boolean;
  device: number | null;
  nested: boolean;
  error: string | null;
}

export interface DedupeRootsReport {
  roots: DedupeRootCheck[];
  same_device: boolean;
  warnings: string[];
}

export interface DedupeSettingsView {
  settings: DedupeSettings;
  report: DedupeRootsReport;
}

export interface DedupeScanCounters {
  dirs_done: number;
  entries_seen: number;
  files_recorded: number;
  bytes_recorded: number;
  skipped_small: number;
  skipped_recent: number;
  skipped_excluded: number;
  skipped_other_fs: number;
  skipped_errors: number;
}

export interface DedupeLiveStatus {
  state: string;
  trigger: string;
  run_id: number;
  generation: number;
  resumed: boolean;
  current_dir: string;
  pending_dirs: number;
  counters: DedupeScanCounters;
  elapsed_secs: number;
  dirs_per_sec: number;
  entries_per_sec: number;
  io_priority_idle: boolean;
  last_error: string | null;
  stage: "walk" | "partial" | "full" | "groups";
  hash_files_total: number;
  hash_files_done: number;
  hash_bytes_total: number;
  hash_bytes_done: number;
  hash_errors: number;
  bytes_per_sec: number;
}

export interface DedupeScanRun extends DedupeScanCounters {
  id: number;
  generation: number;
  roots: string[];
  state: string;
  trigger: string;
  started_at: string;
  updated_at: string;
  finished_at: string | null;
  elapsed_secs: number;
  last_error: string | null;
  pending_dirs: number;
  stage: "walk" | "hash";
  hash_files: number;
  hash_bytes: number;
  hash_errors: number;
  hash_secs: number;
}

export interface DedupeScanStatus {
  state: "idle" | "running" | "pausing" | "paused" | "stopping";
  live: DedupeLiveStatus | null;
  eta_secs: number | null;
  eta_basis: "previous_scan" | "queue" | "hash_bytes" | null;
  unfinished_run: DedupeScanRun | null;
  last_completed: DedupeScanRun | null;
  last_outcome: string | null;
  last_error: string | null;
  inventory: { files: number; bytes: number; inodes: number };
  schedule: string | null;
  in_schedule_window: boolean;
}

export interface DedupeFile {
  path: string;
  dev: number;
  ino: number;
  size: number;
  mtime: number;
  nlink: number;
  uid: number;
  gid: number;
}

export interface UnusedTorrentsResponse {
  torrent_roots: string[];
  last_scan_finished_at: string | null;
  files: DedupeFile[];
  total_count: number;
  total_bytes: number;
}

export interface DedupeMember {
  ino: number;
  nlink: number;
  uid: number;
  gid: number;
  mtime: number;
  keep: boolean;
  paths: string[];
}

export interface DedupeGroup {
  id: number;
  size: number;
  hash: string;
  inode_count: number;
  path_count: number;
  reclaimable: number;
  pair_label: string;
  keeper_ino: number | null;
  owners_differ: boolean;
  members: DedupeMember[];
}

export interface DedupePairSummary {
  label: string;
  groups: number;
  reclaimable: number;
}

export interface DedupeGroupsPage {
  summary: {
    groups: number;
    inodes: number;
    reclaimable: number;
    built_at: string | null;
    by_pair: DedupePairSummary[];
  };
  keeper_rule: string;
  total: number;
  total_reclaimable: number;
  groups: DedupeGroup[];
}

export interface DedupeGroupSelection {
  group_id: number;
  keeper_ino: number | null;
  exclude_inos: number[];
}

export interface DedupeOpResult {
  group_id: number;
  path: string;
  result: "simulated" | "replaced" | "skipped" | "failed";
  reason: string | null;
  freed_bytes: number;
}

export interface DedupeJob {
  id: string;
  mode: "simulate" | "apply";
  state: "running" | "done" | "cancelled" | "failed";
  groups_total: number;
  groups_done: number;
  paths_total: number;
  paths_done: number;
  bytes_to_compare: number;
  bytes_compared: number;
  freed_bytes: number;
  expected_freed_bytes: number;
  simulated: number;
  replaced: number;
  skipped: number;
  failed: number;
  current: string;
  error: string | null;
  recent: DedupeOpResult[];
}

export interface DedupeHistoryRow {
  id: number;
  job_id: string;
  mode: "simulate" | "apply";
  at: string;
  group_id: number | null;
  path: string;
  keeper_path: string | null;
  content_hash: string | null;
  before_ino: number | null;
  before_size: number | null;
  before_owner: string | null;
  before_mode: string | null;
  keeper_ino: number | null;
  keeper_owner: string | null;
  after_ino: number | null;
  result: string;
  reason: string | null;
  freed_bytes: number;
}

export interface DedupeHistoryPage {
  rows: DedupeHistoryRow[];
  total: number;
  total_freed: number;
}

export interface DedupeZfsInfo {
  available: boolean;
  datasets: { name: string; mountpoint: string; used_by_snapshots: number }[];
}
