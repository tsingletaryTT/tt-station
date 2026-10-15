// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Explaining part of a research report: the pure pieces (no React, no DOM),
// so they can be tested with `npm test`. The screen is ResearchExplain.tsx;
// the agent's side is `POST /research/{id}/explain` (research.rs).

export type ExplainMode = "explain" | "simpler" | "ask";

/** The agent's limits (research.rs `MAX_SELECTION`, `MAX_QUESTION`). */
export const MAX_SELECTION = 4000;
export const MAX_QUESTION = 500;

/** A selection out of the rendered report, as the agent will read it: one line, single spaces. */
export const tidySelection = (raw: string) => raw.replace(/\s+/g, " ").trim();

/** Why this selection can't be explained, or null when it can. */
export function selectionProblem(text: string): string | null {
  if (text.length < 2) return "Select some of the report first.";
  if (text.length > MAX_SELECTION) return "That's a lot of the report at once. Select a paragraph or two.";
  return null;
}

/** Citations `[3]` become links to the source list below the report (`#src-3`). */
export const linkCitations = (md: string) => md.replace(/\[(\d{1,3})\](?!\()/g, (_, n) => `[[${n}]](#src-${n})`);

const clip = (s: string, n: number) => (s.length <= n ? s : `${s.slice(0, n).trimEnd()}…`);

/** The panel's heading for a request. */
export function modeTitle(mode: ExplainMode, question?: string): string {
  if (mode === "explain") return "Explained";
  if (mode === "simpler") return "In simpler words";
  return clip(question?.trim() || "Your question", 120);
}

/**
 * A follow-up research question from what was asked about a passage: the
 * question itself, else "go deeper" on the passage (without its citations,
 * which mean nothing to a new search). Always long enough to start one.
 */
export function followUpQuestion(selection: string, mode: ExplainMode, question?: string): string {
  const passage = clip(tidySelection(selection).replace(/\s*\[\d{1,3}\]/g, ""), 160);
  const q = question?.trim() ?? "";
  if (mode === "ask" && q.length >= 8) return `${q} (about: “${clip(passage, 100)}”)`;
  return `Go deeper on: “${passage}”`;
}

export interface Box { top: number; left: number; width: number; height: number }

/**
 * Where the floating menu goes, in the report box's coordinates: centred above
 * the selection, or below it when there's no room above, and never past the
 * box's sides. `sel` and `box` are viewport rects (getBoundingClientRect).
 */
export function menuPlacement(sel: Box, box: Box, menu: { width: number; height: number }, gap = 8): { top: number; left: number; below: boolean } {
  const centre = sel.left + sel.width / 2 - box.left;
  const left = Math.max(0, Math.min(centre - menu.width / 2, box.width - menu.width));
  const above = sel.top - box.top - menu.height - gap;
  // Above needs room in the box and on screen (the box's top may be scrolled away).
  const below = above < 0 || sel.top - menu.height - gap < 0;
  return { top: below ? sel.top - box.top + sel.height + gap : above, left, below };
}
