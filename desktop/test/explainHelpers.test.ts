// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// `npm test`: the pure helpers behind explaining part of a research report.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  followUpQuestion, linkCitations, MAX_SELECTION, menuPlacement, modeTitle, selectionProblem, tidySelection,
} from "../src/pages/explainHelpers.ts";

test("selections are tidied and bounded like the agent does", () => {
  assert.equal(tidySelection("  Tesla Powerwall 3\t10 Years\n\n70% [2] "), "Tesla Powerwall 3 10 Years 70% [2]");
  assert.equal(selectionProblem("x"), "Select some of the report first.");
  assert.equal(selectionProblem("ok"), null);
  assert.equal(selectionProblem("a".repeat(MAX_SELECTION)), null);
  assert.match(selectionProblem("a".repeat(MAX_SELECTION + 1)) ?? "", /a lot of the report/);
});

test("citations link to the sources, but real links are left alone", () => {
  assert.equal(linkCitations("Ten years [2][29]."), "Ten years [[2]](#src-2)[[29]](#src-29).");
  assert.equal(linkCitations("[see](https://x) [1000]"), "[see](https://x) [1000]");
});

test("panel titles", () => {
  assert.equal(modeTitle("explain"), "Explained");
  assert.equal(modeTitle("simpler"), "In simpler words");
  assert.equal(modeTitle("ask", "  Is it transferable? "), "Is it transferable?");
  assert.equal(modeTitle("ask", "q".repeat(200)).length, 121);
});

test("follow-up questions are long enough to start research and drop citations", () => {
  assert.equal(followUpQuestion("Ten  years [2][29].", "explain"), "Go deeper on: “Ten years.”");
  assert.equal(followUpQuestion("Ten years [2].", "ask", "Is the warranty transferable?"), "Is the warranty transferable? (about: “Ten years.”)");
  // A question too short to research on its own falls back to the passage.
  assert.equal(followUpQuestion("Ten years", "ask", "why?"), "Go deeper on: “Ten years”");
  const long = followUpQuestion("word ".repeat(200), "simpler");
  assert.ok(long.length < 200 && long.length >= 8, long);
});

test("the menu sits above the selection, inside the box, or below when there's no room", () => {
  const box = { top: 100, left: 50, width: 600, height: 800 };
  const menu = { width: 300, height: 40 };
  // Room above: centred on the selection.
  assert.deepEqual(menuPlacement({ top: 400, left: 250, width: 200, height: 20 }, box, menu), { top: 252, left: 150, below: false });
  // Near the box's left and right edges: clamped.
  assert.equal(menuPlacement({ top: 400, left: 50, width: 10, height: 20 }, box, menu).left, 0);
  assert.equal(menuPlacement({ top: 400, left: 640, width: 10, height: 20 }, box, menu).left, 300);
  // At the top of the box: below the selection instead.
  assert.deepEqual(menuPlacement({ top: 110, left: 250, width: 200, height: 20 }, box, menu), { top: 38, left: 150, below: true });
  // The box scrolled up past the screen's top, selection near the screen's top: below.
  assert.equal(menuPlacement({ top: 20, left: 250, width: 200, height: 20 }, { ...box, top: -500 }, menu).below, true);
});
