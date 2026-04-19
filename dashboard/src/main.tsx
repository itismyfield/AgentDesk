import React from "react";
import ReactDOM from "react-dom/client";
import { BrowserRouter } from "react-router-dom";
import App from "./App";
import { OverlayProvider } from "./components/common/overlay";
import {
  DEFAULT_ACCENT_PRESET,
  applyThemeAccentDataset,
  readStoredAccentPreset,
  readStoredThemePreference,
  resolveThemePreference,
} from "./app/themePreferences";
import "./styles/main.css";

const prefersDarkScheme = window.matchMedia("(prefers-color-scheme: dark)").matches;
const initialThemePreference = readStoredThemePreference(window.localStorage, "dark");
const initialAccentPreset = readStoredAccentPreset(window.localStorage, DEFAULT_ACCENT_PRESET);

applyThemeAccentDataset(
  document.documentElement,
  resolveThemePreference(initialThemePreference, prefersDarkScheme),
  initialAccentPreset,
);

// Recover from stale lazy chunks after deploy: detect load failures and reload once
window.addEventListener("error", (e) => {
  if (
    e.message?.includes("Failed to fetch dynamically imported module") ||
    e.message?.includes("Loading chunk") ||
    e.message?.includes("Loading CSS chunk")
  ) {
    const key = "chunk-reload";
    if (!sessionStorage.getItem(key)) {
      sessionStorage.setItem(key, "1");
      window.location.reload();
    }
  }
});

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <BrowserRouter>
      <OverlayProvider>
        <App />
      </OverlayProvider>
    </BrowserRouter>
  </React.StrictMode>,
);
