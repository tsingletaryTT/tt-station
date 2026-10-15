// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { describe, expect, it } from "vitest";
import { ApiError } from "./api";
import { sharedFolderQuestion } from "./sharedFolder";

describe("starting outside the sandbox where a member worked", () => {
  it("asks only for the box's shared-folder refusal", () => {
    expect(sharedFolderQuestion(new ApiError(409, "Busy", { error: "Busy" }))).toBeNull();
    expect(sharedFolderQuestion(new ApiError(403, "No", { needs_confirm: "shared_folder" }))).toBeNull();
    expect(sharedFolderQuestion(new Error("boom"))).toBeNull();
  });

  it("names who worked there and says what could run", () => {
    const now = Math.floor(Date.now() / 1000);
    const q = sharedFolderQuestion(new ApiError(409, "x", {
      error: "Max used this folder in Code's sandbox. Anything left in it (git hooks, a Makefile, npm scripts, Claude settings) would run as the box's own account.",
      needs_confirm: "shared_folder",
      folder: "/home/u/proj",
      used_by: [{ who: "Max", when: now - 7200, folder: "/home/u/proj/web" }, { who: "Ada", when: now - 9000, folder: "/home/u/proj" }, { who: "Max", when: now - 10000 }],
    }));
    expect(q?.title).toBe("Max and Ada worked in this folder in the sandbox");
    expect(q?.body).toContain("git hooks");
    expect(q?.body).toContain("Last used 2 h ago, in /home/u/proj/web.");
  });
});
