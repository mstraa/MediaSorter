import { useState } from "react";
import { NavLink, Navigate, Route, Routes } from "react-router-dom";
import { ProgressProvider } from "./components/Progress";
import { ImportsProvider, useImports } from "./components/ImportsContext";
import BrowsePage from "./pages/Browse";
import MatchPage from "./pages/Match";
import ImportsPage from "./pages/Imports";
import ResultsPage from "./pages/Results";
import HistoryPage from "./pages/History";
import SettingsPage from "./pages/Settings";
import { toggleTheme, type Theme } from "./theme";

const NAV = [
  { to: "/browse", label: "Browse", hint: "input" },
  { to: "/imports", label: "Imports", hint: "jobs" },
  { to: "/history", label: "History", hint: "log" },
  { to: "/settings", label: "Settings", hint: "config" },
];

function ImportsNavLink() {
  const { jobs, activeCount } = useImports();
  const active = jobs.filter((job) => job.active);
  const percent = active.length
    ? Math.round(active.reduce((sum, job) => sum + job.percent, 0) / active.length)
    : 0;
  return (
    <NavLink to="/imports" className="nav-link nav-link-imports">
      <span>
        Imports
        {activeCount > 0 && <span className="nav-badge">{activeCount}</span>}
      </span>
      <strong>jobs</strong>
      {activeCount > 0 && (
        <div className="nav-progress">
          <span style={{ width: `${percent}%` }} />
        </div>
      )}
    </NavLink>
  );
}

export default function App() {
  const [theme, setTheme] = useState<Theme>(
    (document.documentElement.dataset.theme as Theme) || "dark",
  );

  return (
    <ProgressProvider>
      <ImportsProvider>
      <div className="terminal-shell">
        <aside className="sidebar">
          <div className="brand">
            <span className="prompt-mark">▸</span>
            <div>
              <h1>MediaSorter</h1>
              <p className="eyebrow">tv · anime · film · music</p>
            </div>
          </div>

          <nav className="filter-list">
            {NAV.map((item) =>
              item.to === "/imports" ? (
                <ImportsNavLink key={item.to} />
              ) : (
                <NavLink key={item.to} to={item.to} className="nav-link">
                  <span>{item.label}</span>
                  <strong>{item.hint}</strong>
                </NavLink>
              ),
            )}
          </nav>

          <div className="sidebar-bottom">
            <button
              className="secondary-button theme-toggle"
              type="button"
              onClick={() => setTheme(toggleTheme(theme))}
            >
              theme: {theme}
            </button>
          </div>
        </aside>

        <main className="workspace">
          <Routes>
            <Route path="/" element={<Navigate to="/browse" replace />} />
            <Route path="/browse" element={<BrowsePage />} />
            <Route path="/match" element={<MatchPage />} />
            <Route path="/imports" element={<ImportsPage />} />
            <Route path="/results/:jobId" element={<ResultsPage />} />
            <Route path="/history" element={<HistoryPage />} />
            <Route path="/settings" element={<SettingsPage />} />
            <Route path="*" element={<Navigate to="/browse" replace />} />
          </Routes>
        </main>
      </div>
      </ImportsProvider>
    </ProgressProvider>
  );
}
