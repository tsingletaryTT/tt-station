// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Explain part of a finished research report: select text in it and a small
// menu offers Explain, Simpler and Ask about this; the answer streams into a
// panel docked at the bottom of the report column (sticky, so the passage
// and its explanation are on screen together and the report doesn't move).
// The menu is also reachable from a button in the report's header, for the
// keyboard. Answers come from `/research/{id}/explain`, which reads only the
// report; they're shown as Markdown without raw HTML, links or images,
// because the report was written from arbitrary web pages.

import {
  useCallback, useEffect, useLayoutEffect, useRef, useState,
  type KeyboardEvent as ReactKeyboardEvent, type MouseEvent as ReactMouseEvent, type ReactNode, type RefObject,
} from "react";
import Markdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { Button } from "@tenstorrent/vesper/button";
import { Icon } from "@tenstorrent/vesper/icons";
import { StatusIndicator } from "@tenstorrent/vesper/status-indicator";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { ApiError, streamExplain } from "../api";
import { copyText } from "../ui";
import {
  followUpQuestion, linkCitations, MAX_QUESTION, menuPlacement, modeTitle, selectionProblem, tidySelection, type Box, type ExplainMode,
} from "./explainHelpers";
import { smoothBehavior } from "../stickToBottom";

/** A citation link scrolls to its source in the list below the report. */
const toSource = (href: string) => {
  const el = document.getElementById(href.slice(1));
  el?.scrollIntoView({ behavior: smoothBehavior(), block: "center" });
};

/** How an answer renders: citations link to the sources; any other link or image is shown as text, never followed or loaded. */
const ANSWER_COMPONENTS: Components = {
  a: ({ href, children }) => href?.startsWith("#src-")
    ? <a href={href} className="cite" onClick={(e) => { e.preventDefault(); toSource(href); }}>{children}</a>
    : <span>{children}</span>,
  img: ({ alt }) => (alt ? <span>{alt}</span> : null),
};

export interface Picked { text: string; rect: Box }

/**
 * The text selected inside `root` (null when there's none, or it's elsewhere).
 * `pointer` says the selection was just finished with the mouse, which is when
 * the floating menu appears on its own.
 */
export function useReportSelection(root: RefObject<HTMLElement | null>) {
  const [picked, setPicked] = useState<Picked | null>(null);
  const [pointer, setPointer] = useState(0);
  const read = useCallback((): Picked | null => {
    const s = window.getSelection();
    const el = root.current;
    if (!s || !el || s.isCollapsed || !s.rangeCount) return null;
    const range = s.getRangeAt(0);
    if (!el.contains(range.commonAncestorContainer)) return null;
    const text = tidySelection(s.toString());
    if (text.length < 2) return null;
    const r = range.getBoundingClientRect();
    return { text, rect: { top: r.top, left: r.left, width: r.width, height: r.height } };
  }, [root]);
  useEffect(() => {
    let frame = 0;
    const changed = () => { cancelAnimationFrame(frame); frame = requestAnimationFrame(() => setPicked(read())); };
    const up = (e: MouseEvent) => {
      if (!root.current?.contains(e.target as Node)) return;
      // After the click has settled the selection.
      requestAnimationFrame(() => { const p = read(); setPicked(p); if (p) setPointer((n) => n + 1); });
    };
    document.addEventListener("selectionchange", changed);
    document.addEventListener("mouseup", up);
    return () => { cancelAnimationFrame(frame); document.removeEventListener("selectionchange", changed); document.removeEventListener("mouseup", up); };
  }, [read, root]);
  return { picked, pointer, reread: read };
}

/** The floating menu: Explain, Simpler, Ask about this (which opens a question box). */
export function SelectionMenu({ picked, box, focusFirst, onPick, onClose }: {
  picked: Picked; box: RefObject<HTMLElement | null>; focusFirst: boolean;
  onPick: (mode: ExplainMode, question?: string) => void; onClose: (restoreFocus: boolean) => void;
}) {
  const menu = useRef<HTMLDivElement>(null);
  const [asking, setAsking] = useState(false);
  const [question, setQuestion] = useState("");
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);
  const problem = selectionProblem(picked.text);

  useLayoutEffect(() => {
    const b = box.current?.getBoundingClientRect();
    const m = menu.current;
    if (!b || !m) return;
    setPos(menuPlacement(picked.rect, { top: b.top, left: b.left, width: b.width, height: b.height }, { width: m.offsetWidth, height: m.offsetHeight }));
  }, [picked, box, asking]);
  useEffect(() => {
    if (!focusFirst || !pos) return;
    menu.current?.querySelector<HTMLElement>("button, input")?.focus({ preventScroll: true });
    menu.current?.scrollIntoView({ block: "nearest" });
  }, [focusFirst, !!pos]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    if (asking) menu.current?.querySelector<HTMLInputElement>("input")?.focus();
  }, [asking]);
  useEffect(() => {
    const away = (e: MouseEvent) => { if (!menu.current?.contains(e.target as Node)) onClose(false); };
    // Escape closes it wherever focus is (after a mouse selection, it's still on the page).
    const esc = (e: KeyboardEvent) => { if (e.key === "Escape" && !menu.current?.contains(e.target as Node)) onClose(false); };
    document.addEventListener("mousedown", away);
    document.addEventListener("keydown", esc);
    return () => { document.removeEventListener("mousedown", away); document.removeEventListener("keydown", esc); };
  }, [onClose]);

  const keys = (e: ReactKeyboardEvent) => {
    if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); if (asking) setAsking(false); else onClose(true); return; }
    if (asking || (e.key !== "ArrowRight" && e.key !== "ArrowLeft")) return;
    const items = [...(menu.current?.querySelectorAll<HTMLButtonElement>("button:not([disabled])") ?? [])];
    const at = items.indexOf(document.activeElement as HTMLButtonElement);
    items[(at + (e.key === "ArrowRight" ? 1 : items.length - 1)) % items.length]?.focus();
    e.preventDefault();
  };
  // Keep the report's selection while the menu is clicked.
  const keep = (e: ReactMouseEvent) => { if (!(e.target instanceof HTMLInputElement)) e.preventDefault(); };
  const ask = () => { const q = question.trim(); if (q) onPick("ask", q); };

  return (
    <div ref={menu} className="explain-menu" role={asking ? "group" : "toolbar"} aria-label="Ask about the selected text"
      style={{ top: pos?.top ?? 0, left: pos?.left ?? 0, visibility: pos ? "visible" : "hidden" }} onKeyDown={keys} onMouseDown={keep}>
      {problem ? (
        <span className="explain-menu-note" role="status">{problem}</span>
      ) : asking ? (
        <form className="explain-ask" onSubmit={(e) => { e.preventDefault(); ask(); }}>
          <TextInput size="sm" aria-label="Your question about the selected text" placeholder="What do you want to know about this?" value={question}
            maxLength={MAX_QUESTION} onChange={(e) => setQuestion(e.target.value)} />
          <Button size="sm" type="submit" disabled={!question.trim()}>Ask</Button>
        </form>
      ) : (
        <>
          <button type="button" onClick={() => onPick("explain")}>Explain</button>
          <button type="button" onClick={() => onPick("simpler")}>Simpler</button>
          <button type="button" onClick={() => setAsking(true)}>Ask about this…</button>
        </>
      )}
    </div>
  );
}

export interface ExplainRequest { key: number; selection: string; mode: ExplainMode; question?: string }

type Phase = "waiting" | "thinking" | "writing" | "done" | "stopped" | "error";

/** The answer, streamed: working, then the text with copy / research further / close. */
export function ExplainPanel({ reportId, req, onClose, onFollowUp, onRetry, startModel }: {
  reportId: string; req: ExplainRequest; onClose: () => void; onFollowUp: (q: string) => void; onRetry: () => void;
  /** Offered when nothing is serving and the page knows what to start. */
  startModel?: () => void;
}) {
  const [phase, setPhase] = useState<Phase>("waiting");
  const [text, setText] = useState("");
  const [error, setError] = useState<{ message: string; notServing: boolean } | null>(null);
  const [slow, setSlow] = useState(false);
  const abort = useRef<AbortController | null>(null);
  const heading = useRef<HTMLHeadingElement>(null);
  const working = phase === "waiting" || phase === "thinking" || phase === "writing";

  useEffect(() => {
    const ctl = new AbortController();
    abort.current = ctl;
    heading.current?.focus({ preventScroll: true });
    const slowTimer = window.setTimeout(() => setSlow(true), 8000);
    (async () => {
      let finished = false;
      try {
        for await (const ev of streamExplain(reportId, { selection: req.selection, mode: req.mode, question: req.question }, ctl.signal)) {
          if (ev.type === "thinking") setPhase((p) => (p === "waiting" ? "thinking" : p));
          else if (ev.type === "delta") { setPhase("writing"); setText((t) => t + ev.text); }
          else if (ev.type === "done") { finished = true; setText(ev.answer); setPhase("done"); }
          else if (ev.type === "error") { finished = true; setError({ message: ev.error, notServing: false }); setPhase("error"); }
        }
        if (!finished && !ctl.signal.aborted) { setError({ message: "The answer stopped before it finished. Try again.", notServing: false }); setPhase("error"); }
      } catch (e) {
        if (ctl.signal.aborted) return;
        const message = e instanceof Error ? e.message : String(e);
        setError({ message, notServing: e instanceof ApiError && e.status === 409 && /serving|chat model/i.test(message) });
        setPhase("error");
      } finally {
        window.clearTimeout(slowTimer);
      }
    })();
    return () => { window.clearTimeout(slowTimer); ctl.abort(); };
  }, [reportId, req]);

  // Escape closes it from anywhere on the page, unless the selection menu is open (that closes first) or another field has the key.
  useEffect(() => {
    const esc = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement;
      if (e.key !== "Escape" || e.defaultPrevented || document.querySelector(".explain-menu")) return;
      if (t.closest(".explain-panel") || !t.closest("input, textarea, [contenteditable], [role=dialog]")) { e.preventDefault(); onClose(); }
    };
    document.addEventListener("keydown", esc);
    return () => document.removeEventListener("keydown", esc);
  }, [onClose]);

  const stop = () => { abort.current?.abort(); setPhase("stopped"); };
  const status: ReactNode = phase === "waiting" || phase === "thinking" ? (
    <div className="stack" style={{ gap: 4 }}>
      <StatusIndicator label={phase === "thinking" ? "Thinking it over…" : "Reading the passage…"} state="progress" animated />
      {slow && <span className="muted" style={{ fontSize: 12 }}>Still working. The model reads the report around the passage first, and it's shared with everything else on the box, so the first words can take a while.</span>}
    </div>
  ) : null;

  return (
    <section className="explain-panel" aria-label="About the selected text">
      <div className="row-between" style={{ alignItems: "flex-start", gap: 8 }}>
        <h3 ref={heading} tabIndex={-1} className="explain-title">{modeTitle(req.mode, req.question)}</h3>
        <button type="button" className="code-menu-button" aria-label="Close the explanation" title="Close (Esc)" onClick={onClose}><Icon kind="close" /></button>
      </div>
      <blockquote className="explain-quote">{req.selection}</blockquote>
      <div className="explain-body" aria-live="polite" aria-busy={working}>
        {status}
        {text && (
          <div className="markdown selectable research-report explain-answer">
            <Markdown remarkPlugins={[remarkGfm]} skipHtml components={ANSWER_COMPONENTS}>{linkCitations(text)}</Markdown>
          </div>
        )}
        {phase === "stopped" && <span className="muted" style={{ fontSize: 12 }}>Stopped.</span>}
        {error && (
          <Admonition variant="warning" subtle size="sm">
            <div className="row-between wrap" style={{ gap: 8 }}>
              <span>{error.message}</span>
              <span className="row" style={{ gap: 6 }}>
                {error.notServing && startModel && <Button size="xs" onClick={startModel}>Start Qwen3-32B</Button>}
                <Button size="xs" variant="subtle" onClick={onRetry}>Try again</Button>
              </span>
            </div>
          </Admonition>
        )}
      </div>
      <div className="row wrap" style={{ gap: 6 }}>
        {working
          ? <Button size="xs" variant="subtle" onClick={stop}>Stop</Button>
          : <>
            {text && <Button size="xs" variant="subtle" iconLeft={<Icon kind="copy" />} onClick={() => void copyText(text, "Answer copied.")}>Copy</Button>}
            <Button size="xs" variant="ghost" onClick={() => onFollowUp(followUpQuestion(req.selection, req.mode, req.question))}>Research this further</Button>
          </>}
        <span className="muted grow" style={{ fontSize: 12, textAlign: "right" }}>From this report only; check the sources it cites.</span>
      </div>
    </section>
  );
}
