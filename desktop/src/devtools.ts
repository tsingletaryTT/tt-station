// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Developer tools: the places and cards for people who port models and work on
// the chips themselves (Bring-up, the silicon check, who holds the chips). Off for a new install, so the app opens on what most people need;
// on by itself for someone who has used bring-up here before. Settings →
// General → This app turns it either way, and the choice is remembered.

import { useSyncExternalStore } from "react";

export const DEV_KEY = "tt.devtools";
/** Set by Bring-up when it's used: a cheap sign someone is a developer. */
const USED_HINTS = ["tt.bringup.way"];

/** Places and cards only shown with developer tools on. */
export const DEV_ROUTES = ["bringup", "orchard"] as const;

export type DevChoice = "on" | "off" | null;

/** The remembered choice, if one was made. */
export function storedChoice(storage: Pick<Storage, "getItem"> | null = safeStorage()): DevChoice {
  try {
    const v = storage?.getItem(DEV_KEY);
    return v === "on" || v === "off" ? v : null;
  } catch { return null; }
}

/** Whether this app has seen bring-up used before (from its own remembered state). */
export function usedBefore(storage: Pick<Storage, "getItem"> | null = safeStorage()): boolean {
  try { return USED_HINTS.some((k) => !!storage?.getItem(k)); } catch { return false; }
}

/**
 * On or off: a choice made in Settings wins; otherwise on for someone who has
 * used bring-up (here, or runs the box reports), off for everyone else.
 */
export function resolveDevTools(choice: DevChoice, used: boolean, runsOnBox: boolean): boolean {
  if (choice) return choice === "on";
  return used || runsOnBox;
}

/** Whether a sidebar place shows. */
export function placeVisible(route: string, devTools: boolean): boolean {
  return devTools || !(DEV_ROUTES as readonly string[]).includes(route);
}

function safeStorage(): Storage | null {
  try { return typeof localStorage === "undefined" ? null : localStorage; } catch { return null; }
}

// ---------------------------------------------------------------- the live setting

const subs = new Set<() => void>();
let current = resolveDevTools(storedChoice(), usedBefore(), false);

/** Turn developer tools on or off, and remember it. */
export function setDevTools(on: boolean) {
  try { localStorage.setItem(DEV_KEY, on ? "on" : "off"); } catch { /* not remembered */ }
  current = on;
  subs.forEach((fn) => fn());
}

/**
 * With no choice made yet: if the box has bring-up runs, this person has used
 * them, so developer tools come on (and stay on). Called once at start.
 */
export function adoptFromBox(runsOnBox: boolean) {
  if (storedChoice() || !runsOnBox || current) return;
  setDevTools(true);
}

export function useDevTools(): boolean {
  return useSyncExternalStore(
    (fn) => { subs.add(fn); return () => subs.delete(fn); },
    () => current,
    () => current,
  );
}
