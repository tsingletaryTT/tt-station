// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Scrolling a conversation (Chat, Code): the message list is its own scroll
// box, and new output keeps it at the bottom only while the person is already
// there. Scrolled up to read, they stay put and get a "Jump to latest" button.
// Only that box ever scrolls -- never the window (no scrollIntoView).

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";

/** How close to the bottom (px) still counts as "at the bottom". */
export const NEAR_BOTTOM_PX = 80;

/** Pixels between what's in view and the end of the content. */
export const distanceFromBottom = (el: HTMLElement) => el.scrollHeight - el.scrollTop - el.clientHeight;

export const isNearBottom = (el: HTMLElement, threshold = NEAR_BOTTOM_PX) => distanceFromBottom(el) <= threshold;

/** Whether the person asked for less motion (no smooth scrolling then). */
export function prefersReducedMotion(): boolean {
  try { return window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false; } catch { return false; }
}

/** "smooth", unless reduced motion is on. */
export const smoothBehavior = (): ScrollBehavior => (prefersReducedMotion() ? "auto" : "smooth");

/** Scroll the box itself (never its ancestors or the window) to its end. */
export function scrollToBottom(el: HTMLElement, smooth = false) {
  const top = el.scrollHeight;
  if (smooth && !prefersReducedMotion() && typeof el.scrollTo === "function") el.scrollTo({ top, behavior: "smooth" });
  else el.scrollTop = top;
}

/**
 * Stick a scroll box to its bottom while the person is there.
 *
 * `content` is anything that changes when there's new output (the messages);
 * `resetKey` (a conversation's id) starts over at the bottom when it changes.
 * Attach `ref` and `onScroll` to the scroll box; show `<JumpToLatest>` with
 * `unseen` and `jump`. Call `stick()` when the person sends something, so the
 * answer follows them down.
 */
export function useStickToBottom<T extends HTMLElement = HTMLDivElement>(content: unknown, resetKey?: unknown) {
  const ref = useRef<T>(null);
  const pinned = useRef(true);
  const [unseen, setUnseen] = useState(false);

  const onScroll = useCallback(() => {
    const el = ref.current;
    if (!el) return;
    pinned.current = isNearBottom(el);
    if (pinned.current) setUnseen(false);
  }, []);

  // A different conversation: open it at its end.
  useLayoutEffect(() => {
    pinned.current = true;
    setUnseen(false);
    if (ref.current) scrollToBottom(ref.current);
  }, [resetKey]);

  // New output: follow it only if the person was at the bottom.
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    if (pinned.current) scrollToBottom(el);
    else if (el.scrollHeight > el.clientHeight) setUnseen(true);
  }, [content]);

  // Content that grows by itself (an image loading, a code block laid out):
  // still at the bottom afterwards, if that's where they were.
  useEffect(() => {
    const el = ref.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(() => { if (pinned.current) scrollToBottom(el); });
    ro.observe(el);
    for (const child of el.children) ro.observe(child);
    return () => ro.disconnect();
  }, []);

  const jump = useCallback(() => {
    pinned.current = true;
    setUnseen(false);
    if (ref.current) scrollToBottom(ref.current, true);
  }, []);
  const stick = useCallback(() => { pinned.current = true; setUnseen(false); }, []);

  return { ref, onScroll, unseen, jump, stick, pinned };
}

/** The small button over the bottom of a conversation, when there's more below. */
export function JumpToLatest({ show, onClick }: { show: boolean; onClick: () => void }) {
  if (!show) return null;
  return <button type="button" className="jump-latest" onClick={onClick}>↓ Jump to latest</button>;
}
