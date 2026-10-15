// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { resetShared } from "./poll";
import { usePoll, useSharedPoll } from "./ui";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let hidden = false;
let root: Root;
let host: HTMLDivElement;
beforeEach(() => {
  vi.useFakeTimers();
  hidden = false;
  Object.defineProperty(document, "hidden", { configurable: true, get: () => hidden });
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  resetShared();
});
afterEach(() => {
  act(() => root.unmount());
  host.remove();
  vi.useRealTimers();
});
const setHidden = async (h: boolean) => {
  hidden = h;
  await act(async () => { document.dispatchEvent(new Event("visibilitychange")); });
};

function Poller({ fn, ms }: { fn: () => Promise<number>; ms: number }) {
  const { data } = usePoll(fn, ms);
  return <span>{data ?? "…"}</span>;
}

describe("usePoll", () => {
  it("polls while shown, pauses while the window is hidden, and resumes at once when shown", async () => {
    let n = 0;
    const fn = vi.fn(async () => ++n);
    await act(async () => root.render(<Poller fn={fn} ms={1000} />));
    await act(async () => { await vi.advanceTimersByTimeAsync(2000); });
    const shown = fn.mock.calls.length;
    expect(shown).toBe(3);
    await setHidden(true);
    await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });
    expect(fn.mock.calls.length).toBeLessThanOrEqual(shown + 1);
    const paused = fn.mock.calls.length;
    await setHidden(false);
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(fn.mock.calls.length).toBe(paused + 1);
    expect(host.textContent).toBe(String(n));
  });
});

function Shared({ id, fn, ms }: { id: string; fn: () => Promise<string>; ms: number }) {
  const { data } = useSharedPoll("/serve", fn, ms);
  return <span data-id={id}>{data ?? "…"}</span>;
}

describe("useSharedPoll", () => {
  it("two places asking for the same thing share one request", async () => {
    const fn = vi.fn(async () => "serving");
    await act(async () => root.render(<><Shared id="sidebar" fn={fn} ms={4000} /><Shared id="home" fn={fn} ms={3000} /></>));
    await act(async () => { await vi.advanceTimersByTimeAsync(60_000); });
    // One source at the quicker 3 s: 1 + 60/3 calls, not 1 + 15 plus 1 + 20.
    expect(fn).toHaveBeenCalledTimes(21);
    expect(host.textContent).toBe("servingserving");
  });
});
