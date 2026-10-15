// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { describe, expect, it } from "vitest";
import { sectionFrom, SECTIONS } from "./settingsSections";

describe("Settings sections", () => {
  it("opens a section from a link or a saved choice", () => {
    expect(sectionFrom("#devices")).toBe("devices");
    expect(sectionFrom("security")).toBe("security");
    expect(sectionFrom("")).toBeNull();
    expect(sectionFrom("#nonsense")).toBeNull();
    expect(sectionFrom(null)).toBeNull();
  });

  it("sends the old Pairing and security choice to Devices and pairing", () => {
    expect(sectionFrom("phone")).toBe("devices");
  });

  it("has pairing in one place, and no schedules or energy section", () => {
    const labels = SECTIONS.map((s) => s.label.toLowerCase()).join(" | ");
    expect(labels.match(/pairing/g)).toHaveLength(1);
    expect(labels).not.toMatch(/schedule|energy/);
  });
});
