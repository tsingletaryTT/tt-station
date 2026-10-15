// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The color theme chosen in Settings. Vesper's switcher sets the attribute on
// the root; this remembers it across launches. Dark until one is chosen.

export const THEME_KEY = "tt-station.theme";

/** Puts the saved theme on the root, if there is one and it's one we know. */
export function loadTheme(root: HTMLElement = document.documentElement) {
  try {
    const saved = localStorage.getItem(THEME_KEY);
    if (saved === "light" || saved === "dark" || saved === "system") root.dataset.vesperTheme = saved;
  } catch { /* storage refused: stay dark */ }
}

/** Saves every change the switcher makes to the root's theme. */
export function persistTheme(root: HTMLElement = document.documentElement): MutationObserver {
  const watch = new MutationObserver(() => {
    try { localStorage.setItem(THEME_KEY, root.dataset.vesperTheme ?? "dark"); } catch { /* not remembered */ }
  });
  watch.observe(root, { attributes: true, attributeFilter: ["data-vesper-theme"] });
  return watch;
}
