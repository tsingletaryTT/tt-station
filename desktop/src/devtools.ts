// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Developer tools: extra detail for people who work on the chips themselves
// (who holds which chips, on Hardware). Off unless turned on in Settings →
// General → This app; the choice is remembered.

import { useSyncExternalStore } from "react";

export const DEV_KEY = "tt.devtools";

export type DevChoice = "on" | "off" | null;

/** The remembered choice, if one was made. */
export function storedChoice(storage: Pick<Storage, "getItem"> | null = safeStorage()): DevChoice {
  try {
    const v = storage?.getItem(DEV_KEY);
    return v === "on" || v === "off" ? v : null;
  } catch { return null; }
}

function safeStorage(): Storage | null {
  try { return typeof localStorage === "undefined" ? null : localStorage; } catch { return null; }
}

// ---------------------------------------------------------------- the live setting

const subs = new Set<() => void>();
let current = storedChoice() === "on";

/** Turn developer tools on or off, and remember it. */
export function setDevTools(on: boolean) {
  try { localStorage.setItem(DEV_KEY, on ? "on" : "off"); } catch { /* not remembered */ }
  current = on;
  subs.forEach((fn) => fn());
}

export function useDevTools(): boolean {
  return useSyncExternalStore(
    (fn) => { subs.add(fn); return () => subs.delete(fn); },
    () => current,
    () => current,
  );
}
