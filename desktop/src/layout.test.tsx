// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { readFileSync } from "node:fs";
import { act } from "react";
import { createRoot } from "react-dom/client";
import { describe, expect, it, vi } from "vitest";
import { FullHeightContext, useFullHeight } from "./layout";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

describe("useFullHeight", () => {
  it("asks the shell for the full height while the page wants it, and gives it back when it leaves", () => {
    const set = vi.fn();
    function Pane({ on }: { on: boolean }) { useFullHeight(on); return null; }
    const host = document.createElement("div");
    const root = createRoot(host);
    act(() => root.render(<FullHeightContext.Provider value={set}><Pane on /></FullHeightContext.Provider>));
    expect(set).toHaveBeenLastCalledWith(true);
    act(() => root.render(<FullHeightContext.Provider value={set}><Pane on={false} /></FullHeightContext.Provider>));
    expect(set).toHaveBeenLastCalledWith(false);
    act(() => root.render(<FullHeightContext.Provider value={set}><Pane on /></FullHeightContext.Provider>));
    act(() => root.unmount());
    expect(set).toHaveBeenLastCalledWith(false);
  });
});

// The rules that keep the window from scrolling (jsdom has no layout, so they're checked as written).
describe("the shell's CSS", () => {
  // Read from disk: the test runner hands CSS imports over empty. Tests run from desktop/.
  const css = readFileSync(`${process.cwd()}/src/app.css`, "utf8");
  const rule = (selector: string) => {
    const at = css.indexOf(`\n${selector} {`);
    expect(at, `${selector} is styled`).toBeGreaterThanOrEqual(0);
    return css.slice(at, css.indexOf("}", at));
  };

  it("never lets the window scroll", () => {
    expect(rule("html, body")).toMatch(/overflow: hidden/);
    // One row exactly the window's height: an implicit (auto) row would grow with the page.
    expect(rule(".shell")).toMatch(/grid-template-rows: minmax\(0, 1fr\)/);
    expect(rule(".shell")).toMatch(/overflow: clip/);
  });

  it("scrolls the sidebar's places, the page area, and a conversation's panes on their own", () => {
    expect(rule(".nav")).toMatch(/min-height: 0;[^}]*overflow-y: auto/);
    expect(rule(".page-area")).toMatch(/min-height: 0;[^}]*overflow-y: auto/);
    expect(rule(".chat")).toMatch(/grid-template-rows: minmax\(0, 1fr\)/);
    expect(rule(".chat-list")).toMatch(/overflow-y: auto;[^}]*min-height: 0/);
    expect(rule(".chat-main > .chat-scroll, .chat-main > .scroll-pane")).toMatch(/min-height: 0/);
    expect(rule(".chat-scroll")).toMatch(/overflow-y: auto/);
  });
});
