import { useEffect, useState } from "react";
import { api, ApiError } from "../api";
import { useProgress } from "./Progress";
import FolderPicker from "./FolderPicker";
import type { DedupeRootsReport, DedupeSettings } from "../types";

type Field = Exclude<keyof DedupeSettings, "roots" | "torrent_roots" | "excludes">;

function toLines(text: string): string[] {
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);
}

/** The "Duplicates" block of the Settings page. Saved separately from the
 *  import settings: it has its own endpoint and validation. */
export default function DedupeSettingsSection() {
  const progress = useProgress();
  const [roots, setRoots] = useState("");
  const [torrentRoots, setTorrentRoots] = useState("");
  const [excludes, setExcludes] = useState("");
  const [fields, setFields] = useState<Record<Field, string>>({
    min_size_mib: "",
    min_age_minutes: "",
    stat_rate: "",
    read_rate_mbps: "",
    schedule: "",
    preferred_owner: "",
  });
  const [report, setReport] = useState<DedupeRootsReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [picker, setPicker] = useState<"roots" | "torrents" | null>(null);

  function apply(settings: DedupeSettings, nextReport: DedupeRootsReport) {
    setRoots(settings.roots.join("\n"));
    setTorrentRoots(settings.torrent_roots.join("\n"));
    setExcludes(settings.excludes.join("\n"));
    setFields({
      min_size_mib: settings.min_size_mib,
      min_age_minutes: settings.min_age_minutes,
      stat_rate: settings.stat_rate,
      read_rate_mbps: settings.read_rate_mbps,
      schedule: settings.schedule,
      preferred_owner: settings.preferred_owner,
    });
    setReport(nextReport);
  }

  useEffect(() => {
    api
      .getDedupeSettings()
      .then((view) => apply(view.settings, view.report))
      .catch((e) => setError(e instanceof ApiError ? e.message : String(e)));
  }, []);

  async function save() {
    setSaved(false);
    setError(null);
    progress.startDelayed("Saving duplicate settings...");
    try {
      const view = await api.saveDedupeSettings({
        roots: toLines(roots),
        torrent_roots: toLines(torrentRoots),
        excludes: toLines(excludes),
        ...fields,
      });
      apply(view.settings, view.report);
      setSaved(true);
    } catch (e) {
      setError(e instanceof ApiError ? e.message : "Could not save duplicate settings.");
    } finally {
      progress.hide();
    }
  }

  function setField(field: Field, value: string) {
    setFields((current) => ({ ...current, [field]: value }));
  }

  function handleChoose(path: string) {
    const [text, setText] = picker === "torrents" ? [torrentRoots, setTorrentRoots] : [roots, setRoots];
    const lines = toLines(text);
    if (!lines.includes(path)) lines.push(path);
    setText(lines.join("\n"));
    setPicker(null);
  }

  return (
    <>
      <h2>Duplicates</h2>
      <p className="muted">
        Folders scanned for duplicate files. They are separate from the input roots and must all be on
        the same filesystem (same st_dev) for hardlinks to work.
      </p>
      {error && <p className="error">{error}</p>}
      {saved && <p className="muted">Duplicate settings saved.</p>}

      <div className="stack">
        <label>
          Dedupe roots (one per line)
          <textarea
            rows={5}
            value={roots}
            placeholder={"/mnt/data/Torrents\n/mnt/data/Tv Show\n/mnt/data/Films"}
            onChange={(e) => setRoots(e.target.value)}
          />
        </label>
        <button className="secondary-button" type="button" onClick={() => setPicker("roots")}>
          Browse Dedupe Root
        </button>

        <label>
          Torrent folders (for “Unused torrents”)
          <textarea
            rows={2}
            value={torrentRoots}
            placeholder="/mnt/data/Torrents"
            onChange={(e) => setTorrentRoots(e.target.value)}
          />
        </label>
        <button className="secondary-button" type="button" onClick={() => setPicker("torrents")}>
          Browse Torrent Folder
        </button>

        <label>
          Excluded patterns (one per line)
          <textarea rows={6} value={excludes} onChange={(e) => setExcludes(e.target.value)} />
        </label>
        <p className="muted">
          A trailing / matches folders only (Torrent-work/). A pattern with a / is matched against the
          path relative to the root; otherwise against the file or folder name (*.part).
        </p>

        <label>
          Minimum file size (Mio)
          <input
            type="number"
            min={0}
            value={fields.min_size_mib}
            onChange={(e) => setField("min_size_mib", e.target.value)}
          />
        </label>
        <label>
          Minimum age (minutes since last modification)
          <input
            type="number"
            min={0}
            value={fields.min_age_minutes}
            onChange={(e) => setField("min_age_minutes", e.target.value)}
          />
        </label>
        <label>
          Scan speed limit (stat calls per second)
          <input
            type="number"
            min={0}
            value={fields.stat_rate}
            onChange={(e) => setField("stat_rate", e.target.value)}
          />
        </label>
        <label>
          Hash read limit (Mo/s)
          <input
            type="number"
            min={0}
            step="any"
            value={fields.read_rate_mbps}
            onChange={(e) => setField("read_rate_mbps", e.target.value)}
          />
        </label>
        <p className="muted">
          0 = unlimited. On a raidz of slow HDDs keep both limits low: a fast walk saturates the pool
          and stalls every other container.
        </p>
        <label>
          Automatic scan window (optional)
          <input
            value={fields.schedule}
            placeholder="01:00-07:00"
            onChange={(e) => setField("schedule", e.target.value)}
          />
        </label>
        <p className="muted">
          Inside this window a scan starts (or resumes) on its own, at most once a day, and stops
          (keeping its progress) when the window closes. Empty = manual scans only.
        </p>
        <label>
          Preferred owner of the kept file (uid:gid)
          <input
            value={fields.preferred_owner}
            placeholder="100000:100000"
            onChange={(e) => setField("preferred_owner", e.target.value)}
          />
        </label>

        <button type="button" onClick={save}>
          Save Duplicate Settings
        </button>
      </div>

      {report && report.roots.length > 0 && (
        <>
          {report.warnings.map((warning, i) => (
            <p key={i} className={!report.same_device && i === 0 ? "error" : "warning-note"}>
              {warning}
            </p>
          ))}
          <table className="dedupe-root-table">
            <thead>
              <tr>
                <th>Root</th>
                <th>Exists</th>
                <th>Read</th>
                <th>Device</th>
              </tr>
            </thead>
            <tbody>
              {report.roots.map((root) => (
                <tr key={root.path}>
                  <td>
                    {root.path}
                    {root.nested ? " (inside another root)" : ""}
                  </td>
                  <td>{root.exists ? "yes" : "no"}</td>
                  <td>{root.readable ? "yes" : "no"}</td>
                  <td>{root.device ?? "-"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </>
      )}

      {picker && (
        <FolderPicker
          initialPath={toLines(picker === "torrents" ? torrentRoots : roots)[0] || "/mnt"}
          onChoose={handleChoose}
          onClose={() => setPicker(null)}
        />
      )}
    </>
  );
}
