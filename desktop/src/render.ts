// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Keeping streaming pages cheap to re-render: streamed tokens applied once per
// frame rather than once each, and rebuilt lists that keep the objects of the
// entries that didn't change, so memoized views of those skip rendering.

/** Runs `flush` at most once per animation frame however often `poke` is called;
 *  `now` runs it at once (before something that must come after the batched part). */
export function frameBatcher(
  flush: () => void,
  schedule: (f: () => void) => number = (f) => requestAnimationFrame(f),
  cancel: (handle: number) => void = (h) => cancelAnimationFrame(h),
) {
  let pending: number | null = null;
  return {
    poke() {
      if (pending === null) pending = schedule(() => { pending = null; flush(); });
    },
    now() {
      if (pending !== null) { cancel(pending); pending = null; }
      flush();
    },
  };
}

/** Whether two plain values (JSON-like: objects, arrays, primitives) are the same. */
export function same(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a)) {
    const bb = b as unknown[];
    return a.length === bb.length && a.every((x, i) => same(x, bb[i]));
  }
  const ka = Object.keys(a);
  const ob = b as Record<string, unknown>;
  return ka.length === Object.keys(ob).length && ka.every((k) => k in ob && same((a as Record<string, unknown>)[k], ob[k]));
}

/** `next`, with each entry that is the same as `prev`'s entry of that key replaced
 *  by `prev`'s object. */
export function reuse<T extends { key: string }>(prev: readonly T[], next: T[]): T[] {
  if (prev.length === 0) return next;
  const old = new Map(prev.map((p) => [p.key, p]));
  return next.map((n) => {
    const p = old.get(n.key);
    return p !== undefined && same(p, n) ? p : n;
  });
}
