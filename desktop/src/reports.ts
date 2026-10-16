// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Research reports as files: their names, and "Download all" as one zip.
// And whose a report is: who started it, and whether it's shared.

import { strToU8, zipSync } from "fflate";

/** A report's file name without its extension: safe characters only, 60 at most. */
const stem = (title: string) => title.replace(/[^\w\- ]+/g, "").trim().slice(0, 60) || "report";

/** A report's file name, from its title. */
export const fileName = (title: string, ext: string) => `${stem(title)}.${ext}`;

/** A finished report: the title that names its file, and its text (none yet if empty). */
export interface ReportText { title: string; text: string | null }

/** One Markdown file per report that has text; a second "Foo" becomes "Foo 2.md". */
export function reportFiles(reports: ReportText[]): Record<string, Uint8Array> {
  const files: Record<string, Uint8Array> = {};
  for (const r of reports) {
    if (!r.text) continue;
    // The number goes on after the name is cut to length, or a long title's
    // " 2" would be cut off and the name would never change.
    const base = stem(r.title);
    let name = `${base}.md`;
    for (let n = 2; name in files; n++) name = `${base} ${n}.md`;
    files[name] = strToU8(r.text);
  }
  return files;
}

/** The files as a zip archive. */
export const zipFiles = (files: Record<string, Uint8Array>) => zipSync(files);

/** Whose a report is, as the box says (`GET /research`). */
export interface Ownership { started_by?: string | null; started_by_name?: string | null; shared?: boolean }

/**
 * The small line under a report in the list: "Shared", who started it (for
 * an owner, who sees everyone's), or nothing for one's own private report.
 * `owner` is whether this device is an owner; `mine` whether it started it.
 */
export function ownershipLabel(r: Ownership, mine: boolean, owner: boolean): string | null {
  const by = !mine && owner ? (r.started_by_name ?? (r.started_by ? "another device" : "the box")) : null;
  if (r.shared) return by ? `Shared · ${by}` : "Shared";
  return by ? `By ${by}` : null;
}

/** The share action for a report this caller may change; `null` when it may not. */
export function shareAction(r: Ownership, canChange: boolean): { label: string; shared: boolean } | null {
  if (!canChange) return null;
  return r.shared
    ? { label: "Stop sharing", shared: false }
    : { label: "Share with everyone on the box", shared: true };
}
