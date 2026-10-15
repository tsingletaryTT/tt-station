// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Whether the box answers. Every API call reports here (api.ts): a reply of
// any kind means the box is there; a call that never reached it (no network,
// the agent restarting, the box off) is a miss. After a short grace, so one
// blip doesn't flash a banner, the app says it can't reach the box, keeps
// checking on its own, and clears the moment anything answers again.

import { useSyncExternalStore } from "react";

export interface Connectivity {
  /** False once calls have been failing for longer than the grace period. */
  online: boolean;
  /** When the first miss in the current run of misses happened (ms), or null. */
  failingSince: number | null;
  /** The last miss, in the browser's own words, for "Details for support". */
  lastError: string | null;
  /** When the box first answered after a run of misses (or at all), ms; null before that. */
  lastOk: number | null;
}

/** How long calls must keep failing before the app says so. */
export const GRACE_MS = 2500;
/** How often to check by itself while the box can't be reached. */
export const RETRY_MS = [2000, 4000, 8000, 15000];

export interface ConnectivityStore {
  get: () => Connectivity;
  subscribe: (fn: () => void) => () => void;
  /** The box answered (any HTTP status). */
  ok: () => void;
  /** A call never reached the box. */
  miss: (why: string) => void;
  /** Check now, without waiting for the next retry. */
  retryNow: () => void;
  /** Stops the timers (tests). */
  dispose: () => void;
}

export interface StoreDeps {
  now?: () => number;
  setTimeout?: (fn: () => void, ms: number) => number;
  clearTimeout?: (id: number) => void;
  /** Resolves if the box answers at all (default: GET /status). */
  ping?: () => Promise<unknown>;
}

export function createConnectivity(deps: StoreDeps = {}): ConnectivityStore {
  const now = deps.now ?? (() => Date.now());
  const later = deps.setTimeout ?? ((fn, ms) => window.setTimeout(fn, ms));
  const cancel = deps.clearTimeout ?? ((id) => window.clearTimeout(id));
  const ping = deps.ping ?? (() => fetch("/status", { cache: "no-store", signal: AbortSignal.timeout(8000) }).then((r) => {
    // A gateway that couldn't reach the box isn't the box answering.
    if (r.status === 502 || r.status === 504) throw new Error(`${r.status} ${r.statusText}`.trim());
    return r;
  }));
  let state: Connectivity = { online: true, failingSince: null, lastError: null, lastOk: null };
  const subs = new Set<() => void>();
  let grace: number | null = null;
  let retry: number | null = null;
  let attempt = 0;

  const set = (next: Partial<Connectivity>) => {
    state = { ...state, ...next };
    subs.forEach((fn) => fn());
  };
  const stopTimers = () => {
    if (grace != null) cancel(grace);
    if (retry != null) cancel(retry);
    grace = retry = null;
  };
  const scheduleRetry = () => {
    if (retry != null) cancel(retry);
    const wait = RETRY_MS[Math.min(attempt, RETRY_MS.length - 1)];
    attempt += 1;
    retry = later(check, wait);
  };
  const check = () => {
    retry = null;
    ping().then(() => store.ok(), (e) => { store.miss(String((e as Error)?.message ?? e)); });
  };

  const store: ConnectivityStore = {
    get: () => state,
    subscribe: (fn) => { subs.add(fn); return () => subs.delete(fn); },
    ok: () => {
      attempt = 0;
      stopTimers();
      // Only changes are published: a steady stream of answers re-renders nothing.
      if (!state.online || state.failingSince != null || state.lastOk == null) set({ online: true, failingSince: null, lastOk: now() });
    },
    miss: (why) => {
      const first = state.failingSince ?? now();
      if (state.failingSince == null) {
        set({ failingSince: first, lastError: why });
        grace = later(() => { grace = null; if (state.failingSince != null) set({ online: false }); }, GRACE_MS);
      } else if (why !== state.lastError) set({ lastError: why });
      if (retry == null) scheduleRetry();
    },
    retryNow: () => { attempt = 0; if (retry != null) cancel(retry); retry = null; check(); },
    dispose: stopTimers,
  };
  return store;
}

/** The app's one store. */
export const connectivity = createConnectivity();

/** Whether the box answers, for the banner and screens that wait on it. */
export function useConnectivity(store: ConnectivityStore = connectivity): Connectivity {
  return useSyncExternalStore(store.subscribe, store.get, store.get);
}
