// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { describe, expect, it } from "vitest";
import type { Mode } from "./api";
import { firstRun, shouldShow } from "./firstRun";

const mode = (id: string, over: Partial<Mode> = {}): Mode => ({
  id, title: id, blurb: "", icon: "", models: [`${id}-model`], ready: false, needs_download: [`${id}-model`], active: false, eta_s: 60, note: null, ...over,
});

describe("first run", () => {
  it("shows on a box nobody has used yet, and not before the box has said", () => {
    expect(shouldShow({}, 0)).toBe(true);
    expect(shouldShow({}, null)).toBe(false);
    expect(shouldShow({}, 12)).toBe(false);
  });
  it("once shown it stays until it's put away", () => {
    expect(shouldShow({ shown: true }, 3)).toBe(true);
    expect(shouldShow({ shown: true, dismissed: true }, 0)).toBe(false);
  });
  it("walks choose → get → try from what the box reports", () => {
    const modes = [mode("chat"), mode("images", { ready: true })];
    expect(firstRun({}, modes, 0)).toMatchObject({ choose: "current", get: "later", try: "later", complete: false });
    expect(firstRun({ mode: "chat" }, modes, 0)).toMatchObject({ choose: "done", get: "current", try: "later" });
    expect(firstRun({ mode: "images" }, modes, 0)).toMatchObject({ choose: "done", get: "done", try: "current" });
    expect(firstRun({ mode: "images" }, modes, 1)).toMatchObject({ complete: true });
  });
  it("an active mode counts as chosen", () => {
    expect(firstRun({}, [mode("chat", { ready: true, active: true })], 0).mode?.id).toBe("chat");
  });
});
