// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Delete now, undo for a while: the thing is hidden at once and only removed
// for real once the undo window has passed.

/** How long a deleted thing can be brought back before it's removed for real. */
export const UNDO_MS = 8000;

export interface Timers {
  setTimeout: (run: () => void, ms: number) => number;
  clearTimeout: (id: number) => void;
}
const windowTimers: Timers = {
  setTimeout: (run, ms) => window.setTimeout(run, ms),
  clearTimeout: (id) => window.clearTimeout(id),
};

/**
 * Hides at once and calls `remove` after `ms` (UNDO_MS); a failed remove shows it again
 * and reports why. Returns the undo: cancels the remove and shows it again.
 */
export function deleteWithUndo({ ms = UNDO_MS, hide, show, remove, removed, failed, timers = windowTimers }: {
  ms?: number;
  hide: () => void;
  show: () => void;
  remove: () => Promise<unknown>;
  removed?: () => void;
  failed: (e: unknown) => void;
  timers?: Timers;
}): () => void {
  hide();
  const later = timers.setTimeout(async () => {
    try { await remove(); removed?.(); }
    catch (e) { show(); failed(e); }
  }, ms);
  return () => { timers.clearTimeout(later); show(); };
}
