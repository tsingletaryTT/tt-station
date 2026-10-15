// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { loadTheme, persistTheme, THEME_KEY } from "./theme";

const root = document.documentElement;
const settle = () => new Promise((r) => setTimeout(r)); // MutationObserver callbacks run as microtasks
let watching: MutationObserver | null = null;

beforeEach(() => { localStorage.clear(); delete root.dataset.vesperTheme; });
afterEach(() => { watching?.disconnect(); watching = null; vi.restoreAllMocks(); });

describe("loadTheme", () => {
  it.each(["light", "dark", "system"])("applies a saved %s theme", (theme) => {
    localStorage.setItem(THEME_KEY, theme);
    loadTheme();
    expect(root.dataset.vesperTheme).toBe(theme);
  });

  it("leaves the root alone when nothing is saved", () => {
    loadTheme();
    expect(root.dataset.vesperTheme).toBeUndefined();
  });

  it.each(["", "Light", "neon", "dark ", "null"])("ignores a junk value %j", (junk) => {
    localStorage.setItem(THEME_KEY, junk);
    loadTheme();
    expect(root.dataset.vesperTheme).toBeUndefined();
  });

  it("starts up when reading storage throws", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => { throw new DOMException("denied", "SecurityError"); });
    expect(() => loadTheme()).not.toThrow();
    expect(root.dataset.vesperTheme).toBeUndefined();
  });

  it("starts up when storage itself can't be reached", () => {
    vi.spyOn(window, "localStorage", "get").mockImplementation(() => { throw new DOMException("denied", "SecurityError"); });
    expect(() => loadTheme()).not.toThrow();
    expect(() => { watching = persistTheme(); }).not.toThrow();
  });
});

describe("persistTheme", () => {
  it("saves each theme the switcher sets", async () => {
    watching = persistTheme();
    root.dataset.vesperTheme = "light";
    await settle();
    expect(localStorage.getItem(THEME_KEY)).toBe("light");
    root.dataset.vesperTheme = "system";
    await settle();
    expect(localStorage.getItem(THEME_KEY)).toBe("system");
  });

  it("saves dark when the attribute is removed", async () => {
    root.dataset.vesperTheme = "light";
    watching = persistTheme();
    delete root.dataset.vesperTheme;
    await settle();
    expect(localStorage.getItem(THEME_KEY)).toBe("dark");
  });

  it("doesn't save on other attribute changes", async () => {
    watching = persistTheme();
    root.setAttribute("lang", "en");
    await settle();
    expect(localStorage.getItem(THEME_KEY)).toBeNull();
    root.removeAttribute("lang");
  });

  it("keeps working when storage refuses to save", async () => {
    const set = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new DOMException("full", "QuotaExceededError"); });
    const errors = vi.fn();
    window.addEventListener("error", errors);
    watching = persistTheme();
    root.dataset.vesperTheme = "light";
    await settle();
    window.removeEventListener("error", errors);
    expect(set).toHaveBeenCalledWith(THEME_KEY, "light");
    expect(errors).not.toHaveBeenCalled();
    expect(root.dataset.vesperTheme).toBe("light");
  });

  it("round-trips: what's saved is what the next launch loads", async () => {
    watching = persistTheme();
    root.dataset.vesperTheme = "system";
    await settle();
    watching.disconnect();
    delete root.dataset.vesperTheme;
    loadTheme();
    expect(root.dataset.vesperTheme).toBe("system");
  });
});
