import type {
  BrowseResponse,
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

  getBrowse: (rootId: number | null, path: string) => {
    const params = new URLSearchParams();
    if (rootId != null) params.set("root_id", String(rootId));
    params.set("path", path);
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

  listImportJobs: () => request<{ jobs: JobSnapshot[] }>("/api/import-jobs"),

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
