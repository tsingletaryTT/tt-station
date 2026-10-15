// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The first-run checklist on Home: choose what you want to do, get its model,
// then start it and try it. Each step is worked out from what the box reports,
// so it stays right if someone does a step elsewhere (Models, the phone app).

import type { Mode } from "./api";

export const FIRSTRUN_KEY = "tt.firstrun";

export interface FirstRunSaved {
  /** The mode picked in step one. */
  mode?: string;
  /** Hidden for good by the person. */
  dismissed?: boolean;
  /** Shown once: it stays until it's finished and put away, even after the first request. */
  shown?: boolean;
}

/**
 * Whether to show the checklist at all: not once hidden, and not on a box that
 * has already been used (someone set it up before this app was installed).
 */
export function shouldShow(saved: FirstRunSaved, requestsAll: number | null): boolean {
  if (saved.dismissed) return false;
  if (saved.shown) return true;
  if (requestsAll == null) return false; // not known yet: don't flash it
  return requestsAll === 0;
}

export function loadFirstRun(): FirstRunSaved {
  try { return JSON.parse(localStorage.getItem(FIRSTRUN_KEY) ?? "{}") ?? {}; } catch { return {}; }
}
export function saveFirstRun(v: FirstRunSaved) {
  try { localStorage.setItem(FIRSTRUN_KEY, JSON.stringify(v)); } catch { /* not remembered */ }
}

export type StepState = "done" | "current" | "later";

export interface FirstRun {
  /** The mode the steps are about: the one picked, else the one active. */
  mode: Mode | null;
  choose: StepState;
  get: StepState;
  try: StepState;
  /** All three done. */
  complete: boolean;
}

export function firstRun(saved: FirstRunSaved, modes: Mode[], requestsAll: number): FirstRun {
  const mode = modes.find((m) => m.id === saved.mode) ?? modes.find((m) => m.active) ?? null;
  const chosen = !!mode;
  const got = !!mode?.ready;
  // The first chat, picture or transcript: anything that asked the served model for something.
  const tried = requestsAll > 0;
  const state = (done: boolean, ready: boolean): StepState => (done ? "done" : ready ? "current" : "later");
  return {
    mode,
    choose: state(chosen, true),
    get: state(got, chosen),
    try: state(tried, chosen && got),
    complete: chosen && got && tried,
  };
}
