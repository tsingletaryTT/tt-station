// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { afterEach, describe, expect, it, vi } from "vitest";
import { createConnectivity, GRACE_MS, RETRY_MS } from "./connectivity";

/** A store on fake timers, with a ping that fails until told otherwise. */
function setup() {
  vi.useFakeTimers();
  let up = false;
  const ping = vi.fn(() => (up ? Promise.resolve("ok") : Promise.reject(new TypeError("Failed to fetch"))));
  const store = createConnectivity({
    now: () => Date.now(),
    setTimeout: (fn, ms) => setTimeout(fn, ms) as unknown as number,
    clearTimeout: (id) => clearTimeout(id),
    ping,
  });
  return { store, ping, comeBack: () => { up = true; } };
}

afterEach(() => { vi.useRealTimers(); });

describe("connectivity", () => {
  it("starts online and stays quiet through a short blip", async () => {
    const { store } = setup();
    expect(store.get().online).toBe(true);
    store.miss("Failed to fetch");
    expect(store.get().online).toBe(true);
    expect(store.get().failingSince).not.toBeNull();
    await vi.advanceTimersByTimeAsync(GRACE_MS - 100);
    store.ok();
    await vi.advanceTimersByTimeAsync(1000);
    expect(store.get().online).toBe(true);
    expect(store.get().failingSince).toBeNull();
    store.dispose();
  });

  it("says it can't reach the box once misses outlast the grace period", async () => {
    const { store } = setup();
    const seen: boolean[] = [];
    store.subscribe(() => seen.push(store.get().online));
    store.miss("Failed to fetch");
    await vi.advanceTimersByTimeAsync(GRACE_MS + 10);
    expect(store.get().online).toBe(false);
    expect(store.get().lastError).toBe("Failed to fetch");
    expect(seen).toContain(false);
    store.dispose();
  });

  it("keeps checking by itself, backing off, and recovers when the box answers", async () => {
    const { store, ping, comeBack } = setup();
    store.miss("Failed to fetch");
    await vi.advanceTimersByTimeAsync(RETRY_MS[0] + 10);
    expect(ping).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(RETRY_MS[1] + 10);
    expect(ping).toHaveBeenCalledTimes(2);
    expect(store.get().online).toBe(false);
    comeBack();
    await vi.advanceTimersByTimeAsync(RETRY_MS[2] + 10);
    expect(ping).toHaveBeenCalledTimes(3);
    expect(store.get().online).toBe(true);
    // Back online: no more checking.
    await vi.advanceTimersByTimeAsync(60_000);
    expect(ping).toHaveBeenCalledTimes(3);
    store.dispose();
  });

  it("many failing calls at once schedule one check, not one each", async () => {
    const { store, ping } = setup();
    for (let i = 0; i < 20; i++) store.miss("Failed to fetch");
    await vi.advanceTimersByTimeAsync(RETRY_MS[0] + 10);
    expect(ping).toHaveBeenCalledTimes(1);
    store.dispose();
  });

  it("Try now checks at once", async () => {
    const { store, ping, comeBack } = setup();
    store.miss("Failed to fetch");
    await vi.advanceTimersByTimeAsync(GRACE_MS + 10);
    comeBack();
    store.retryNow();
    await vi.advanceTimersByTimeAsync(0);
    expect(ping).toHaveBeenCalled();
    expect(store.get().online).toBe(true);
    store.dispose();
  });
});

describe("request() errors", () => {
  afterEach(() => { vi.unstubAllGlobals(); vi.resetModules(); });

  it("a call that never reaches the box says so in words, and counts as a miss", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.reject(new TypeError("Failed to fetch"))));
    const { connectivity } = await import("./connectivity");
    const miss = vi.spyOn(connectivity, "miss");
    const { get, NetworkError } = await import("./api");
    const err = await get<never>("/serve").catch((e: unknown) => e as Error & { detail?: string; timedOut?: boolean });
    expect(err).toBeInstanceOf(NetworkError);
    expect(err.message).toMatch(/^Can't reach .+\. Check that it's on and connected/);
    expect(err.message).not.toMatch(/Failed to fetch/);
    expect(err.detail).toBe("Failed to fetch");
    expect(miss).toHaveBeenCalledWith("Failed to fetch");
    connectivity.dispose();
  });

  it("a slow box times out with its own message, and isn't counted as unreachable", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.reject(Object.assign(new Error("signal timed out"), { name: "TimeoutError" }))));
    const { connectivity } = await import("./connectivity");
    const miss = vi.spyOn(connectivity, "miss");
    const { get } = await import("./api");
    const err = await get<never>("/serve").catch((e: unknown) => e as Error & { detail?: string; timedOut?: boolean });
    expect(err.message).toMatch(/took too long to answer/);
    expect(err.timedOut).toBe(true);
    expect(miss).not.toHaveBeenCalled();
  });

  it("reads time out; writes (a stop can take minutes) don't", async () => {
    const fetch = vi.fn((_path: string, _init: RequestInit) => Promise.resolve(new Response("{}")));
    vi.stubGlobal("fetch", fetch);
    const { get, post } = await import("./api");
    await get("/serve");
    await post("/stop");
    expect(fetch.mock.calls[0][1].signal).toBeInstanceOf(AbortSignal);
    expect(fetch.mock.calls[1][1].signal).toBeUndefined();
  });

  it("a gateway that couldn't reach the box counts as no answer", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("", { status: 502, statusText: "Bad Gateway" }))));
    const { connectivity } = await import("./connectivity");
    const miss = vi.spyOn(connectivity, "miss");
    const { get, NetworkError } = await import("./api");
    const err = await get<never>("/serve").catch((e: unknown) => e as Error);
    expect(err).toBeInstanceOf(NetworkError);
    expect(miss).toHaveBeenCalledWith("502 Bad Gateway");
    connectivity.dispose();
  });

  it("any answer, even an error status, means the box is there", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ error: "No such mode." }), { status: 404 }))));
    const { connectivity } = await import("./connectivity");
    const ok = vi.spyOn(connectivity, "ok");
    const { get, ApiError } = await import("./api");
    const err = await get<never>("/modes/x").catch((e: unknown) => e as Error & { detail?: string; timedOut?: boolean });
    expect(err).toBeInstanceOf(ApiError);
    expect(err.message).toBe("No such mode.");
    expect(ok).toHaveBeenCalled();
  });
});
