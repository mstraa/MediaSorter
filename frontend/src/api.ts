import type {
  BrowseResponse,
  DedupeGroupSelection,
  DedupeGroupsPage,
  DedupeHistoryPage,
  DedupeJob,
  DedupeZfsInfo,
  DedupeScanStatus,
  DedupeSettings,
  DedupeSettingsView,
  EpisodeCandidate,
  FoldersResponse,
  ImportBatch,
  ImportHistoryRow,
  JobSnapshot,
  LibraryFile,
  MatchResponse,
  MediaType,
  PreviewResult,
  Settings,
  ShowCandidate,
  UnusedTorrentsResponse,
} from "./types";

export class ApiError extends Error {}

async function request<T>(url: string, options?: RequestInit): Promise<T> {
  const response = await fetch(url, {
    headers: { "Content-Type": "application/json" },
    ...options,
  });
  if (!response.ok) {
    let detail = `Request failed (${response.status})`;
    try {
      const body = await response.json();
      if (body && typeof body.detail === "string") {
        detail = body.detail;
      }
    } catch {
      // ignore parse errors
    }
    throw new ApiError(detail);
  }
  if (response.status === 204) {
    return undefined as T;
  }
  return (await response.json()) as T;
}

export const api = {
  // ---- Duplicates ----
  getDedupeSettings: () => request<DedupeSettingsView>("/api/dedupe/settings"),
  saveDedupeSettings: (settings: DedupeSettings) =>
    request<DedupeSettingsView>("/api/dedupe/settings", {
      method: "PUT",
      body: JSON.stringify(settings),
    }),
  getDedupeScanStatus: () => request<DedupeScanStatus>("/api/dedupe/scan/status"),
  startDedupeScan: (fresh = false) =>
    request<DedupeScanStatus>("/api/dedupe/scan", {
      method: "POST",
      body: JSON.stringify({ fresh }),
    }),
  dedupeScanAction: (action: "pause" | "resume" | "cancel" | "clear") =>
    request<{ status: string }>(`/api/dedupe/scan/${action}`, { method: "POST" }),
  getDedupeGroups: (params: {
    limit: number;
    offset: number;
    pair?: string;
    folder?: string;
    minSize?: number;
  }) => {
    const query = new URLSearchParams({ limit: String(params.limit), offset: String(params.offset) });
    if (params.pair) query.set("pair", params.pair);
    if (params.folder) query.set("folder", params.folder);
    if (params.minSize) query.set("min_size", String(params.minSize));
    return request<DedupeGroupsPage>(`/api/dedupe/groups?${query.toString()}`);
  },
  startDedupeJob: (mode: "simulate" | "apply", groups: DedupeGroupSelection[]) =>
    request<DedupeJob>("/api/dedupe/jobs", {
      method: "POST",
      body: JSON.stringify({ mode, groups }),
    }),
  getLatestDedupeJob: () => request<DedupeJob | null>("/api/dedupe/jobs"),
  getDedupeJob: (id: string) => request<DedupeJob>(`/api/dedupe/jobs/${id}`),
  cancelDedupeJob: (id: string) =>
    request<{ status: string }>(`/api/dedupe/jobs/${id}/cancel`, { method: "POST" }),
  getDedupeHistory: (limit: number, offset: number, mode?: string) => {
    const query = new URLSearchParams({ limit: String(limit), offset: String(offset) });
    if (mode) query.set("mode", mode);
    return request<DedupeHistoryPage>(`/api/dedupe/history?${query.toString()}`);
  },
  getDedupeSpace: () => request<DedupeZfsInfo>("/api/dedupe/space"),
  getUnusedTorrents: (limit: number, offset: number) =>
    request<UnusedTorrentsResponse>(
      `/api/dedupe/unused-torrents?limit=${limit}&offset=${offset}`,
    ),

  getSettings: () => request<Settings>("/api/settings"),
  saveSettings: (payload: {
    input_roots: string[];
    tv_output_root: string;
    anime_output_root: string;
    film_output_root: string;
    music_output_root: string;
    copy_rate_limit_mbps: string;
  }) =>
    request<{ status: string }>("/api/settings", {
      method: "PUT",
      body: JSON.stringify(payload),
    }),

  getBrowse: (rootId: number | null, path: string, status?: string) => {
    const params = new URLSearchParams();
    if (rootId != null) params.set("root_id", String(rootId));
    params.set("path", path);
    // Filtering server-side lets "Only no status" skip folders that already
    // have a status instead of walking every file under them.
    if (status && status !== "all") params.set("status", status);
    return request<BrowseResponse>(`/api/browse?${params.toString()}`);
  },

  postMatch: (rootId: number, mediaType: MediaType, selected: string[]) =>
    request<MatchResponse>("/api/match", {
      method: "POST",
      body: JSON.stringify({ root_id: rootId, media_type: mediaType, selected }),
    }),

  postPreview: (batch: ImportBatch) =>
    request<{ results: PreviewResult[] }>("/api/preview", {
      method: "POST",
      body: JSON.stringify(batch),
    }),

  startImportJob: (batch: ImportBatch) =>
    request<JobSnapshot>("/api/import-jobs", {
      method: "POST",
      body: JSON.stringify(batch),
    }),

  /**
   * Poll the job list. Pass the `etag` of the list you already hold: resolves
   * to `null` when it is unchanged (the server answered 304), so the caller
   * can skip a re-render.
   */
  listImportJobs: async (
    etag?: string | null,
  ): Promise<{ jobs: JobSnapshot[]; etag: string | null } | null> => {
    const headers: Record<string, string> = {};
    if (etag) headers["If-None-Match"] = etag;
    // `no-store` keeps the browser cache out of it: otherwise it revalidates
    // on its own and hands back a 200 with the cached body, hiding the 304.
    const response = await fetch("/api/import-jobs", { headers, cache: "no-store" });
    if (response.status === 304) return null;
    if (!response.ok) throw new ApiError(`Request failed (${response.status})`);
    const body = (await response.json()) as { jobs: JobSnapshot[] };
    return { jobs: body.jobs, etag: response.headers.get("ETag") };
  },

  clearImportJobs: () =>
    request<{ cleared: number; jobs: JobSnapshot[] }>("/api/import-jobs/clear", { method: "POST" }),

  getImportJob: (id: string) => request<JobSnapshot>(`/api/import-jobs/${encodeURIComponent(id)}`),

  cancelImportJob: (id: string) =>
    request<JobSnapshot>(`/api/import-jobs/${encodeURIComponent(id)}/cancel`, { method: "POST" }),

  cancelImportItem: (id: string, index: number) =>
    request<JobSnapshot>(
      `/api/import-jobs/${encodeURIComponent(id)}/items/${index}/cancel`,
      { method: "POST" },
    ),

  /** Re-import skipped files, replacing their existing destination. No indexes = every skipped file. */
  forceImportJob: (id: string, indexes?: number[]) =>
    request<JobSnapshot>(`/api/import-jobs/${encodeURIComponent(id)}/force`, {
      method: "POST",
      body: JSON.stringify(indexes ? { indexes } : {}),
    }),

  getImportJobResults: (id: string) =>
    request<{ results: PreviewResult[] }>(`/api/import-jobs/${encodeURIComponent(id)}/results`),

  getLibrary: () =>
    request<{ files: LibraryFile[]; roots: Record<string, string> }>("/api/library"),

  rescanLibrary: () =>
    request<{ counts: Record<string, number> }>("/api/library/rescan", { method: "POST" }),

  getHistory: () => request<{ imports: ImportHistoryRow[] }>("/api/history"),

  search: (mediaType: MediaType, q: string) => {
    const params = new URLSearchParams({ media_type: mediaType, q });
    return request<{ results: ShowCandidate[] }>(`/api/search?${params.toString()}`);
  },

  episodes: (mediaType: MediaType, providerShowId: string, provider?: string) => {
    const params = new URLSearchParams({ media_type: mediaType, provider_show_id: providerShowId });
    if (provider) params.set("provider", provider);
    return request<{ results: EpisodeCandidate[] }>(`/api/episodes?${params.toString()}`);
  },

  setSourceStatus: (payload: {
    status: string;
    root_id?: number;
    selected?: string[];
    source_path?: string;
  }) =>
    request<{ status: string; updated: number }>("/api/source-status", {
      method: "POST",
      body: JSON.stringify(payload),
    }),

  getFolders: (path: string) =>
    request<FoldersResponse>(`/api/folders?path=${encodeURIComponent(path)}`),
};
