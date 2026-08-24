import { useEffect, useState } from "react";
import { api, ApiError } from "../api";
import { useProgress } from "../components/Progress";
import FolderPicker from "../components/FolderPicker";
import type { PermissionCheck } from "../types";

type PickerTarget =
  | { kind: "input" }
  | { kind: "tv" }
  | { kind: "anime" }
  | { kind: "film" }
  | { kind: "music" };

export default function SettingsPage() {
  const progress = useProgress();
  const [inputRootsText, setInputRootsText] = useState("");
  const [tvRoot, setTvRoot] = useState("");
  const [animeRoot, setAnimeRoot] = useState("");
  const [filmRoot, setFilmRoot] = useState("");
  const [musicRoot, setMusicRoot] = useState("");
  const [copyRate, setCopyRate] = useState("15");
  const [checks, setChecks] = useState<PermissionCheck[]>([]);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [picker, setPicker] = useState<PickerTarget | null>(null);

  function load() {
    api
      .getSettings()
      .then((settings) => {
        setInputRootsText(settings.input_roots.join("\n"));
        setTvRoot(settings.tv_output_root);
        setAnimeRoot(settings.anime_output_root);
        setFilmRoot(settings.film_output_root);
        setMusicRoot(settings.music_output_root);
        setCopyRate(settings.copy_rate_limit_mbps);
        setChecks(settings.checks);
        setError(null);
      })
      .catch((e) => setError(e instanceof ApiError ? e.message : String(e)));
  }

  useEffect(load, []);

  async function save() {
    setSaved(false);
    // Clear any previous failure, otherwise a stale error banner renders
    // alongside "Settings saved." once the retry succeeds.
    setError(null);
    progress.startDelayed("Saving settings...");
    try {
      await api.saveSettings({
        input_roots: inputRootsText
          .split("\n")
          .map((line) => line.trim())
          .filter(Boolean),
        tv_output_root: tvRoot,
        anime_output_root: animeRoot,
        film_output_root: filmRoot,
        music_output_root: musicRoot,
        copy_rate_limit_mbps: copyRate,
      });
      setSaved(true);
      load();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : "Could not save settings.");
    } finally {
      progress.hide();
    }
  }

  function pickerInitialPath(): string {
    if (!picker) return "/mnt";
    if (picker.kind === "input") {
      return inputRootsText.split("\n").map((l) => l.trim()).find(Boolean) || "/mnt";
    }
    if (picker.kind === "tv") return tvRoot || "/mnt";
    if (picker.kind === "anime") return animeRoot || "/mnt";
    if (picker.kind === "film") return filmRoot || "/mnt";
    return musicRoot || "/mnt";
  }

  function handleChoose(path: string) {
    if (!picker) return;
    if (picker.kind === "input") {
      const existing = inputRootsText.split("\n").map((l) => l.trim()).filter(Boolean);
      if (!existing.includes(path)) existing.push(path);
      setInputRootsText(existing.join("\n"));
    } else if (picker.kind === "tv") {
      setTvRoot(path);
    } else if (picker.kind === "anime") {
      setAnimeRoot(path);
    } else if (picker.kind === "film") {
      setFilmRoot(path);
    } else {
      setMusicRoot(path);
    }
    setPicker(null);
  }

  return (
    <section className="panel">
      <h1>Settings</h1>
      {error && <p className="error">{error}</p>}
      {saved && <p className="muted">Settings saved.</p>}

      <div className="stack">
        <label>
          Input roots
          <textarea
            rows={6}
            value={inputRootsText}
            placeholder="/mnt/downloads"
            onChange={(e) => setInputRootsText(e.target.value)}
          />
        </label>
        <button className="secondary-button" type="button" onClick={() => setPicker({ kind: "input" })}>
          Browse Input Root
        </button>

        <label>
          TV output root
          <input value={tvRoot} placeholder="/mnt/media/TV" onChange={(e) => setTvRoot(e.target.value)} />
        </label>
        <button className="secondary-button" type="button" onClick={() => setPicker({ kind: "tv" })}>
          Browse TV Output Root
        </button>

        <label>
          Anime output root
          <input
            value={animeRoot}
            placeholder="/mnt/media/Anime"
            onChange={(e) => setAnimeRoot(e.target.value)}
          />
        </label>
        <button className="secondary-button" type="button" onClick={() => setPicker({ kind: "anime" })}>
          Browse Anime Output Root
        </button>

        <label>
          Film output root
          <input
            value={filmRoot}
            placeholder="/mnt/media/Films"
            onChange={(e) => setFilmRoot(e.target.value)}
          />
        </label>
        <button className="secondary-button" type="button" onClick={() => setPicker({ kind: "film" })}>
          Browse Film Output Root
        </button>

        <label>
          Music output root
          <input
            value={musicRoot}
            placeholder="/mnt/media/Music"
            onChange={(e) => setMusicRoot(e.target.value)}
          />
        </label>
        <button className="secondary-button" type="button" onClick={() => setPicker({ kind: "music" })}>
          Browse Music Output Root
        </button>

        <label>
          Copy speed limit (Mo/s)
          <input
            type="number"
            min={0}
            step={1}
            value={copyRate}
            placeholder="15"
            onChange={(e) => setCopyRate(e.target.value)}
          />
        </label>
        <p className="muted">
          Use 0 for no copy limit. A conservative limit protects shared Proxmox storage from IO stalls
          during large batch copies.
        </p>

        <button type="button" onClick={save}>
          Save Settings
        </button>
      </div>

      <h2>Permission Checks</h2>
      {checks.length > 0 ? (
        <table>
          <thead>
            <tr>
              <th>Path</th>
              <th>Exists</th>
              <th>Read</th>
              <th>Write</th>
            </tr>
          </thead>
          <tbody>
            {checks.map((check, i) => (
              <tr key={i}>
                <td>{check.label}</td>
                <td>{check.exists ? "yes" : "no"}</td>
                <td>{check.read ? "yes" : "no"}</td>
                <td>{check.write == null ? "-" : check.write ? "yes" : "no"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : (
        <p className="muted">No paths configured yet.</p>
      )}

      {picker && (
        <FolderPicker
          initialPath={pickerInitialPath()}
          onChoose={handleChoose}
          onClose={() => setPicker(null)}
        />
      )}
    </section>
  );
}
