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
