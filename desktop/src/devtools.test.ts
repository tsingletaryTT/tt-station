// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DEV_KEY, placeVisible, resolveDevTools, storedChoice, usedBefore } from "./devtools";

const mem = (v: Record<string, string>) => ({ getItem: (k: string) => v[k] ?? null });

describe("which places show", () => {
  it("hides Bring-up without developer tools, and nothing else", () => {
    const all = ["home", "chat", "studio", "research", "code", "library", "routines", "models", "bringup", "hardware", "discover", "connect", "settings", "orchard"];
    expect(all.filter((r) => placeVisible(r, false))).toEqual(["home", "chat", "studio", "research", "code", "library", "routines", "models", "hardware", "discover", "connect", "settings"]);
    expect(all.filter((r) => placeVisible(r, true))).toEqual(all);
  });
});

describe("whether developer tools are on", () => {
  it("is off for a new install", () => {
    expect(resolveDevTools(storedChoice(mem({})), usedBefore(mem({})), false)).toBe(false);
  });
  it("is on for someone who has used bring-up in this app", () => {
    expect(usedBefore(mem({ "tt.bringup.way": "orchard" }))).toBe(true);
    expect(resolveDevTools(null, true, false)).toBe(true);
  });
  it("is on when the box has bring-up runs", () => {
    expect(resolveDevTools(null, false, true)).toBe(true);
  });
  it("a choice made in Settings wins either way", () => {
    expect(resolveDevTools(storedChoice(mem({ [DEV_KEY]: "off" })), true, true)).toBe(false);
    expect(resolveDevTools(storedChoice(mem({ [DEV_KEY]: "on" })), false, false)).toBe(true);
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

  it("turns on by itself for bring-up runs on the box, but never over a choice", async () => {
    let dev = await import("./devtools");
    dev.adoptFromBox(true);
    expect(localStorage.getItem(DEV_KEY)).toBe("on");
    localStorage.setItem(DEV_KEY, "off");
    vi.resetModules();
    dev = await import("./devtools");
    dev.adoptFromBox(true);
    expect(localStorage.getItem(DEV_KEY)).toBe("off");
  });

  it("stays off without runs", async () => {
    const dev = await import("./devtools");
    dev.adoptFromBox(false);
    expect(localStorage.getItem(DEV_KEY)).toBeNull();
  });
});
