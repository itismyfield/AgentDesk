import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./styles/main.css";

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
    <App />
  </React.StrictMode>,
);
