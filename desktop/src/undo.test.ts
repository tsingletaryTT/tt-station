// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { afterEach, describe, expect, it, vi } from "vitest";
import { deleteWithUndo, type Timers, UNDO_MS } from "./undo";

/** Timers that only run when told to, so the undo window is ours to end. */
function manualTimers() {
  const pending = new Map<number, { run: () => void; ms: number }>();
  let next = 1;
  const timers: Timers = {
    setTimeout: vi.fn((run: () => void, ms: number) => { pending.set(next, { run, ms }); return next++; }),
    clearTimeout: vi.fn((id: number) => { pending.delete(id); }),
  };
  const elapse = async () => {
    for (const [id, t] of [...pending]) { pending.delete(id); t.run(); }
    await new Promise((r) => setTimeout(r)); // let the async remove settle
  };
  return { timers, pending, elapse };
}

function callbacks(remove: () => Promise<unknown> = () => Promise.resolve()) {
  return { hide: vi.fn(), show: vi.fn(), remove: vi.fn(remove), removed: vi.fn(), failed: vi.fn() };
}

describe("deleteWithUndo", () => {
  afterEach(() => { vi.useRealTimers(); });

  it("hides at once and removes only after the undo window", async () => {
    const { timers, pending, elapse } = manualTimers();
    const cb = callbacks();
    deleteWithUndo({ ...cb, timers });
    expect(cb.hide).toHaveBeenCalledTimes(1);
    expect(cb.remove).not.toHaveBeenCalled();
    expect([...pending.values()].map((t) => t.ms)).toEqual([UNDO_MS]);

    await elapse();
    expect(cb.remove).toHaveBeenCalledTimes(1);
    expect(cb.removed).toHaveBeenCalledTimes(1);
    expect(cb.show).not.toHaveBeenCalled();
    expect(cb.failed).not.toHaveBeenCalled();
  });

  it("waits 8 seconds on the real clock", async () => {
    vi.useFakeTimers();
    const cb = callbacks();
    deleteWithUndo(cb);
    await vi.advanceTimersByTimeAsync(UNDO_MS - 1);
    expect(cb.remove).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    expect(UNDO_MS).toBe(8000);
    expect(cb.remove).toHaveBeenCalledTimes(1);
    expect(cb.removed).toHaveBeenCalledTimes(1);
  });

  it("undo cancels the remove and shows it again", async () => {
    const { timers, pending, elapse } = manualTimers();
    const cb = callbacks();
    const undo = deleteWithUndo({ ...cb, timers });
    undo();
    expect(timers.clearTimeout).toHaveBeenCalledTimes(1);
    expect(pending.size).toBe(0);
    expect(cb.show).toHaveBeenCalledTimes(1);

    await elapse();
    expect(cb.remove).not.toHaveBeenCalled();
    expect(cb.removed).not.toHaveBeenCalled();
  });

  it("undo cancels on the real clock too", async () => {
    vi.useFakeTimers();
    const cb = callbacks();
    const undo = deleteWithUndo(cb);
    await vi.advanceTimersByTimeAsync(UNDO_MS / 2);
    undo();
    await vi.advanceTimersByTimeAsync(UNDO_MS * 2);
    expect(cb.remove).not.toHaveBeenCalled();
    expect(cb.show).toHaveBeenCalledTimes(1);
  });

  it("a failed remove shows it again and reports why", async () => {
    const { timers, elapse } = manualTimers();
    const why = new Error("The box said no.");
    const cb = callbacks(() => Promise.reject(why));
    deleteWithUndo({ ...cb, timers });
    await elapse();
    expect(cb.remove).toHaveBeenCalledTimes(1);
    expect(cb.show).toHaveBeenCalledTimes(1);
    expect(cb.failed).toHaveBeenCalledWith(why);
    expect(cb.removed).not.toHaveBeenCalled();
  });
});
