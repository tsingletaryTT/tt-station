// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DEV_KEY, storedChoice } from "./devtools";

const mem = (v: Record<string, string>) => ({ getItem: (k: string) => v[k] ?? null });

describe("whether developer tools are on", () => {
  it("reads only a choice made in Settings", () => {
    expect(storedChoice(mem({}))).toBeNull();
    expect(storedChoice(mem({ [DEV_KEY]: "on" }))).toBe("on");
    expect(storedChoice(mem({ [DEV_KEY]: "off" }))).toBe("off");
  });
  it("ignores anything else stored under the key", () => {
    expect(storedChoice(mem({ [DEV_KEY]: "yes" }))).toBeNull();
    expect(storedChoice({ getItem: () => { throw new Error("refused"); } })).toBeNull();
  });
});

describe("the live setting", () => {
  beforeEach(() => { localStorage.clear(); vi.resetModules(); });
  afterEach(() => localStorage.clear());

  it("remembers a change and tells everyone listening", async () => {
    const dev = await import("./devtools");
    expect(dev.useDevTools).toBeTypeOf("function");
    dev.setDevTools(true);
    expect(localStorage.getItem(DEV_KEY)).toBe("on");
    dev.setDevTools(false);
    expect(localStorage.getItem(DEV_KEY)).toBe("off");
  });
});
