import { useState } from "react";
import { Link } from "react-router-dom";
import { api, ApiError } from "../api";
import { useImports } from "../components/ImportsContext";
import { formatBytes } from "../theme";

// Hardlink and dry-run items are counted as whole items, not bytes; rendering
// those through formatBytes produced nonsense like "0 o / 12 o".
function formatProgress(value: number, unit: string): string {
  if (unit === "bytes") return formatBytes(value);
  if (unit === "items") return `${value} item${value === 1 ? "" : "s"}`;
  return String(value);
}
import type { JobItem, JobSnapshot } from "../types";

const TERMINAL_ITEM_STATES = new Set(["imported", "skipped", "preview", "failed", "cancelled"]);

function itemIsActive(item: JobItem): boolean {
  return !TERMINAL_ITEM_STATES.has(item.status);
}

function baseName(path: string): string {
  if (!path) return "";
  const parts = path.split(/[/\\]/);
  return parts[parts.length - 1] ?? "";
}

export default function ImportsPage() {
  const { jobs, refresh } = useImports();

  const clearableCount = jobs.filter(
    (job) => job.state === "done" && job.failed_items === 0 && job.cancelled_items === 0,
  ).length;

  async function cancelJob(id: string) {
    try {
      await api.cancelImportJob(id);
    } finally {
      refresh();
    }
  }

  async function clearImported() {
    try {
      await api.clearImportJobs();
    } finally {
      refresh();
    }
  }

  // Re-run skipped files with the "replace" policy. The backend clears each
  // file's `forceable` flag as it enqueues it, so a second click is harmless;
  // `forcing` only stops a click from landing while the first is in flight.
  const [forcing, setForcing] = useState(false);
  async function forceItems(id: string, indexes?: number[]) {
    if (forcing) return;
    setForcing(true);
    try {
      await api.forceImportJob(id, indexes);
    } catch (e) {
      alert(e instanceof ApiError ? e.message : "Could not force the import.");
    } finally {
      setForcing(false);
      refresh();
    }
  }

  async function cancelItem(id: string, index: number) {
    try {
      await api.cancelImportItem(id, index);
    } finally {
      refresh();
    }
  }

  return (
    <section className="panel">
      <div className="imports-header">
        <h1>Imports</h1>
        {clearableCount > 0 && (
          <button className="secondary-button" type="button" onClick={clearImported}>
            Clear imported ({clearableCount})
          </button>
        )}
      </div>
      {jobs.length === 0 ? (
        <p className="muted">No imports yet. Start one from the Match queue.</p>
      ) : (
        jobs.map((job) => (
          <JobCard
            key={job.id}
            job={job}
            onCancelJob={() => cancelJob(job.id)}
            onCancelItem={(index) => cancelItem(job.id, index)}
            onForce={(indexes) => forceItems(job.id, indexes)}
            forcing={forcing}
          />
        ))
      )}
    </section>
  );
}

interface JobCardProps {
  job: JobSnapshot;
  onCancelJob: () => void;
  onCancelItem: (index: number) => void;
  /** Replace existing destinations; no indexes = every forceable file. */
  onForce: (indexes?: number[]) => void;
  forcing: boolean;
}

function JobCard({ job, onCancelJob, onCancelItem, onForce, forcing }: JobCardProps) {
  const forceableCount = job.items.filter((item) => item.forceable).length;
  const stateLabel =
    job.state === "running" ? "running" : job.state === "cancelled" ? "cancelled" : "done";
  return (
    <div className="match-group import-job">
      <div className="group-header">
        <div className="group-show">
          <span className="import-job-title">{job.label}</span>
          <span className={`state-badge state-${stateLabel}`}>{stateLabel}</span>
          <span className="muted import-job-counts">
            {job.completed_items} done
            {job.failed_items > 0 ? ` · ${job.failed_items} failed` : ""}
            {job.cancelled_items > 0 ? ` · ${job.cancelled_items} cancelled` : ""} / {job.total_items}
          </span>
          {job.active && (
            <button className="secondary-button danger" type="button" onClick={onCancelJob}>
              Cancel all
            </button>
          )}
          {!job.active && (
            <Link className="secondary-button" to={`/results/${job.id}`}>
              View results
            </Link>
          )}
          {forceableCount > 0 && (
            <button
              className="secondary-button"
              type="button"
              disabled={forcing}
              title="Import the skipped files again, replacing the files already in the library"
              onClick={() => onForce()}
            >
              Force all skipped ({forceableCount})
            </button>
          )}
        </div>
        <div className="import-progress">
          <div className="import-progress-bar">
            <span style={{ width: `${job.percent}%` }} />
          </div>
          <span className="muted import-progress-text">
            {job.percent}% · {formatProgress(job.completed, job.unit)} /{" "}
            {formatProgress(job.total, job.unit)}
          </span>
        </div>
      </div>

      {job.error && <p className="error group-error">{job.error}</p>}

      <table className="episode-table import-items">
        <thead>
          <tr>
            <th className="col-status">Status</th>
            <th className="col-title">Source</th>
            <th className="col-dest">New name</th>
            <th className="col-progress">Progress</th>
            <th className="col-cancel" aria-label="Cancel" />
          </tr>
        </thead>
        <tbody>
          {job.items.map((item) => (
            <tr key={item.index} className="import-item-row">
              <td className="col-status">
                <span className={`state-badge state-${item.status}`}>{item.status}</span>
              </td>
              <td className="col-title">
                {item.name}
                {item.error && <span className="muted item-error"> — {item.error}</span>}
              </td>
              <td className="col-dest muted">{baseName(item.destination) || "—"}</td>
              <td className="col-progress muted">
                {item.status === "running" && item.total > 0
                  ? `${formatProgress(item.bytes, item.unit)} / ${formatProgress(item.total, item.unit)}`
                  : item.status === "imported"
                    ? formatProgress(item.total, item.unit)
                    : ""}
              </td>
              <td className="col-cancel">
                {item.forceable && (
                  <button
                    className="secondary-button force-button"
                    type="button"
                    disabled={forcing}
                    title="Replace the file already in the library with this one"
                    onClick={() => onForce([item.index])}
                  >
                    Force
                  </button>
                )}
                {itemIsActive(item) && job.active && (
                  <button
                    className="icon-cancel"
                    type="button"
                    title="Cancel this file"
                    aria-label="Cancel this file"
                    onClick={() => onCancelItem(item.index)}
                  >
                    ✕
                  </button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
