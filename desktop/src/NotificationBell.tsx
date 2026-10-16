// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The bell at the top right of the window: how many things are waiting for
// this person (Notifications, `inbox.ts`), and a dropdown of them, newest first,
// answered right there or opened where they came from. "See everything" opens
// the full Notifications page.
//
// The dropdown closes on Escape, a click outside, focus leaving it, or going
// somewhere; focus goes into it on open and back to the bell on close. In it,
// ↑ ↓ (or j k) move between items, A allows, D denies, Enter opens.

import { type SVGProps, useEffect, useRef, useState } from "react";
import { Badge } from "@tenstorrent/vesper/badge";
import { Button } from "@tenstorrent/vesper/button";
import { IconButton } from "@tenstorrent/vesper/icon-button";
import { Icon } from "@tenstorrent/vesper/icons";
import { navigate } from "./App";
import { badgeText, bellLabel, newestFirst, type Decision, type InboxItem } from "./inbox";
import { INBOX_ICONS, itemAge, onRowKey, openItem, useAnswering, usePairingTick } from "./inboxActions";
import { useInbox } from "./useInbox";

/** Vesper has no bell; this one is drawn in its icons' manner (24 grid, square joins, one stroke). */
function Bell(props: SVGProps<SVGSVGElement>) {
  return (
    <svg viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg" {...props}>
      <path d="M12 3.5L16.5 6V11.5L18.5 16.5H5.5L7.5 11.5V6L12 3.5Z" stroke="currentColor" strokeLinejoin="miter" />
      <path d="M10 19H14L13 20.5H11L10 19Z" stroke="currentColor" />
    </svg>
  );
}

export function NotificationBell() {
  const { items, owner, loaded, error, refresh, ignore } = useInbox();
  const { shown, busy, said, decide: answerItem } = useAnswering(items, refresh);
  const list = newestFirst(shown);
  const count = list.length;
  const [open, setOpen] = useState(false);
  const wrap = useRef<HTMLDivElement>(null);
  const bell = useRef<HTMLButtonElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  const rows = useRef<(HTMLLIElement | null)[]>([]);
  const [active, setActive] = useState(0);
  usePairingTick(open ? list : []);

  const close = (focusBell: boolean) => {
    setOpen(false);
    if (focusBell) bell.current?.focus();
  };

  // New arrivals: a pulse on the badge and a polite word (not for the first load).
  const before = useRef<number | null>(null);
  const [pulse, setPulse] = useState(0);
  const [heard, setHeard] = useState("");
  useEffect(() => {
    if (!loaded) return;
    if (before.current !== null && count > before.current) {
      setPulse((n) => n + 1);
      setHeard(`${count - before.current === 1 ? "Something new needs you" : `${count - before.current} new things need you`}. ${count} waiting.`);
    }
    before.current = count;
  }, [count, loaded]);
  useEffect(() => { if (said) setHeard(said); }, [said]);

  // Going somewhere closes it (a place, or a session in this one).
  useEffect(() => {
    if (!open) return;
    const shut = () => setOpen(false);
    const outside = (e: MouseEvent) => { if (!wrap.current?.contains(e.target as Node)) shut(); };
    const esc = (e: KeyboardEvent) => { if (e.key === "Escape") { e.preventDefault(); close(true); } };
    window.addEventListener("popstate", shut);
    window.addEventListener("mousedown", outside);
    window.addEventListener("keydown", esc);
    return () => { window.removeEventListener("popstate", shut); window.removeEventListener("mousedown", outside); window.removeEventListener("keydown", esc); };
  }, [open]);

  // On open, focus goes to the newest item (or the panel, when there's none).
  useEffect(() => {
    if (!open) return;
    setActive(0);
    (rows.current[0] ?? panel.current)?.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);
  useEffect(() => { if (active >= count && count > 0) setActive(count - 1); }, [count, active]);

  const focusRow = (i: number) => { setActive(i); rows.current[i]?.focus(); };
  const decide = async (item: InboxItem, d: Decision) => {
    const at = list.findIndex((i) => i.key === item.key);
    const hadFocus = !!rows.current[at]?.contains(document.activeElement);
    if (await answerItem(item, d) && hadFocus) {
      // What took its place, once it has rendered; the panel itself when none is left.
      requestAnimationFrame(() => (rows.current[Math.min(at, rows.current.filter(Boolean).length - 1)] ?? panel.current)?.focus());
    }
  };
  const go = (item: InboxItem) => { close(true); openItem(item); };
  const all = () => { close(true); navigate("inbox"); };

  const text = badgeText(count);
  return (
    <div className="bell" ref={wrap}
      onBlur={(e) => { if (open && e.relatedTarget && !wrap.current?.contains(e.relatedTarget as Node)) setOpen(false); }}>
      <IconButton ref={bell} icon={<Bell />} variant="ghost" size="sm" className="bell-button"
        aria-label={bellLabel(count)} title="Notifications" aria-haspopup="dialog" aria-expanded={open} aria-controls={open ? "bell-panel" : undefined}
        onClick={() => (open ? close(true) : setOpen(true))} />
      {text && (
        <Badge key={pulse} as="span" size="sm" variant="warning" className={`bell-count ${pulse ? "bell-pulse" : ""}`} aria-hidden>{text}</Badge>
      )}
      <div className="sr-only" role="status" aria-live="polite">{heard}</div>
      {open && (
        <div id="bell-panel" ref={panel} className="bell-panel" role="dialog" aria-label="Notifications" tabIndex={-1}>
          <div className="bell-head">
            <span className="bell-title">Notifications</span>
            {count > 0 && <span className="muted">{count} waiting</span>}
          </div>
          {error && <div className="bell-error">Couldn't check what's waiting. Trying again.</div>}
          {!loaded ? (
            <div className="bell-empty muted">Checking…</div>
          ) : count === 0 ? (
            <div className="bell-empty">
              <b>You're all caught up</b>
              <span className="muted">When {owner ? "Code, a routine, a bring-up or someone pairing" : "Code or a routine"} needs your OK, it shows up here.</span>
            </div>
          ) : (
            <ul className="bell-list" aria-label={`Waiting for you: ${count}`}>
              {list.map((item, i) => {
                const mine = busy === item.key;
                return (
                  <li key={item.key} ref={(el) => { rows.current[i] = el; }} className={`bell-item inbox-${item.kind}`}
                    tabIndex={i === Math.min(active, count - 1) ? 0 : -1} onFocus={(e) => { if (e.target === e.currentTarget) setActive(i); }}
                    onKeyDown={(e) => onRowKey(e, i, item, { count, focusRow, open: go, decide: (it, d) => void decide(it, d) })}
                    aria-label={`${item.where}: ${item.summary}. ${itemAge(item)}`} aria-keyshortcuts={item.answer === "decide" ? "A D Enter" : "Enter"}>
                    <Icon kind={INBOX_ICONS[item.kind]} className="bell-icon" aria-hidden />
                    <div className="bell-main">
                      <div className="bell-where"><span>{item.where}</span><span className="muted">{itemAge(item)}</span></div>
                      <div className={`bell-summary ${item.kind === "code" || item.kind === "bringup" ? "mono" : ""}`}>{item.summary}</div>
                      <div className="bell-actions">
                        {item.answer === "decide" ? (
                          <>
                            <Button size="sm" disabled={!!busy} onClick={() => void decide(item, "allow")}>{mine ? "Answering…" : "Allow"}</Button>
                            <Button size="sm" variant="ghost" disabled={!!busy} onClick={() => void decide(item, "deny")}>Deny</Button>
                            <span className="grow" />
                            <Button size="sm" variant="ghost" onClick={() => go(item)}>Open</Button>
                          </>
                        ) : (
                          <>
                            <Button size="sm" variant="subtle" onClick={() => go(item)}>Open</Button>
                            <Button size="sm" variant="ghost" onClick={() => ignore(item.key)}>Ignore</Button>
                          </>
                        )}
                      </div>
                    </div>
                  </li>
                );
              })}
            </ul>
          )}
          <div className="bell-foot">
            <button className="bell-all" onClick={all}>See all notifications</button>
            {count > 0 && <span className="muted bell-keys" aria-hidden><span className="kbd">A</span> allow · <span className="kbd">D</span> deny · <span className="kbd">Enter</span> open</span>}
          </div>
        </div>
      )}
    </div>
  );
}
