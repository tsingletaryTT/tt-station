// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
// Chat scrolls its message list, never the window: new output follows you only
// while you're at the bottom, and the composer focuses without scrolling.
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { StreamEvent } from "../api";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

// The answer streams one chunk each time the test lets it.
const stream = vi.hoisted(() => ({ release: [] as (() => void)[], chunks: [] as string[] }));
vi.mock("../api", async (orig) => ({
  ...(await orig<typeof import("../api")>()),
  clientKind: async () => "desktop",
  streamChat: async function* (): AsyncGenerator<StreamEvent> {
    for (const text of stream.chunks) {
      await new Promise<void>((r) => stream.release.push(r));
      yield { type: "delta", text };
    }
    yield { type: "done", reply: stream.chunks.join(""), model: "m" };
  },
}));
vi.mock("../ui", () => ({
  usePoll: () => ({ data: { where: "chips", model: "m", phase: "serving" }, refresh: () => {}, error: null }),
  useToriShortcut: () => "Ctrl+Space",
  toastError: () => {},
  toastOk: () => {},
}));
vi.mock("../VoiceMode", () => ({ VoiceMode: () => null }));
vi.mock("../render", () => ({ frameBatcher: (fn: () => void) => ({ poke: fn, now: fn }) }));

const { Chat } = await import("./Chat");

let root: Root;
let host: HTMLDivElement;
const scrollIntoView = vi.fn();
const windowScrollTo = vi.fn();
const focus = vi.fn();
const realFocus = HTMLTextAreaElement.prototype.focus;

// jsdom doesn't lay out: the message list is 400px tall, its content grows with its text.
const scroller = () => host.querySelector<HTMLDivElement>(".chat-scroll")!;
const measure = () => {
  const el = scroller();
  Object.defineProperty(el, "clientHeight", { configurable: true, get: () => 400 });
  Object.defineProperty(el, "scrollHeight", { configurable: true, get: () => 1000 + (el.textContent?.length ?? 0) * 10 });
};
const atBottom = () => scroller().scrollTop === scroller().scrollHeight;
const jumpButton = () => [...host.querySelectorAll("button")].find((b) => b.textContent?.includes("Jump to latest"));
const nextChunk = async () => { await act(async () => { stream.release.shift()?.(); await new Promise((r) => setTimeout(r, 0)); }); };

const send = async (text: string) => {
  const ta = host.querySelector("textarea")!;
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(ta, text);
    ta.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await act(async () => { ta.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true })); });
};

beforeEach(() => {
  stream.release = [];
  stream.chunks = ["First part of the answer. ", "More of it. ", "And the rest."];
  localStorage.clear();
  localStorage.setItem("tt.conversations", JSON.stringify([{
    id: "c1", title: "Long one", updated: 1,
    messages: Array.from({ length: 12 }, (_, i) => ({ role: i % 2 ? "assistant" : "user", content: `message ${i}` })),
  }]));
  Element.prototype.scrollIntoView = scrollIntoView;
  window.scrollTo = windowScrollTo as unknown as typeof window.scrollTo;
  HTMLTextAreaElement.prototype.focus = focus;
  scrollIntoView.mockClear();
  windowScrollTo.mockClear();
  focus.mockClear();
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
});
afterEach(() => {
  act(() => root.unmount());
  host.remove();
  HTMLTextAreaElement.prototype.focus = realFocus;
});

describe("Chat scrolling", () => {
  it("follows a streaming answer in the message list, not the window", async () => {
    await act(async () => root.render(<Chat />));
    measure();
    await send("Tell me more");
    await nextChunk();
    expect(atBottom()).toBe(true);
    const before = scroller().scrollTop;
    await nextChunk();
    expect(scroller().scrollTop).toBeGreaterThan(before);
    expect(atBottom()).toBe(true);
    expect(jumpButton()).toBeUndefined();
    expect(scrollIntoView).not.toHaveBeenCalled();
    expect(windowScrollTo).not.toHaveBeenCalled();
    expect(document.documentElement.scrollTop).toBe(0);
  });

  it("leaves you reading earlier messages, with a button to jump to the latest", async () => {
    await act(async () => root.render(<Chat />));
    measure();
    await send("Tell me more");
    await nextChunk();
    await act(async () => { scroller().scrollTop = 100; scroller().dispatchEvent(new Event("scroll")); });
    await nextChunk();
    expect(scroller().scrollTop).toBe(100);
    expect(jumpButton()).toBeDefined();
    await act(async () => jumpButton()!.click());
    expect(atBottom()).toBe(true);
    expect(jumpButton()).toBeUndefined();
    expect(windowScrollTo).not.toHaveBeenCalled();
  });

  it("focuses the composer without scrolling anything into view", async () => {
    await act(async () => root.render(<Chat />));
    expect(focus).toHaveBeenCalledWith({ preventScroll: true });
    expect(focus.mock.calls.every((c) => (c[0] as FocusOptions | undefined)?.preventScroll)).toBe(true);
    expect(scrollIntoView).not.toHaveBeenCalled();
  });
});
