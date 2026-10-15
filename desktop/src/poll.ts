// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Polling the box, politely. A source fetches, waits, and fetches again for as
// long as something on screen uses it; it rests while the window is hidden
// (minimized, another workspace, closed to the tray) and catches up the moment
// it's shown again. Screens that ask for the same thing (`/serve` from the
// sidebar and Home) share one source keyed by name, at the quickest interval
// any of them asked for, instead of each polling on its own.

export interface PollEnv {
  now: () => number;
  setTimeout: (fn: () => void, ms: number) => number;
  clearTimeout: (id: number) => void;
  hidden: () => boolean;
  /** Calls `fn` whenever the window is shown or hidden; returns the unsubscribe. */
  onVisibility: (fn: () => void) => () => void;
}

export const browserEnv: PollEnv = {
  now: () => Date.now(),
  setTimeout: (fn, ms) => window.setTimeout(fn, ms),
  clearTimeout: (id) => window.clearTimeout(id),
  hidden: () => typeof document !== "undefined" && document.hidden,
  onVisibility: (fn) => {
    document.addEventListener("visibilitychange", fn);
    return () => document.removeEventListener("visibilitychange", fn);
  },
};

interface Sub { ms: number; notify: () => void }

export class PollSource<T> {
  data: T | null = null;
  error: string | null = null;
  /** When the last fetch finished (ms), 0 before the first. */
  at = 0;
  private subs = new Set<Sub>();
  private timer: number | null = null;
  private inflight: Promise<void> | null = null;
  private paused = false;
  private unwatch: (() => void) | null = null;

  constructor(public fn: () => Promise<T>, private env: PollEnv = browserEnv) {}

  /** The quickest interval anyone using this source asked for. */
  get interval(): number {
    let ms = Infinity;
    for (const s of this.subs) ms = Math.min(ms, s.ms);
    return ms;
  }

  get users(): number { return this.subs.size; }

  /** Start using it: fetches now unless a fresh enough answer is already here. Returns the stop. */
  add(ms: number, notify: () => void): () => void {
    const sub = { ms, notify };
    this.subs.add(sub);
    if (!this.unwatch) this.unwatch = this.env.onVisibility(this.onVisibility);
    if (this.inflight) { /* the answer on its way reaches everyone */ }
    else if (this.at === 0 || this.env.now() - this.at >= ms) void this.fetch();
    else this.schedule();
    return () => {
      this.subs.delete(sub);
      if (this.subs.size === 0) {
        this.clear();
        this.unwatch?.();
        this.unwatch = null;
      } else this.schedule();
    };
  }

  /** Fetch now (one at a time: a second call joins the first). */
  fetch(): Promise<void> {
    if (this.inflight) return this.inflight;
    this.clear();
    this.paused = false;
    this.inflight = (async () => {
      try {
        this.data = await this.fn();
        this.error = null;
      } catch (e) {
        this.error = (e as Error)?.message ?? String(e);
      }
      this.at = this.env.now();
      this.inflight = null;
      this.subs.forEach((s) => s.notify());
      this.schedule();
    })();
    return this.inflight;
  }

  /** Replace the answer by hand (after a write that returns the new state). */
  set(next: T | null | ((cur: T | null) => T | null)) {
    this.data = typeof next === "function" ? (next as (cur: T | null) => T | null)(this.data) : next;
    this.error = null;
    this.subs.forEach((s) => s.notify());
  }

  private clear() {
    if (this.timer != null) this.env.clearTimeout(this.timer);
    this.timer = null;
  }

  private schedule() {
    this.clear();
    if (this.subs.size === 0 || this.inflight) return;
    const wait = Math.max(0, this.interval - (this.env.now() - this.at));
    this.timer = this.env.setTimeout(() => {
      this.timer = null;
      // Hidden: rest until shown again (onVisibility picks it up).
      if (this.env.hidden()) { this.paused = true; return; }
      void this.fetch();
    }, wait);
  }

  private onVisibility = () => {
    if (this.env.hidden() || this.subs.size === 0) return;
    if (this.paused || this.env.now() - this.at >= this.interval) void this.fetch();
  };
}

const shared = new Map<string, PollSource<unknown>>();

/** The one source for `key`, made on first use. It's kept when nobody's using it, so a screen
 *  opened again shows the last answer at once while it fetches a fresh one. */
export function sharedSource<T>(key: string, fn: () => Promise<T>, env: PollEnv = browserEnv): PollSource<T> {
  let s = shared.get(key) as PollSource<T> | undefined;
  if (!s) {
    s = new PollSource<T>(fn, env);
    shared.set(key, s as PollSource<unknown>);
  }
  return s;
}

/** For tests: forget every shared source. */
export function resetShared() { shared.clear(); }
