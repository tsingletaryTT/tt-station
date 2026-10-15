// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Needs you: everything waiting for this person -- a Code session's question,
// a routine's action, and for owners a bring-up's command and someone asking
// to pair -- each answered right here or opened where it came from. The
// desktop's version of the phone's "Waiting for you" (`inbox.ts`).
//
// Keyboard: ↑ ↓ (or j k) move between items, A allows, D denies, Enter or O
// opens it where it came from.

import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { Icon } from "@tenstorrent/vesper/icons";
import type { IconKind } from "@tenstorrent/vesper/icons";
import { ago } from "../api";
import { navigate } from "../App";
import { alwaysLabel, answer, type Decision, type InboxItem, type InboxKind } from "../inbox";
import { useInbox } from "../useInbox";
import { Empty, Page, toastError } from "../ui";

const ICONS: Record<InboxKind, IconKind> = { code: "org-developer", routine: "clock", bringup: "ai-task", pairing: "phone" };
const OPEN_LABEL: Record<InboxKind, string> = { code: "Open in Code", routine: "Open the run", bringup: "Open the bring-up", pairing: "Devices and pairing" };
const DONE: Record<Decision, string> = { allow: "Allowed", always: "Allowed from now on", deny: "Denied" };

/** Opens an item where it came from. */
export function openItem(item: InboxItem) {
  navigate(item.open.route, item.open.hash);
}

const clock = (s: number) => `${Math.floor(s / 60)}:${String(Math.max(0, s) % 60).padStart(2, "0")}`;

export function Inbox() {
  const { items, owner, loaded, error, refresh, ignore } = useInbox();
  // Answered here: hidden at once, before the box's next answer confirms it.
  const [answered, setAnswered] = useState<string[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [said, setSaid] = useState("");
  const shown = items.filter((i) => !answered.includes(i.key));
  const [active, setActive] = useState(0);
  const rows = useRef<(HTMLLIElement | null)[]>([]);
  // Keep the active row in range as items come and go.
  useEffect(() => { if (active >= shown.length && shown.length > 0) setActive(shown.length - 1); }, [shown.length, active]);
  // Forget answers the box has caught up with.
  useEffect(() => { setAnswered((a) => a.filter((k) => items.some((i) => i.key === k))); }, [items]);
  // A second tick for pairing codes' countdowns.
  const [, tick] = useState(0);
  const pairing = shown.some((i) => i.kind === "pairing");
  useEffect(() => { if (!pairing) return; const t = window.setInterval(() => tick((n) => n + 1), 1000); return () => window.clearInterval(t); }, [pairing]);

  const focusRow = (i: number) => { setActive(i); rows.current[i]?.focus(); };
  // After an answer, focus goes to what took that one's place (once it has rendered).
  const [focusAfter, setFocusAfter] = useState<number | null>(null);
  useEffect(() => {
    if (focusAfter === null) return;
    setFocusAfter(null);
    if (shown.length > 0) focusRow(Math.min(focusAfter, shown.length - 1));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusAfter, shown.length]);

  const decide = async (item: InboxItem, d: Decision) => {
    if (busy) return;
    const at = shown.findIndex((i) => i.key === item.key);
    const hadFocus = !!rows.current[at]?.contains(document.activeElement);
    setBusy(item.key);
    try {
      await answer(item, d);
      setAnswered((a) => [...a, item.key]);
      setSaid(`${DONE[d]}: ${item.summary}`);
      if (hadFocus) setFocusAfter(at);
      refresh();
    } catch (e) {
      toastError(e);
      refresh();
    } finally { setBusy(null); }
  };

  const onKey = (e: KeyboardEvent<HTMLLIElement>, i: number, item: InboxItem) => {
    // Keys typed into a control inside the row are the control's.
    if (e.target !== e.currentTarget && e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
    if (e.altKey || e.ctrlKey || e.metaKey) return;
    const k = e.key.toLowerCase();
    if (k === "arrowdown" || k === "j") { e.preventDefault(); focusRow(Math.min(shown.length - 1, i + 1)); }
    else if (k === "arrowup" || k === "k") { e.preventDefault(); focusRow(Math.max(0, i - 1)); }
    else if (k === "home") { e.preventDefault(); focusRow(0); }
    else if (k === "end") { e.preventDefault(); focusRow(shown.length - 1); }
    else if (k === "enter" || k === "o") { e.preventDefault(); openItem(item); }
    else if (k === "a" && item.answer === "decide") { e.preventDefault(); void decide(item, "allow"); }
    else if (k === "d" && item.answer === "decide") { e.preventDefault(); void decide(item, "deny"); }
  };

  const what = owner ? "Code, routines, bring-up and pairing" : "Code and routines";
  return (
    <Page title="Needs you" subtitle={`Everything waiting for your OK, from ${what}, in one place. Answer here, or open it where it came from.`}>
      <div className="sr-only" role="status" aria-live="polite">{said}</div>
      {error && <Admonition variant="warning" subtle size="sm">Couldn't check what's waiting: {error}. Trying again.</Admonition>}
      {!loaded ? (
        <div className="page-loading" role="status" aria-label="Loading" />
      ) : shown.length === 0 ? (
        <Empty title="Nothing needs you right now">
          When {owner ? "Code, a routine, a bring-up or someone pairing a device" : "Code or a routine"} needs your OK, it shows up here, with a count in the sidebar, and as a notification.
        </Empty>
      ) : (
        <>
          <ul className="inbox-list" aria-label={`Waiting for you: ${shown.length}`}>
            {shown.map((item, i) => {
              const left = item.expires ? item.expires - Math.floor(Date.now() / 1000) : null;
              const mine = busy === item.key;
              return (
                <li key={item.key} ref={(el) => { rows.current[i] = el; }} className={`inbox-item inbox-${item.kind}`}
                  tabIndex={i === Math.min(active, shown.length - 1) ? 0 : -1} onFocus={(e) => { if (e.target === e.currentTarget) setActive(i); }}
                  onKeyDown={(e) => onKey(e, i, item)} aria-label={`${item.where}: ${item.summary}`}
                  aria-keyshortcuts={item.answer === "decide" ? "A D Enter" : "Enter"}>
                  <Icon kind={ICONS[item.kind]} className="inbox-icon" aria-hidden />
                  <div className="inbox-main">
                    <div className="inbox-where">
                      <span>{item.where}</span>
                      <span className="muted">{item.kind === "pairing" && left != null ? (left > 0 ? `works for ${clock(left)}` : "expired") : ago(item.since)}</span>
                    </div>
                    <div className={`inbox-summary ${item.kind === "code" || item.kind === "bringup" ? "mono" : ""}`}>{item.summary}</div>
                    {item.detail && <div className="inbox-detail muted">{item.detail}</div>}
                    <div className="inbox-actions">
                      {item.answer === "decide" ? (
                        <>
                          <Button size="sm" disabled={!!busy} onClick={() => void decide(item, "allow")}>{mine ? "Answering…" : "Allow"}</Button>
                          {item.canAlways && <Button size="sm" variant="subtle" disabled={!!busy} onClick={() => void decide(item, "always")}>{alwaysLabel(item.kind)}</Button>}
                          <Button size="sm" variant="ghost" disabled={!!busy} onClick={() => void decide(item, "deny")}>Deny</Button>
                          <span className="grow" />
                          <Button size="sm" variant="ghost" onClick={() => openItem(item)}>{OPEN_LABEL[item.kind]}</Button>
                        </>
                      ) : (
                        <>
                          <Button size="sm" variant="subtle" onClick={() => openItem(item)}>{OPEN_LABEL[item.kind]}</Button>
                          <Button size="sm" variant="ghost" onClick={() => ignore(item.key)}>Ignore</Button>
                          <span className="muted inbox-hint">Read the code out to whoever is pairing. If nobody you know is, ignore it: without the code, nothing pairs.</span>
                        </>
                      )}
                    </div>
                  </div>
                </li>
              );
            })}
          </ul>
          <p className="muted inbox-keys" aria-hidden>
            <span className="kbd">↑</span> <span className="kbd">↓</span> move · <span className="kbd">A</span> allow · <span className="kbd">D</span> deny · <span className="kbd">Enter</span> open
          </p>
        </>
      )}
    </Page>
  );
}
