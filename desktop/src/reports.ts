// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Research reports as files: their names, and "Download all" as one zip.

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
