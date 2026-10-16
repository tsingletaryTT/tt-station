// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { distanceFromBottom, isNearBottom, JumpToLatest, NEAR_BOTTOM_PX, scrollToBottom, useStickToBottom } from "./stickToBottom";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

/** A scroll box jsdom can measure: 400px tall, its content as tall as `height`. */
function fakeBox(height: () => number, client = 400) {
  const el = document.createElement("div");
  Object.defineProperty(el, "clientHeight", { configurable: true, get: () => client });
  Object.defineProperty(el, "scrollHeight", { configurable: true, get: () => height() });
  return el;
}

let reduced = false;
beforeEach(() => {
  reduced = false;
  window.matchMedia = ((q: string) => ({ matches: reduced && q.includes("reduce"), media: q })) as unknown as typeof window.matchMedia;
});

describe("measuring", () => {
  it("knows how far from the bottom a box is, and what counts as near it", () => {
    const el = fakeBox(() => 2000);
    el.scrollTop = 1600;
    expect(distanceFromBottom(el)).toBe(0);
    expect(isNearBottom(el)).toBe(true);
    el.scrollTop = 1600 - NEAR_BOTTOM_PX;
    expect(isNearBottom(el)).toBe(true);
    el.scrollTop = 1600 - NEAR_BOTTOM_PX - 1;
    expect(isNearBottom(el)).toBe(false);
  });

  it("scrolls the box itself, smoothly only when motion is welcome", () => {
    const el = fakeBox(() => 2000);
    el.scrollTo = vi.fn() as unknown as typeof el.scrollTo;
    scrollToBottom(el);
    expect(el.scrollTop).toBe(2000);
    expect(el.scrollTo).not.toHaveBeenCalled();
    scrollToBottom(el, true);
    expect(el.scrollTo).toHaveBeenCalledWith({ top: 2000, behavior: "smooth" });
    reduced = true;
    el.scrollTop = 0;
    scrollToBottom(el, true);
    expect(el.scrollTop).toBe(2000);
    expect(el.scrollTo).toHaveBeenCalledTimes(1);
  });
});

describe("useStickToBottom", () => {
  let root: Root;
  let host: HTMLDivElement;
  let height = 1000;
  // The hook's box: measured by fakeBox's rules, rendered by React.
  const box = fakeBox(() => height);
  let api: ReturnType<typeof useStickToBottom<HTMLDivElement>>;

  function Thread({ items, conv }: { items: number; conv: string }) {
    api = useStickToBottom<HTMLDivElement>(items, conv);
    return (
      <>
        <div ref={(el) => { (api.ref as { current: HTMLDivElement | null }).current = el ? box : null; }} onScroll={api.onScroll} />
        <JumpToLatest show={api.unseen} onClick={api.jump} />
      </>
    );
  }
  const render = (items: number, conv = "a") => act(() => root.render(<Thread items={items} conv={conv} />));
  const userScrollsTo = (top: number) => act(() => { box.scrollTop = top; api.onScroll(); });
  const jumpButton = () => host.querySelector<HTMLButtonElement>(".jump-latest");

  beforeEach(() => {
    height = 1000;
    box.scrollTop = 0;
    box.scrollTo = ((o: ScrollToOptions) => { box.scrollTop = o.top ?? 0; }) as typeof box.scrollTo;
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
  });
  afterEach(() => { act(() => root.unmount()); host.remove(); });

  it("opens at the newest message and follows new ones while you're at the bottom", () => {
    render(1);
    expect(box.scrollTop).toBe(1000);
    height = 1500;
    render(2);
    expect(box.scrollTop).toBe(1500);
    expect(jumpButton()).toBeNull();
  });

  it("leaves you where you are while you read back, and offers to jump to the latest", () => {
    render(1);
    userScrollsTo(200);
    height = 1600;
    render(2);
    expect(box.scrollTop).toBe(200);
    expect(jumpButton()).not.toBeNull();
    act(() => jumpButton()!.click());
    expect(box.scrollTop).toBe(1600);
    expect(jumpButton()).toBeNull();
    // Following again: the next message comes into view by itself.
    height = 1900;
    render(3);
    expect(box.scrollTop).toBe(1900);
  });

  it("counts a little above the bottom as the bottom", () => {
    render(1);
    userScrollsTo(1000 - 400 - 30);
    height = 1300;
    render(2);
    expect(box.scrollTop).toBe(1300);
  });

  it("hides the button once you scroll down to the latest yourself", () => {
    render(1);
    userScrollsTo(0);
    height = 1400;
    render(2);
    expect(jumpButton()).not.toBeNull();
    userScrollsTo(1000);
    expect(jumpButton()).toBeNull();
  });

  it("follows again after you send something, and starts at the end of another conversation", () => {
    render(1);
    userScrollsTo(0);
    act(() => api.stick());
    height = 1200;
    render(2);
    expect(box.scrollTop).toBe(1200);
    userScrollsTo(0);
    render(2, "b");
    expect(box.scrollTop).toBe(1200);
    expect(jumpButton()).toBeNull();
  });
});
