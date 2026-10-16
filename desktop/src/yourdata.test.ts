// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { describe, expect, it } from "vitest";
import { changesSomething, describePreview, exportFileName, type ImportPreview, retentionDays, retentionValue, MODEL_CONTROL_CHOICES, modelControlSaved } from "./yourdata";

const preview = (categories: ImportPreview["categories"], skipped: string[] = []): ImportPreview => ({
  agent_version: "0.11.0", format_version: 1, created: 1, box: "QuietBox", categories, overwritten: [], skipped, bytes: 10,
});

describe("describePreview", () => {
  it("says what's added and what's replaced, in plain words", () => {
    const p = preview({
      transcripts: { added: 2, overwritten: 0, unchanged: 0 },
      memories: { added: 1, overwritten: 0, unchanged: 3 },
      prefs: { added: 0, overwritten: 1, unchanged: 0 },
    }, ["app-keys.json: not something an export holds"]);
    expect(describePreview(p)).toBe(
      "Adds 2 transcripts and 1 memory. Replaces 1 settings file with the export's copy. Leaves out 1 file it can't bring in.",
    );
    expect(changesSomething(p)).toBe(true);
  });

  it("says when there's nothing new", () => {
    const p = preview({ memories: { added: 0, overwritten: 0, unchanged: 3 } });
    expect(describePreview(p)).toBe("This export from QuietBox has nothing this box doesn't already have.");
    expect(changesSomething(p)).toBe(false);
  });
});

describe("exportFileName", () => {
  it("uses the box's name for the file, cleaned", () => {
    expect(exportFileName('attachment; filename="tt-station-qb2-2026-10-15.tar.gz"')).toBe("tt-station-qb2-2026-10-15.tar.gz");
    expect(exportFileName('attachment; filename="../../x/evil.tar.gz"', new Date("2026-10-15T12:00:00Z"))).toBe("tt-station-2026-10-15.tar.gz");
  });

  it("falls back to a dated name", () => {
    expect(exportFileName(null, new Date("2026-10-15T12:00:00Z"))).toBe("tt-station-2026-10-15.tar.gz");
    expect(exportFileName('attachment; filename="x.exe"', new Date("2026-10-15T12:00:00Z"))).toBe("tt-station-2026-10-15.tar.gz");
  });
});

describe("retention", () => {
  it("maps settings to choices and back", () => {
    expect(retentionValue(null)).toBe("keep");
    expect(retentionValue(90)).toBe("90");
    expect(retentionDays("keep")).toBeNull();
    expect(retentionDays("365")).toBe(365);
  });
});

describe("who may start and stop models", () => {
  it("offers anyone (the default) or owners only, and says what changed", () => {
    expect(MODEL_CONTROL_CHOICES.map((c) => c.value)).toEqual(["anyone", "owners"]);
    expect(modelControlSaved("owners")).toContain("Only owners");
    expect(modelControlSaved("anyone")).toContain("Anyone");
  });
});
