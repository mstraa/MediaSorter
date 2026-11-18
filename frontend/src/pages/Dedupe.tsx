import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { api, ApiError } from "../api";
import { useProgress } from "../components/Progress";
import { formatBytes } from "../theme";
import type {
  DedupeGroup,
  DedupeGroupsPage,
  DedupeRootsReport,
  DedupeScanRun,
  DedupeScanStatus,
  UnusedTorrentsResponse,
} from "../types";

const PAGE_SIZE = 200;

/** SQLite `CURRENT_TIMESTAMP` is UTC without a zone; show it in local time. */
export function formatDbTime(value: string | null | undefined): string {
  if (!value) return "-";
  const date = new Date(`${value.replace(" ", "T")}Z`);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

export function formatDuration(totalSeconds: number | null | undefined): string {
  if (totalSeconds == null || !Number.isFinite(totalSeconds)) return "-";
  const s = Math.max(0, Math.round(totalSeconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (h > 0) return `${h} h ${String(m).padStart(2, "0")} min`;
  if (m > 0) return `${m} min ${String(s % 60).padStart(2, "0")} s`;
  return `${s} s`;
}

function formatCount(value: number): string {
  return value.toLocaleString("fr-FR");
}

function errorText(e: unknown): string {
  return e instanceof ApiError ? e.message : String(e);
}

type Tab = "groups" | "unused";

export default function DedupePage() {
  const [status, setStatus] = useState<DedupeScanStatus | null>(null);
  const [report, setReport] = useState<DedupeRootsReport | null>(null);
  const [hasRoots, setHasRoots] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>("groups");

  const refresh = useCallback(() => {
    api
      .getDedupeScanStatus()
      .then((next) => {
        setStatus(next);
        setError(null);
      })
      .catch((e) => setError(errorText(e)));
  }, []);

  useEffect(() => {
    api
      .getDedupeSettings()
      .then((view) => {
        setReport(view.report);
        setHasRoots(view.settings.roots.length > 0);
      })
      .catch((e) => setError(errorText(e)));
    refresh();
  }, [refresh]);

  // Poll quickly while a scan runs, slowly otherwise (an automatic scan may
  // start on its own inside the configured window).
  const active = status != null && status.state !== "idle";
  useEffect(() => {
    const id = window.setInterval(refresh, active ? 2000 : 15000);
    return () => window.clearInterval(id);
  }, [active, refresh]);

  return (
    <>
      <section className="panel">
        <h1>Duplicates</h1>
        <p className="muted">
          Finds files with identical content stored on different inodes of the same filesystem. The
          scan is read-only and throttled; nothing is modified on disk.
        </p>
        {error && <p className="error">{error}</p>}
        {!hasRoots && (
          <p className="error">
            No dedupe roots configured yet. Add them in <Link to="/settings">Settings</Link>.
          </p>
        )}
        {report && report.warnings.length > 0 && (
          <div className="dedupe-warnings">
            {report.warnings.map((warning, i) => (
              <p key={i} className={!report.same_device && i === 0 ? "error" : "warning-note"}>
                {warning}
              </p>
            ))}
          </div>
        )}
        {status && <ScanPanel status={status} onChange={refresh} />}
      </section>

      <section className="panel">
        <div className="segmented" role="tablist" aria-label="Duplicates views">
          <button
            type="button"
            role="tab"
            className={`segmented-button ${tab === "groups" ? "active" : ""}`}
            onClick={() => setTab("groups")}
          >
            Duplicates
          </button>
          <button
            type="button"
            role="tab"
            className={`segmented-button ${tab === "unused" ? "active" : ""}`}
            onClick={() => setTab("unused")}
          >
            Unused torrents
          </button>
        </div>
        {tab === "groups" && <GroupsList scanGeneration={status?.last_completed?.id ?? 0} />}
        {tab === "unused" && <UnusedTorrents scanGeneration={status?.last_completed?.id ?? 0} />}
      </section>
    </>
  );
}

function ScanPanel({ status, onChange }: { status: DedupeScanStatus; onChange: () => void }) {
  const progress = useProgress();
  const [actionError, setActionError] = useState<string | null>(null);
  const live = status.live;
  const lastFull = status.last_completed;
  const unfinished = status.unfinished_run;

  async function run(label: string, action: () => Promise<unknown>) {
    setActionError(null);
    progress.startDelayed(label);
    try {
      await action();
    } catch (e) {
      setActionError(errorText(e));
    } finally {
      progress.hide();
      onChange();
    }
  }

  const hashing = live != null && (live.stage === "partial" || live.stage === "full");
  const percent = !live
    ? null
    : hashing
      ? live.hash_bytes_total > 0
        ? Math.min(99, Math.floor((live.hash_bytes_done / live.hash_bytes_total) * 100))
        : null
      : live.stage === "walk" && status.eta_basis === "previous_scan" && lastFull && lastFull.dirs_done > 0
        ? Math.min(99, Math.floor((live.counters.dirs_done / lastFull.dirs_done) * 100))
        : null;
  const etaText =
    status.eta_secs == null
      ? "-"
      : `${status.eta_basis === "queue" ? "≥ " : "≈ "}${formatDuration(status.eta_secs)}`;

  return (
    <div className="dedupe-scan">
      <div className="toolbar">
        <span className={`state-badge state-${status.state === "idle" ? "done" : "running"}`}>
          {status.state}
        </span>
        {status.state === "idle" && (
          <>
            <button type="button" onClick={() => run("Starting scan...", () => api.startDedupeScan())}>
              {unfinished ? "Resume scan" : "Start scan"}
            </button>
            {unfinished && (
              <button
                className="secondary-button"
                type="button"
                onClick={() => run("Starting scan...", () => api.startDedupeScan(true))}
              >
                Restart from scratch
              </button>
            )}
          </>
        )}
        {(status.state === "running" || status.state === "pausing") && (
          <button
            className="secondary-button"
            type="button"
            onClick={() => run("Pausing...", () => api.dedupeScanAction("pause"))}
          >
            Pause
          </button>
        )}
        {status.state === "paused" && (
          <button type="button" onClick={() => run("Resuming...", () => api.dedupeScanAction("resume"))}>
            Resume
          </button>
        )}
        {status.state !== "idle" && status.state !== "stopping" && (
          <button
            className="secondary-button danger"
            type="button"
            onClick={() => run("Stopping...", () => api.dedupeScanAction("cancel"))}
          >
            Stop (keeps progress)
          </button>
        )}
        {status.schedule && (
          <span className="muted">
            automatic window {status.schedule}
            {status.in_schedule_window ? " (open)" : ""}
          </span>
        )}
      </div>
      {actionError && <p className="error">{actionError}</p>}

      {live && (
        <div className="dedupe-live">
          <div className="progress-track">
            <div
              className={`progress-bar ${percent == null ? "indeterminate" : ""}`}
              style={percent == null ? undefined : { width: `${percent}%` }}
            />
          </div>
          <p className="dedupe-stage">{STAGE_LABELS[live.stage] ?? live.stage}</p>
          {hashing ? (
            <dl className="dedupe-stats">
              <Stat
                label="Files hashed"
                value={`${formatCount(live.hash_files_done)} / ${formatCount(live.hash_files_total)}`}
              />
              <Stat
                label="Read"
                value={`${formatBytes(live.hash_bytes_done)} / ${formatBytes(live.hash_bytes_total)}`}
              />
              <Stat label="Speed" value={`${formatBytes(live.bytes_per_sec)}/s`} />
              <Stat label="Time left (this pass)" value={etaText} />
              <Stat label="Read errors" value={formatCount(live.hash_errors)} />
              <Stat label="Elapsed (walk)" value={formatDuration(live.elapsed_secs)} />
            </dl>
          ) : (
          <dl className="dedupe-stats">
            <Stat label="Folders walked" value={formatCount(live.counters.dirs_done)} />
            <Stat label="Folders queued" value={formatCount(live.pending_dirs)} />
            <Stat label="Entries seen" value={formatCount(live.counters.entries_seen)} />
            <Stat
              label="Files recorded"
              value={`${formatCount(live.counters.files_recorded)} · ${formatBytes(live.counters.bytes_recorded)}`}
            />
            <Stat
              label="Speed"
              value={`${live.entries_per_sec.toFixed(0)} entries/s · ${live.dirs_per_sec.toFixed(1)} folders/s`}
            />
            <Stat label="Elapsed" value={formatDuration(live.elapsed_secs)} />
            <Stat
              label="Time left"
              value={
                etaText
              }
            />
            <Stat label="I/O class" value={live.io_priority_idle ? "idle" : "normal (throttled)"} />
          </dl>
          )}
          <p className="muted dedupe-current">{live.current_dir}</p>
          {live.stage === "walk" && status.eta_basis === "queue" && (
            <p className="muted">
              First scan: the estimate only counts folders discovered so far and grows as the walk
              goes deeper.
            </p>
          )}
          {live.resumed && <p className="muted">Resumed from the last checkpoint.</p>}
          {live.last_error && <p className="muted">Last read error: {live.last_error}</p>}
        </div>
      )}

      {!live && unfinished && (
        <p className="warning-note">
          An unfinished scan (started {formatDbTime(unfinished.started_at)}) stopped{" "}
          {unfinished.stage === "hash"
            ? "while hashing; the walk is done, Resume goes straight back to hashing."
            : `after ${formatCount(unfinished.dirs_done)} folders with ${formatCount(unfinished.pending_dirs)} still queued. Resume continues where it stopped.`}
        </p>
      )}
      {status.last_outcome === "failed" && status.last_error && (
        <p className="error">Last scan failed: {status.last_error}</p>
      )}

      <dl className="dedupe-stats">
        <Stat label="Last full scan" value={lastFull ? formatDbTime(lastFull.finished_at) : "never"} />
        {lastFull && (
          <Stat
            label="Took"
            value={`${formatDuration(lastFull.elapsed_secs)} walk · ${formatDuration(lastFull.hash_secs)} hash`}
          />
        )}
        {lastFull && (
          <Stat
            label="Hashed last time"
            value={`${formatCount(lastFull.hash_files)} files · ${formatBytes(lastFull.hash_bytes)} read`}
          />
        )}
        <Stat
          label="Known files"
          value={`${formatCount(status.inventory.files)} paths · ${formatCount(status.inventory.inodes)} inodes · ${formatBytes(status.inventory.bytes)}`}
        />
      </dl>
      {lastFull && <SkipSummary run={lastFull} />}
    </div>
  );
}

const STAGE_LABELS: Record<string, string> = {
  walk: "Step 1/3 · walking folders (metadata only)",
  partial: "Step 2/3 · quick hash (head, middle and tail of same-size files)",
  full: "Step 3/3 · full hash (only files whose quick hash matched)",
  groups: "Building duplicate groups...",
};

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <dt>{label}</dt>
      <dd>{value}</dd>
    </div>
  );
}

function SkipSummary({ run }: { run: DedupeScanRun }) {
  const parts = [
    run.skipped_small > 0 && `${formatCount(run.skipped_small)} below the minimum size`,
    run.skipped_recent > 0 && `${formatCount(run.skipped_recent)} modified too recently`,
    run.skipped_excluded > 0 && `${formatCount(run.skipped_excluded)} excluded`,
    run.skipped_other_fs > 0 && `${formatCount(run.skipped_other_fs)} on another filesystem`,
    run.skipped_errors > 0 && `${formatCount(run.skipped_errors)} unreadable`,
    run.hash_errors > 0 && `${formatCount(run.hash_errors)} could not be hashed`,
  ].filter(Boolean);
  if (parts.length === 0) return null;
  return <p className="muted">Ignored in the last full scan: {parts.join(" · ")}.</p>;
}

function UnusedTorrents({ scanGeneration }: { scanGeneration: number }) {
  const [data, setData] = useState<UnusedTorrentsResponse | null>(null);
  const [offset, setOffset] = useState(0);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .getUnusedTorrents(PAGE_SIZE, offset)
      .then((next) => {
        setData(next);
        setError(null);
      })
      .catch((e) => setError(errorText(e)));
  }, [offset, scanGeneration]);

  if (error) return <p className="error">{error}</p>;
  if (!data) return <p className="muted">Loading...</p>;
  if (data.torrent_roots.length === 0) {
    return (
      <p className="muted">
        No torrent folder configured. Set one under Duplicates in <Link to="/settings">Settings</Link>.
      </p>
    );
  }

  const now = Date.now() / 1000;
  // Paths relative to their torrent folder: the absolute prefix is the same on
  // every row and would only push the useful part out of view.
  const relative = (path: string) => {
    const root = data.torrent_roots.find((r) => path.startsWith(`${r}/`));
    return root ? path.slice(root.length + 1) : path;
  };
  return (
    <div className="dedupe-unused">
      <p className="muted">
        Files under {data.torrent_roots.join(", ")} with a single link (nlink = 1): nothing in the
        library points to them any more. Link counts are as of the last scan
        {data.last_scan_finished_at ? ` (${formatDbTime(data.last_scan_finished_at)})` : ""}. Read-only
        for now.
      </p>
      <p>
        <strong>{formatCount(data.total_count)}</strong> files ·{" "}
        <strong>{formatBytes(data.total_bytes)}</strong>
      </p>
      {data.files.length > 0 && (
        <table>
          <thead>
            <tr>
              <th>Path</th>
              <th>Size</th>
              <th>Age</th>
              <th>Owner</th>
            </tr>
          </thead>
          <tbody>
            {data.files.map((file) => (
              <tr key={file.path}>
                <td className="path-cell" title={file.path}>
                  {relative(file.path)}
                </td>
                <td className="nowrap-cell">{formatBytes(file.size)}</td>
                <td className="nowrap-cell">{Math.max(0, Math.floor((now - file.mtime) / 86400))} d</td>
                <td className="nowrap-cell">
                  {file.uid}:{file.gid}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {data.total_count > PAGE_SIZE && (
        <div className="toolbar dedupe-pager">
          <button
            className="secondary-button"
            type="button"
            disabled={offset === 0}
            onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}
          >
            Previous
          </button>
          <span className="muted">
            {offset + 1}–{Math.min(offset + PAGE_SIZE, data.total_count)} of{" "}
            {formatCount(data.total_count)}
          </span>
          <button
            className="secondary-button"
            type="button"
            disabled={offset + PAGE_SIZE >= data.total_count}
            onClick={() => setOffset(offset + PAGE_SIZE)}
          >
            Next
          </button>
        </div>
      )}
    </div>
  );
}

const GROUPS_PAGE = 50;

const MIN_SIZES: { label: string; bytes: number }[] = [
  { label: "any size", bytes: 0 },
  { label: "≥ 100 Mo", bytes: 100 * 1024 ** 2 },
  { label: "≥ 1 Go", bytes: 1024 ** 3 },
  { label: "≥ 5 Go", bytes: 5 * 1024 ** 3 },
  { label: "≥ 20 Go", bytes: 20 * 1024 ** 3 },
];

function GroupsList({ scanGeneration }: { scanGeneration: number }) {
  const [data, setData] = useState<DedupeGroupsPage | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pair, setPair] = useState("");
  const [folderInput, setFolderInput] = useState("");
  const [folder, setFolder] = useState("");
  const [minSize, setMinSize] = useState(0);
  const [offset, setOffset] = useState(0);

  useEffect(() => {
    api
      .getDedupeGroups({ limit: GROUPS_PAGE, offset, pair, folder, minSize })
      .then((next) => {
        setData(next);
        setError(null);
      })
      .catch((e) => setError(errorText(e)));
  }, [offset, pair, folder, minSize, scanGeneration]);

  function choosePair(label: string) {
    setPair(label === pair ? "" : label);
    setOffset(0);
  }

  if (!data && !error) return <p className="muted">Loading...</p>;
  const summary = data?.summary;

  return (
    <div className="dedupe-groups">
      {error && <p className="error">{error}</p>}
      {summary && summary.groups === 0 ? (
        <p className="muted">
          No duplicates found yet. Groups appear once a scan has finished its hashing steps.
        </p>
      ) : (
        summary && (
          <>
            <dl className="dedupe-stats">
              <Stat label="Groups" value={formatCount(summary.groups)} />
              <Stat label="Recoverable" value={formatBytes(summary.reclaimable)} />
              <Stat label="Inodes involved" value={formatCount(summary.inodes)} />
              <Stat label="Built" value={formatDbTime(summary.built_at)} />
            </dl>
            <table className="dedupe-pairs">
              <thead>
                <tr>
                  <th>Folders</th>
                  <th>Groups</th>
                  <th>Recoverable</th>
                </tr>
              </thead>
              <tbody>
                {summary.by_pair.map((p) => (
                  <tr
                    key={p.label}
                    className={`clickable-row ${pair === p.label ? "selected-row" : ""}`}
                    onClick={() => choosePair(p.label)}
                  >
                    <td>{p.label}</td>
                    <td className="nowrap-cell">{formatCount(p.groups)}</td>
                    <td className="nowrap-cell">{formatBytes(p.reclaimable)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
            <p className="muted">
              Read-only for now: nothing is linked yet. The kept file is chosen by {data?.keeper_rule}.
              Space only comes back once ZFS snapshots holding the old copies expire.
            </p>
          </>
        )
      )}

      {summary && summary.groups > 0 && data && (
        <>
          <div className="toolbar">
            <label className="toolbar-field">
              Folders
              <select
                value={pair}
                onChange={(e) => {
                  setPair(e.target.value);
                  setOffset(0);
                }}
              >
                <option value="">all</option>
                {summary.by_pair.map((p) => (
                  <option key={p.label} value={p.label}>
                    {p.label}
                  </option>
                ))}
              </select>
            </label>
            <label className="toolbar-field">
              File size
              <select
                value={minSize}
                onChange={(e) => {
                  setMinSize(Number(e.target.value));
                  setOffset(0);
                }}
              >
                {MIN_SIZES.map((m) => (
                  <option key={m.bytes} value={m.bytes}>
                    {m.label}
                  </option>
                ))}
              </select>
            </label>
            <form
              className="toolbar-field dedupe-folder-filter"
              onSubmit={(e) => {
                e.preventDefault();
                setFolder(folderInput.trim());
                setOffset(0);
              }}
            >
              Under folder
              <input
                value={folderInput}
                placeholder="/mnt/data/Films"
                onChange={(e) => setFolderInput(e.target.value)}
                onBlur={() => {
                  setFolder(folderInput.trim());
                  setOffset(0);
                }}
              />
            </form>
            <span className="muted">
              {formatCount(data.total)} groups · {formatBytes(data.total_reclaimable)} recoverable
            </span>
          </div>

          {data.groups.map((group) => (
            <GroupCard key={group.id} group={group} />
          ))}

          {data.total > GROUPS_PAGE && (
            <div className="toolbar dedupe-pager">
              <button
                className="secondary-button"
                type="button"
                disabled={offset === 0}
                onClick={() => setOffset(Math.max(0, offset - GROUPS_PAGE))}
              >
                Previous
              </button>
              <span className="muted">
                {offset + 1}–{Math.min(offset + GROUPS_PAGE, data.total)} of {formatCount(data.total)}
              </span>
              <button
                className="secondary-button"
                type="button"
                disabled={offset + GROUPS_PAGE >= data.total}
                onClick={() => setOffset(offset + GROUPS_PAGE)}
              >
                Next
              </button>
            </div>
          )}
        </>
      )}
    </div>
  );
}

function GroupCard({ group }: { group: DedupeGroup }) {
  return (
    <div className="match-group dedupe-group">
      <div className="group-header">
        <div className="group-show">
          <strong>{formatBytes(group.reclaimable)}</strong>
          <span className="muted">
            recoverable · {group.inode_count} copies of {formatBytes(group.size)} · {group.pair_label}
          </span>
          <span className="muted dedupe-hash" title={group.hash}>
            blake3 {group.hash.slice(0, 12)}
          </span>
        </div>
      </div>
      {group.owners_differ && (
        <p className="warning-note">
          Owners differ. After linking, every path shares the kept file&apos;s owner, permissions and
          modification time.
        </p>
      )}
      <table>
        <thead>
          <tr>
            <th></th>
            <th>Path(s)</th>
            <th>Inode</th>
            <th>Links</th>
            <th>Owner</th>
            <th>Modified</th>
          </tr>
        </thead>
        <tbody>
          {group.members.map((member) => (
            <tr key={member.ino} className={member.keep ? "dedupe-keep" : ""}>
              <td className="nowrap-cell">
                <span className={`state-badge ${member.keep ? "state-imported" : "state-skipped"}`}>
                  {member.keep ? "keep" : "duplicate"}
                </span>
              </td>
              <td className="path-cell">
                {member.paths.map((path) => (
                  <div key={path}>{path}</div>
                ))}
              </td>
              <td className="nowrap-cell">{member.ino}</td>
              <td className="nowrap-cell">{member.nlink}</td>
              <td className="nowrap-cell">
                {member.uid}:{member.gid}
              </td>
              <td className="nowrap-cell">{new Date(member.mtime * 1000).toLocaleDateString()}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
