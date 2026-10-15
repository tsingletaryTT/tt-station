// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { PollSource, type PollEnv } from "./poll";

let hidden = false;
let onVis: (() => void) | null = null;
const env: PollEnv = {
  now: () => Date.now(),
  setTimeout: (fn, ms) => setTimeout(fn, ms) as unknown as number,
  clearTimeout: (id) => clearTimeout(id),
  hidden: () => hidden,
  onVisibility: (fn) => { onVis = fn; return () => { onVis = null; }; },
};
const show = () => { hidden = false; onVis?.(); };
const hide = () => { hidden = true; onVis?.(); };

beforeEach(() => { vi.useFakeTimers(); hidden = false; onVis = null; });
afterEach(() => { vi.useRealTimers(); });

describe("PollSource", () => {
  it("fetches at once, then every interval", async () => {
    const fn = vi.fn(async () => 1);
    const s = new PollSource(fn, env);
    const stop = s.add(1000, () => {});
    await vi.advanceTimersByTimeAsync(0);
    expect(fn).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(3000);
    expect(fn).toHaveBeenCalledTimes(4);
    stop();
  });

  it("rests while the window is hidden, and catches up when it's shown", async () => {
    const fn = vi.fn(async () => 1);
    const s = new PollSource(fn, env);
    const stop = s.add(1000, () => {});
    await vi.advanceTimersByTimeAsync(0);
    hide();
    await vi.advanceTimersByTimeAsync(60_000);
    // At most the one already scheduled when it was hidden.
    expect(fn.mock.calls.length).toBeLessThanOrEqual(2);
    const whileHidden = fn.mock.calls.length;
    show();
    await vi.advanceTimersByTimeAsync(0);
    expect(fn).toHaveBeenCalledTimes(whileHidden + 1);
    await vi.advanceTimersByTimeAsync(1000);
    expect(fn).toHaveBeenCalledTimes(whileHidden + 2);
    stop();
  });

  it("stops entirely when nobody uses it", async () => {
    const fn = vi.fn(async () => 1);
    const s = new PollSource(fn, env);
    const stop = s.add(1000, () => {});
    await vi.advanceTimersByTimeAsync(0);
    stop();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(fn).toHaveBeenCalledTimes(1);
    expect(onVis).toBeNull();
  });

  it("shares one fetch between users, at the quickest interval asked for", async () => {
    const fn = vi.fn(async () => "serving");
    const s = new PollSource(fn, env);
    const a = vi.fn(), b = vi.fn();
    const stopA = s.add(4000, a);
    const stopB = s.add(3000, b);
    await vi.advanceTimersByTimeAsync(0);
    expect(fn).toHaveBeenCalledTimes(1); // the second user joined the first fetch
    await vi.advanceTimersByTimeAsync(12_000);
    expect(fn).toHaveBeenCalledTimes(5); // every 3 s, not 3 s + 4 s
    expect(a).toHaveBeenCalledTimes(5);
    expect(b).toHaveBeenCalledTimes(5);
    expect(s.data).toBe("serving");
    stopB();
    await vi.advanceTimersByTimeAsync(8000);
    expect(fn).toHaveBeenCalledTimes(7); // back to every 4 s
    stopA();
  });

  it("a second user arriving with a fresh answer doesn't fetch again", async () => {
    const fn = vi.fn(async () => 1);
    const s = new PollSource(fn, env);
    const stopA = s.add(3000, () => {});
    await vi.advanceTimersByTimeAsync(500);
    const stopB = s.add(3000, () => {});
    await vi.advanceTimersByTimeAsync(0);
    expect(fn).toHaveBeenCalledTimes(1);
    stopA(); stopB();
  });

  it("refresh joins a fetch already on its way", async () => {
    let resolve!: (v: number) => void;
    const fn = vi.fn(() => new Promise<number>((r) => { resolve = r; }));
    const s = new PollSource(fn, env);
    const stop = s.add(1000, () => {});
    const again = s.fetch();
    expect(fn).toHaveBeenCalledTimes(1);
    resolve(7);
    await again;
    expect(s.data).toBe(7);
    stop();
  });

  it("keeps the last answer through a failed fetch, and reports the error", async () => {
    let fail = false;
    const s = new PollSource(async () => { if (fail) throw new Error("Can't reach qb2"); return 3; }, env);
    const stop = s.add(1000, () => {});
    await vi.advanceTimersByTimeAsync(0);
    fail = true;
    await vi.advanceTimersByTimeAsync(1000);
    expect(s.data).toBe(3);
    expect(s.error).toBe("Can't reach qb2");
    stop();
  });
});
