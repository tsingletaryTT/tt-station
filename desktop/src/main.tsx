// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "@fontsource/inter-tight/400.css";
import "@fontsource/inter-tight/500.css";
import "@fontsource/inter-tight/600.css";
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/500.css";
import "@tenstorrent/vesper/styles.css";
import "./app.css";
import { App } from "./App";

// The color theme chosen in Settings (Vesper's switcher sets the attribute on
// the root); dark until one is chosen.
const THEME_KEY = "tt-station.theme";
try {
  const saved = localStorage.getItem(THEME_KEY);
  if (saved === "light" || saved === "dark" || saved === "system") document.documentElement.dataset.vesperTheme = saved;
} catch { /* storage refused: stay dark */ }
new MutationObserver(() => {
  try { localStorage.setItem(THEME_KEY, document.documentElement.dataset.vesperTheme ?? "dark"); } catch { /* not remembered */ }
}).observe(document.documentElement, { attributes: true, attributeFilter: ["data-vesper-theme"] });

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
