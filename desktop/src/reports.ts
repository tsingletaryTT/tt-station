// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Research reports as files: their names, and "Download all" as one zip.

import { strToU8, zipSync } from "fflate";

/** A report's file name, from its title. */
export const fileName = (title: string, ext: string) => `${title.replace(/[^\w\- ]+/g, "").trim().slice(0, 60) || "report"}.${ext}`;

/** A finished report: the title that names its file, and its text (none yet if empty). */
export interface ReportText { title: string; text: string | null }

/** One Markdown file per report that has text; a second "Foo" becomes "Foo 2.md". */
export function reportFiles(reports: ReportText[]): Record<string, Uint8Array> {
  const files: Record<string, Uint8Array> = {};
  for (const r of reports) {
    if (!r.text) continue;
    let name = fileName(r.title, "md");
    for (let n = 2; name in files; n++) name = fileName(`${r.title} ${n}`, "md");
    files[name] = strToU8(r.text);
  }
  return files;
}

/** The files as a zip archive. */
export const zipFiles = (files: Record<string, Uint8Array>) => zipSync(files);
