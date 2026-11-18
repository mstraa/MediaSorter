import { useEffect, useId, useState } from "react";
import { api, ApiError } from "../api";
import { formatBytes } from "../theme";
import type { DedupeHistoryPage, DedupeJob, DedupeZfsInfo } from "../types";

function errorText(e: unknown): string {
  return e instanceof ApiError ? e.message : String(e);
}

function formatCount(value: number): string {
  return value.toLocaleString("fr-FR");
}

function formatDbTime(value: string | null | undefined): string {
  if (!value) return "-";
  const date = new Date(`${value.replace(" ", "T")}Z`);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

export interface SelectionSummary {
  groups: number;
  files: number;
  bytes: number;
  ownersDiffer: number;
}

/** Confirmation before a simulation or a real replacement. */
export function ConfirmDialog({
  mode,
  summary,
  onConfirm,
  onClose,
}: {
  mode: "simulate" | "apply";
  summary: SelectionSummary;
  onConfirm: () => void;
  onClose: () => void;
}) {
  const titleId = useId();
  const [understood, setUnderstood] = useState(false);
  const [zfs, setZfs] = useState<DedupeZfsInfo | null>(null);
  const apply = mode === "apply";

  useEffect(() => {
    api
      .getDedupeSpace()
      .then(setZfs)
      .catch(() => setZfs(null));
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div className="dialog-backdrop" onClick={onClose}>
      <div
        className="folder-dialog dedupe-confirm"
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="folder-dialog-header">
          <h2 id={titleId}>{apply ? "Replace with hardlinks" : "Simulate"}</h2>
          <button className="icon-button" type="button" onClick={onClose} aria-label="Close">
            ×
          </button>
        </div>
        <div className="folder-dialog-body">
          <p>
            <strong>{formatCount(summary.files)}</strong> duplicate file(s) in{" "}
            <strong>{formatCount(summary.groups)}</strong> group(s), up to{" "}
            <strong>{formatBytes(summary.bytes)}</strong> to free.
          </p>
          {apply ? (
            <>
              <p className="error">
                This cannot be undone. Each duplicate path will point to the kept file and the old
                copy is released. The history keeps a record, but not the data.
              </p>
              <ul className="dedupe-confirm-list">
                <li>
                  Right before each file: its size, inode and modification time are checked again,
                  then it is compared byte for byte with the kept file. Anything that differs is
                  skipped with the reason.
                </li>
                <li>
                  Replacement is atomic (link to a temporary name, then rename over the duplicate):
                  the path never disappears, and nothing is ever copied.
                </li>
                <li>
                  Linked paths share the kept file&apos;s owner, permissions, modification time and
                  extended attributes.
                  {summary.ownersDiffer > 0 &&
                    ` ${formatCount(summary.ownersDiffer)} selected group(s) have different owners.`}
                </li>
                <li>
                  Files still open (for example seeding in qBittorrent) keep their old copy until
                  they are closed, so that space comes back later.
                </li>
                <li>
                  ZFS only releases the space once the snapshots that still hold the old copies
                  expire.
                  {zfs?.available && zfs.datasets.length > 0
                    ? ` Space held by snapshots now: ${zfs.datasets
                        .map((d) => `${d.name} ${formatBytes(d.used_by_snapshots)}`)
                        .join(", ")}.`
                    : " (The zfs command is not available here, so snapshot usage cannot be shown; check it on the host with zfs list -o name,usedbysnapshots.)"}
                </li>
                <li>Stopping is possible at any time; it takes effect between two files.</li>
              </ul>
              <label className="checkbox-label">
                <input
                  type="checkbox"
                  checked={understood}
                  onChange={(e) => setUnderstood(e.target.checked)}
                />
                I understand this is permanent
              </label>
            </>
          ) : (
            <p className="muted">
              A simulation re-checks every file (still there, unchanged, folder writable) and lists
              what would happen, including owner and permission changes. Nothing on disk is modified
              and no file content is read.
            </p>
          )}
          <div className="toolbar">
            <button
              type="button"
              className={apply ? "danger-button" : undefined}
              disabled={apply && !understood}
              onClick={onConfirm}
            >
              {apply ? "Replace now" : "Run simulation"}
            </button>
            <button className="secondary-button" type="button" onClick={onClose}>
              Cancel
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

/** Live view of the latest simulation / replacement job. */
export function JobPanel({ jobId, onFinished }: { jobId: string | null; onFinished: () => void }) {
  const [job, setJob] = useState<DedupeJob | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    let timer: number | null = null;
    let wasRunning = false;
    const load = () => {
      const request = jobId ? api.getDedupeJob(jobId) : api.getLatestDedupeJob();
      request
        .then((next) => {
          if (cancelled) return;
          setJob(next);
          setError(null);
          if (next?.state === "running") {
            wasRunning = true;
            timer = window.setTimeout(load, 1000);
          } else if (wasRunning) {
            wasRunning = false;
            onFinished();
          }
        })
        .catch((e) => !cancelled && setError(errorText(e)));
    };
    load();
    return () => {
      cancelled = true;
      if (timer != null) window.clearTimeout(timer);
    };
  }, [jobId, onFinished]);

  if (error) return <p className="error">{error}</p>;
  if (!job) return null;
  const running = job.state === "running";
  const percent =
    job.bytes_to_compare > 0
      ? Math.min(100, Math.floor((job.bytes_compared / job.bytes_to_compare) * 100))
      : job.paths_total > 0
        ? Math.floor((job.paths_done / job.paths_total) * 100)
        : 0;
  const problems = job.recent.filter((r) => r.result === "skipped" || r.result === "failed");
  const notes = job.recent.filter((r) => r.result === "simulated" && r.reason);

  return (
    <div className="match-group dedupe-job">
      <div className="group-header">
        <div className="group-show">
          <strong>{job.mode === "apply" ? "Replacement" : "Simulation"}</strong>
          <span
            className={`state-badge state-${running ? "running" : job.failed ? "failed" : "done"}`}
          >
            {job.state}
          </span>
          <span className="muted">
            {job.groups_done}/{job.groups_total} groups · {job.paths_done}/{job.paths_total} files
          </span>
          {running && (
            <button
              className="secondary-button danger"
              type="button"
              onClick={() => api.cancelDedupeJob(job.id).catch(() => undefined)}
            >
              Stop after the current file
            </button>
          )}
        </div>
      </div>
      {running && (
        <div className="progress-track">
          <div className="progress-bar" style={{ width: `${percent}%` }} />
        </div>
      )}
      {running && job.current && <p className="muted dedupe-current">{job.current}</p>}
      <dl className="dedupe-stats">
        {job.mode === "apply" ? (
          <>
            <div>
              <dt>Replaced</dt>
              <dd>{formatCount(job.replaced)}</dd>
            </div>
            <div>
              <dt>Freed</dt>
              <dd>{formatBytes(job.freed_bytes)}</dd>
            </div>
            <div>
              <dt>Compared</dt>
              <dd>
                {formatBytes(job.bytes_compared)} / {formatBytes(job.bytes_to_compare)}
              </dd>
            </div>
          </>
        ) : (
          <>
            <div>
              <dt>Would replace</dt>
              <dd>{formatCount(job.simulated)}</dd>
            </div>
            <div>
              <dt>Would free</dt>
              <dd>{formatBytes(job.expected_freed_bytes)}</dd>
            </div>
          </>
        )}
        <div>
          <dt>Skipped</dt>
          <dd>{formatCount(job.skipped)}</dd>
        </div>
        <div>
          <dt>Failed</dt>
          <dd>{formatCount(job.failed)}</dd>
        </div>
      </dl>
      {job.error && <p className="error">{job.error}</p>}
      {job.mode === "apply" && job.replaced > 0 && !running && (
        <p className="muted">
          Space returns once files held open are closed and the ZFS snapshots holding the old copies
          expire.
        </p>
      )}
      {(problems.length > 0 || notes.length > 0) && (
        <table>
          <thead>
            <tr>
              <th>Result</th>
              <th>Path</th>
              <th>Reason / note</th>
            </tr>
          </thead>
          <tbody>
            {[...problems, ...notes].slice(-50).map((r, i) => (
              <tr
                key={i}
                className={`state-row state-${r.result === "failed" ? "failed" : "skipped"}`}
              >
                <td className="nowrap-cell">{r.result}</td>
                <td className="path-cell">{r.path}</td>
                <td>{r.reason}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

const HISTORY_PAGE = 200;

export function HistoryTab({ refreshKey }: { refreshKey: number }) {
  const [data, setData] = useState<DedupeHistoryPage | null>(null);
  const [mode, setMode] = useState("apply");
  const [offset, setOffset] = useState(0);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .getDedupeHistory(HISTORY_PAGE, offset, mode || undefined)
      .then((next) => {
        setData(next);
        setError(null);
      })
      .catch((e) => setError(errorText(e)));
  }, [mode, offset, refreshKey]);

  return (
    <div className="dedupe-history">
      <p className="muted">
        Every simulated or performed replacement, with the file&apos;s state before and after. A
        replacement cannot be undone: the old copy is released, which is why each one is verified
        right before it happens.
      </p>
      <div className="toolbar">
        <label className="toolbar-field">
          Show
          <select
            value={mode}
            onChange={(e) => {
              setMode(e.target.value);
              setOffset(0);
            }}
          >
            <option value="apply">replacements</option>
            <option value="simulate">simulations</option>
            <option value="">everything</option>
          </select>
        </label>
        {data && (
          <span className="muted">
            {formatCount(data.total)} entries · {formatBytes(data.total_freed)} freed in total
          </span>
        )}
      </div>
      {error && <p className="error">{error}</p>}
      {data && data.rows.length === 0 && <p className="muted">Nothing yet.</p>}
      {data && data.rows.length > 0 && (
        <table className="dedupe-history">
          <thead>
            <tr>
              <th>When</th>
              <th>Result</th>
              <th>Path</th>
              <th>Before</th>
              <th>After</th>
              <th>Freed</th>
              <th>Reason</th>
            </tr>
          </thead>
          <tbody>
            {data.rows.map((row) => (
              <tr
                key={row.id}
                className={`state-row state-${row.result === "replaced" ? "imported" : row.result === "failed" ? "failed" : "skipped"}`}
              >
                <td>{formatDbTime(row.at)}</td>
                <td>{row.result}</td>
                <td className="path-cell">
                  <div>{row.path}</div>
                  {row.keeper_path && <div className="muted">→ {row.keeper_path}</div>}
                </td>
                <td>
                  ino {row.before_ino ?? "-"} · {row.before_owner ?? "-"} · {row.before_mode ?? "-"}
                  {row.before_size != null ? ` · ${formatBytes(row.before_size)}` : ""}
                </td>
                <td>
                  {row.after_ino != null
                    ? `ino ${row.after_ino} · ${row.keeper_owner ?? "-"}`
                    : "-"}
                </td>
                <td className="nowrap-cell">
                  {row.freed_bytes > 0 ? formatBytes(row.freed_bytes) : "-"}
                </td>
                <td>{row.reason}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {data && data.total > HISTORY_PAGE && (
        <div className="toolbar dedupe-pager">
          <button
            className="secondary-button"
            type="button"
            disabled={offset === 0}
            onClick={() => setOffset(Math.max(0, offset - HISTORY_PAGE))}
          >
            Previous
          </button>
          <span className="muted">
            {offset + 1}–{Math.min(offset + HISTORY_PAGE, data.total)} of {formatCount(data.total)}
          </span>
          <button
            className="secondary-button"
            type="button"
            disabled={offset + HISTORY_PAGE >= data.total}
            onClick={() => setOffset(offset + HISTORY_PAGE)}
          >
            Next
          </button>
        </div>
      )}
    </div>
  );
}
