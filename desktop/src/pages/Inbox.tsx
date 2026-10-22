// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Notifications: everything waiting for this person -- a Code session's question,
// a routine's action, and for owners someone asking to pair -- each answered right here or opened where it came from. The
// desktop's version of the phone's "Waiting for you" (`inbox.ts`). The bell at
// the top right of the window shows the same list as a dropdown, newest first;
// this page is its "See everything".
//
// Keyboard: ↑ ↓ (or j k) move between items, A allows, D denies, Enter or O
// opens it where it came from.

import { useEffect, useRef, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { Icon } from "@tenstorrent/vesper/icons";
import { type Decision, type InboxItem } from "../inbox";
import { INBOX_ICONS, itemAge, onRowKey, OPEN_LABEL, openItem, useAnswering, usePairingTick } from "../inboxActions";
import { useInbox } from "../useInbox";
import { Empty, Page } from "../ui";

export function Inbox() {
  const { items, owner, loaded, error, refresh, ignore } = useInbox();
  const { shown, busy, said, decide: answerItem } = useAnswering(items, refresh);
  const [active, setActive] = useState(0);
  const rows = useRef<(HTMLLIElement | null)[]>([]);
  // Keep the active row in range as items come and go.
  useEffect(() => { if (active >= shown.length && shown.length > 0) setActive(shown.length - 1); }, [shown.length, active]);
  usePairingTick(shown);

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
    const at = shown.findIndex((i) => i.key === item.key);
    const hadFocus = !!rows.current[at]?.contains(document.activeElement);
    if (await answerItem(item, d) && hadFocus) setFocusAfter(at);
  };

  const what = owner ? "Code, routines and pairing" : "Code and routines";
  return (
    <Page title="Notifications" subtitle={`Everything waiting for your OK, from ${what}, in one place. Answer here, or open it where it came from.`}>
      <div className="sr-only" role="status" aria-live="polite">{said}</div>
      {error && <Admonition variant="warning" subtle size="sm">Couldn't check what's waiting: {error}. Trying again.</Admonition>}
      {!loaded ? (
        <div className="page-loading" role="status" aria-label="Loading" />
      ) : shown.length === 0 ? (
        <Empty title="You're all caught up">
          When {owner ? "Code, a routine or someone pairing a device" : "Code or a routine"} needs your OK, it shows up here and under the bell at the top right, with a count, and as a notification.
        </Empty>
      ) : (
        <>
          <ul className="inbox-list" aria-label={`Waiting for you: ${shown.length}`}>
            {shown.map((item, i) => {
              const mine = busy === item.key;
              return (
                <li key={item.key} ref={(el) => { rows.current[i] = el; }} className={`inbox-item inbox-${item.kind}`}
                  tabIndex={i === Math.min(active, shown.length - 1) ? 0 : -1} onFocus={(e) => { if (e.target === e.currentTarget) setActive(i); }}
                  onKeyDown={(e) => onRowKey(e, i, item, { count: shown.length, focusRow, open: openItem, decide: (it, d) => void decide(it, d) })} aria-label={`${item.where}: ${item.summary}`}
                  aria-keyshortcuts={item.answer === "decide" ? "A D Enter" : "Enter"}>
                  <Icon kind={INBOX_ICONS[item.kind]} className="inbox-icon" aria-hidden />
                  <div className="inbox-main">
                    <div className="inbox-where">
                      <span>{item.where}</span>
                      <span className="muted">{itemAge(item)}</span>
                    </div>
                    <div className={`inbox-summary ${item.kind === "code" ? "mono" : ""}`}>{item.summary}</div>
                    {item.detail && <div className="inbox-detail muted">{item.detail}</div>}
                    <div className="inbox-actions">
                      {item.answer === "decide" ? (
                        <>
                          <Button size="sm" disabled={!!busy} onClick={() => void decide(item, "allow")}>{mine ? "Answering…" : "Allow"}</Button>
                          {item.canAlways && <Button size="sm" variant="subtle" disabled={!!busy} onClick={() => void decide(item, "always")}>Always allow</Button>}
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
