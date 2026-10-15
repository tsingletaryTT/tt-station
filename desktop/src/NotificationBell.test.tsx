// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { InboxItem } from "./inbox";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const state = vi.hoisted(() => ({ items: [] as InboxItem[], loaded: true }));
const navigate = vi.hoisted(() => vi.fn());
const answer = vi.hoisted(() => vi.fn(() => Promise.resolve()));
vi.mock("./useInbox", () => ({
  useInbox: () => ({ items: state.items, owner: true, loaded: state.loaded, error: null, refresh: () => {}, ignore: () => {} }),
}));
vi.mock("./App", () => ({ navigate }));
vi.mock("./inbox", async (orig) => ({ ...(await orig<typeof import("./inbox")>()), answer }));
vi.mock("./ui", () => ({ toastError: () => {} }));

const { NotificationBell } = await import("./NotificationBell");

const NOW = Math.floor(Date.now() / 1000);
const item = (n: number, since = NOW - 600 + n): InboxItem => ({
  key: `code:s${n}:r`, kind: "code", where: `Code · ~/p${n}`, summary: `npm test ${n}`, since,
  answer: "decide", canAlways: true, open: { route: "code", hash: `s${n}` }, ref: { approval: `code:s${n}:r` },
});

let root: Root;
let host: HTMLDivElement;
let outside: HTMLButtonElement;
const render = () => act(() => { root.render(<NotificationBell />); });
const bell = () => document.querySelector<HTMLButtonElement>(".bell-button")!;
const panel = () => document.querySelector<HTMLElement>(".bell-panel");
const rows = () => [...document.querySelectorAll<HTMLLIElement>(".bell-item")];
const key = (k: string, target: EventTarget = document.activeElement ?? window) => act(() => {
  target.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true, cancelable: true }));
});
const click = (el: HTMLElement) => act(() => { el.click(); });

beforeEach(() => {
  state.items = [];
  state.loaded = true;
  navigate.mockClear();
  answer.mockClear();
  host = document.createElement("div");
  outside = document.createElement("button");
  outside.textContent = "elsewhere";
  document.body.append(host, outside);
  root = createRoot(host);
});
afterEach(() => { act(() => root.unmount()); document.body.innerHTML = ""; });

describe("notifications bell", () => {
  it("shows no count when nothing waits, and says so", () => {
    render();
    expect(document.querySelector(".bell-count")).toBeNull();
    expect(bell().getAttribute("aria-label")).toBe("Notifications, nothing waiting");
  });

  it("counts what's waiting, up to 9+", () => {
    state.items = [item(1), item(2), item(3)];
    render();
    expect(document.querySelector(".bell-count")!.textContent).toBe("3");
    expect(bell().getAttribute("aria-label")).toBe("Notifications, 3 waiting");
    state.items = Array.from({ length: 12 }, (_, i) => item(i));
    render();
    expect(document.querySelector(".bell-count")!.textContent).toBe("9+");
    expect(bell().getAttribute("aria-label")).toBe("Notifications, 12 waiting");
  });

  it("pulses and announces when the count goes up, not on the first load", () => {
    state.items = [item(1)];
    render();
    expect(document.querySelector(".bell-pulse")).toBeNull();
    state.items = [item(1), item(2)];
    render();
    expect(document.querySelector(".bell-pulse")).not.toBeNull();
    expect(document.querySelector("[aria-live=polite]")!.textContent).toMatch(/2 waiting/);
  });

  it("opens under the bell, newest first, with focus on the newest; Escape closes and focus returns", () => {
    state.items = [item(1), item(2)];
    render();
    click(bell());
    expect(bell().getAttribute("aria-expanded")).toBe("true");
    expect(rows().map((r) => r.querySelector(".bell-summary")!.textContent)).toEqual(["npm test 2", "npm test 1"]);
    expect(document.activeElement).toBe(rows()[0]);
    key("ArrowDown");
    expect(document.activeElement).toBe(rows()[1]);
    key("Escape");
    expect(panel()).toBeNull();
    expect(document.activeElement).toBe(bell());
  });

  it("closes on a click outside, without taking focus", () => {
    state.items = [item(1)];
    render();
    click(bell());
    act(() => { outside.dispatchEvent(new MouseEvent("mousedown", { bubbles: true })); outside.focus(); });
    expect(panel()).toBeNull();
    expect(document.activeElement).toBe(outside);
  });

  it("toggles from the bell, and closes when the app goes somewhere", () => {
    render();
    click(bell());
    expect(panel()).not.toBeNull();
    click(bell());
    expect(panel()).toBeNull();
    expect(document.activeElement).toBe(bell());
    click(bell());
    act(() => { window.dispatchEvent(new PopStateEvent("popstate")); });
    expect(panel()).toBeNull();
  });

  it("is all caught up when nothing waits, focusing the panel itself", () => {
    render();
    click(bell());
    expect(panel()!.textContent).toMatch(/You're all caught up/);
    expect(document.activeElement).toBe(panel());
  });

  it("answers with A, and Open goes where the item came from", async () => {
    state.items = [item(1), item(2)];
    render();
    click(bell());
    await act(async () => { rows()[0].dispatchEvent(new KeyboardEvent("keydown", { key: "a", bubbles: true, cancelable: true })); });
    expect(answer).toHaveBeenCalledWith(expect.objectContaining({ key: "code:s2:r" }), "allow");
    expect(rows()).toHaveLength(1);
    const open = [...rows()[0].querySelectorAll("button")].find((b) => b.textContent === "Open")!;
    click(open);
    expect(navigate).toHaveBeenCalledWith("code", "s1");
    expect(panel()).toBeNull();
    expect(document.activeElement).toBe(bell());
  });

  it("links to the full page", () => {
    render();
    click(bell());
    click(document.querySelector<HTMLButtonElement>(".bell-all")!);
    expect(navigate).toHaveBeenCalledWith("inbox");
    expect(panel()).toBeNull();
  });
});
