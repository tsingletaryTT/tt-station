// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { strFromU8, unzipSync } from "fflate";
import { describe, expect, it } from "vitest";
import { fileName, reportFiles, zipFiles } from "./reports";

const text = (files: Record<string, Uint8Array>) => Object.fromEntries(Object.entries(files).map(([k, v]) => [k, strFromU8(v)]));

describe("fileName", () => {
  it("keeps letters, digits, spaces, dashes and underscores", () => {
    expect(fileName("Edge VLMs: 2026, which licenses?", "md")).toBe("Edge VLMs 2026 which licenses.md");
    expect(fileName("snake_case-and-dash", "md")).toBe("snake_case-and-dash.md");
  });

  it("drops characters that are unsafe in file names", () => {
    expect(fileName(`a<b>c:d"e/f\\g|h?i*j`, "md")).toBe("abcdefghij.md");
    expect(fileName("../../etc/passwd", "md")).toBe("etcpasswd.md");
  });

  it("trims what's left", () => {
    expect(fileName("  What changed?  ", "md")).toBe("What changed.md");
  });

  it("caps the name at 60 characters before the extension", () => {
    const name = fileName("x".repeat(200), "md");
    expect(name).toBe(`${"x".repeat(60)}.md`);
  });

  it("falls back to 'report' when nothing usable is left", () => {
    expect(fileName("", "md")).toBe("report.md");
    expect(fileName("   ", "md")).toBe("report.md");
    expect(fileName("???!!!", "zip")).toBe("report.zip");
  });
});

describe("reportFiles", () => {
  it("names each report after its title", () => {
    const files = reportFiles([{ title: "Batteries", text: "# Batteries" }, { title: "EU AI Act", text: "# Act" }]);
    expect(text(files)).toEqual({ "Batteries.md": "# Batteries", "EU AI Act.md": "# Act" });
  });

  it("numbers duplicate titles from 2", () => {
    const files = reportFiles([
      { title: "Same", text: "one" },
      { title: "Same", text: "two" },
      { title: "Same?", text: "three" },
    ]);
    expect(text(files)).toEqual({ "Same.md": "one", "Same 2.md": "two", "Same 3.md": "three" });
  });

  it("numbers duplicates of a title cut to length, instead of looping forever", () => {
    const long = "A".repeat(80);
    const files = reportFiles([{ title: long, text: "one" }, { title: long, text: "two" }]);
    expect(text(files)).toEqual({ [`${"A".repeat(60)}.md`]: "one", [`${"A".repeat(60)} 2.md`]: "two" });
  });

  it("numbers duplicates of an unusable title as more reports", () => {
    const files = reportFiles([{ title: "???", text: "one" }, { title: "!!!", text: "two" }]);
    expect(text(files)).toEqual({ "report.md": "one", "report 2.md": "two" });
  });

  it("skips reports with no text", () => {
    const files = reportFiles([{ title: "Empty", text: "" }, { title: "None", text: null }, { title: "Done", text: "ok" }]);
    expect(Object.keys(files)).toEqual(["Done.md"]);
  });

  it("is empty when no report has text", () => {
    expect(reportFiles([{ title: "Empty", text: null }])).toEqual({});
  });
});

describe("zipFiles", () => {
  it("holds one entry per report with its text", () => {
    const reports = [
      { title: "Batteries", text: "# Batteries\n\nPrice per kWh…" },
      { title: "Batteries", text: "# Again" },
      { title: "Skipped", text: null },
      { title: "EU AI Act", text: "# Act ✓" },
    ];
    const zip = zipFiles(reportFiles(reports));
    expect(zip[0]).toBe(0x50); // "PK"
    expect(zip[1]).toBe(0x4b);
    expect(text(unzipSync(zip))).toEqual({
      "Batteries.md": "# Batteries\n\nPrice per kWh…",
      "Batteries 2.md": "# Again",
      "EU AI Act.md": "# Act ✓",
    });
  });
});
