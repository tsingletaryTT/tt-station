// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// What the notifications dropdown and the full Needs you page share: how an
// item looks (icon, age, where Open goes), answering one (hidden at once,
// before the box's next poll confirms it), and the keys on a row.

import { type KeyboardEvent, useEffect, useState } from "react";
import type { IconKind } from "@tenstorrent/vesper/icons";
import { ago } from "./api";
import { navigate } from "./App";
import { answer, type Decision, type InboxItem, type InboxKind } from "./inbox";
import { toastError } from "./ui";

export const INBOX_ICONS: Record<InboxKind, IconKind> = { code: "org-developer", routine: "clock", bringup: "ai-task", pairing: "phone" };
export const OPEN_LABEL: Record<InboxKind, string> = { code: "Open in Code", routine: "Open the run", bringup: "Open the bring-up", pairing: "Devices and pairing" };
const DONE: Record<Decision, string> = { allow: "Allowed", always: "Allowed from now on", deny: "Denied" };

/** Opens an item where it came from. */
export function openItem(item: InboxItem) {
  navigate(item.open.route, item.open.hash);
}

const clock = (s: number) => `${Math.floor(s / 60)}:${String(Math.max(0, s) % 60).padStart(2, "0")}`;

/** "3m ago", or a pairing code's time left. */
export function itemAge(item: InboxItem): string {
  if (item.kind === "pairing" && item.expires) {
    const left = item.expires - Math.floor(Date.now() / 1000);
    return left > 0 ? `works for ${clock(left)}` : "expired";
  }
  return ago(item.since);
}

/** A second tick while a pairing code counts down. */
export function usePairingTick(items: InboxItem[]) {
  const [, tick] = useState(0);
  const pairing = items.some((i) => i.kind === "pairing");
  useEffect(() => { if (!pairing) return; const t = window.setInterval(() => tick((n) => n + 1), 1000); return () => window.clearInterval(t); }, [pairing]);
}

/** Answering items: what's still shown, which one is being answered, and what to announce. */
export function useAnswering(items: InboxItem[], refresh: () => void) {
  const [answered, setAnswered] = useState<string[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [said, setSaid] = useState("");
  const shown = items.filter((i) => !answered.includes(i.key));
  // Forget answers the box has caught up with.
  useEffect(() => { setAnswered((a) => a.filter((k) => items.some((i) => i.key === k))); }, [items]);
  /** Resolves true once answered. */
  const decide = async (item: InboxItem, d: Decision): Promise<boolean> => {
    if (busy) return false;
    setBusy(item.key);
    try {
      await answer(item, d);
      setAnswered((a) => [...a, item.key]);
      setSaid(`${DONE[d]}: ${item.summary}`);
      refresh();
      return true;
    } catch (e) {
      toastError(e);
      refresh();
      return false;
    } finally { setBusy(null); }
  };
  return { shown, busy, said, decide };
}

/**
 * The keys on a row: ↑ ↓ (or j k), Home and End move; A allows, D denies,
 * Enter or O opens. Keys typed into a control inside the row are the control's.
 */
export function onRowKey(e: KeyboardEvent<HTMLElement>, i: number, item: InboxItem, to: {
  count: number; focusRow: (i: number) => void; open: (item: InboxItem) => void; decide: (item: InboxItem, d: Decision) => void;
}) {
  if (e.target !== e.currentTarget && e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
  if (e.altKey || e.ctrlKey || e.metaKey) return;
  const k = e.key.toLowerCase();
  if (k === "arrowdown" || k === "j") { e.preventDefault(); to.focusRow(Math.min(to.count - 1, i + 1)); }
  else if (k === "arrowup" || k === "k") { e.preventDefault(); to.focusRow(Math.max(0, i - 1)); }
  else if (k === "home") { e.preventDefault(); to.focusRow(0); }
  else if (k === "end") { e.preventDefault(); to.focusRow(to.count - 1); }
  else if (k === "enter" || k === "o") { e.preventDefault(); to.open(item); }
  else if (k === "a" && item.answer === "decide") { e.preventDefault(); to.decide(item, "allow"); }
  else if (k === "d" && item.answer === "decide") { e.preventDefault(); to.decide(item, "deny"); }
}
