// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Starting outside the sandbox in a folder a member's sandboxed Code session
// worked in. Whatever was left there (git hooks, a Makefile, npm scripts,
// Claude settings) would run as the box's own account, so the box answers 409
// `needs_confirm: "shared_folder"` and the owner confirms first
// (`workspace.rs`). These start the session, and ask when the box does.

import { ago, ApiError, post } from "./api";
import { ask } from "./dialog";

export interface SharedUse { who: string; when: number; folder?: string }
export interface SharedFolderReply { error?: string; needs_confirm?: string; used_by?: SharedUse[]; folder?: string }

/** The question to ask for a 409 from the box, or null when it's some other refusal. */
export function sharedFolderQuestion(e: unknown): { title: string; body: string } | null {
  if (!(e instanceof ApiError) || e.status !== 409) return null;
  const b = e.body as SharedFolderReply | undefined;
  if (b?.needs_confirm !== "shared_folder") return null;
  const uses = b.used_by ?? [];
  const names = [...new Set(uses.map((u) => u.who).filter(Boolean))];
  const who = names.length === 0 ? "Someone" : names.length === 1 ? names[0] : `${names.slice(0, -1).join(", ")} and ${names.at(-1)}`;
  const inside = uses.find((u) => u.folder && b.folder && u.folder !== b.folder)?.folder;
  const when = uses[0]?.when ? ` Last used ${ago(uses[0].when)}${inside ? `, in ${inside}` : ""}.` : "";
  return {
    title: `${who} worked in this folder in the sandbox`,
    body: `${b.error ?? `${who} used this folder in Code's sandbox. Anything left in it would run as the box's own account.`}${when}`,
  };
}

/** POSTs a session start; on the shared-folder 409 asks, then starts again with `confirm_shared`. Null if they chose not to. */
export async function startConfirmingSharedFolder<T>(path: string, body: Record<string, unknown>): Promise<T | null> {
  try {
    return await post<T>(path, body);
  } catch (e) {
    const q = sharedFolderQuestion(e);
    if (!q) throw e;
    const go = await ask(q.title, { body: q.body, confirm: "Start outside the sandbox anyway", cancel: "Don't start", danger: true });
    if (!go) return null;
    return post<T>(path, { ...body, confirm_shared: true });
  }
}
