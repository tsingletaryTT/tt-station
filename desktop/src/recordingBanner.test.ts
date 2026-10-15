// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { describe, expect, it } from "vitest";
import { recordingBannerText } from "./recordingBanner";

describe("recordingBannerText", () => {
  it("says who started it and for how long", () => {
    const t = recordingBannerText({ started: 1000, seconds: 3, ended: false, started_by: "Max's iPhone" }, 1000 + 125);
    expect(t.lead).toBe("Recording in this room");
    expect(t.meta).toEqual(["started by Max's iPhone", "2:05"]);
  });
  it("still says something when the box doesn't know who", () => {
    const t = recordingBannerText({ started: 0, seconds: 3700, ended: false, started_by: null }, 0);
    expect(t.meta).toEqual(["started from a paired device", "1:01:40"]);
  });
  it("says when it stopped on its own", () => {
    const t = recordingBannerText({ started: 0, seconds: 10, ended: true, started_by: "Ana" }, 50);
    expect(t.lead).toMatch(/stopped on its own/);
    expect(t.meta[0]).toBe("started by Ana");
  });
});
