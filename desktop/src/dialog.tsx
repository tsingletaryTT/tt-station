// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// TT Station's own confirmation dialog, in place of the system's plain
// window.confirm: `if (await ask("Stop the model?", { … })) …`.

import type React from "react";
import { useCallback, useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { Button } from "@tenstorrent/vesper/button";
import { TextInput } from "@tenstorrent/vesper/text-input";

export interface AskOptions {
  /** More detail under the title. */
  body?: string;
  /** The yes button's words (default "Continue"). */
  confirm?: string;
  /** The no button's words (default "Cancel"). */
  cancel?: string;
  /** A destructive action: the yes button is red. */
  danger?: boolean;
}

const FOCUSABLE = 'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

/**
 * A modal's focus: starts on `first` (a selector inside it), Tab and Shift+Tab
 * stay inside, Esc cancels, and focus goes back to where it was when it closes.
 */
export function useDialogFocus(box: React.RefObject<HTMLElement | null>, first: string, onEscape: () => void) {
  useEffect(() => {
    const before = document.activeElement as HTMLElement | null;
    (box.current?.querySelector(first) as HTMLElement | null)?.focus();
    const key = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); onEscape(); return; }
      if (e.key !== "Tab" || !box.current) return;
      const items = [...box.current.querySelectorAll<HTMLElement>(FOCUSABLE)];
      if (!items.length) return;
      const at = items.indexOf(document.activeElement as HTMLElement);
      const next = e.shiftKey ? (at <= 0 ? items.length - 1 : at - 1) : (at === items.length - 1 || at < 0 ? 0 : at + 1);
      e.preventDefault();
      items[next].focus();
    };
    window.addEventListener("keydown", key, true);
    return () => {
      window.removeEventListener("keydown", key, true);
      if (before?.isConnected) before.focus();
    };
  }, [box, first, onEscape]);
}

function Dialog({ title, opts, done }: { title: string; opts: AskOptions; done: (ok: boolean) => void }) {
  const box = useRef<HTMLDivElement>(null);
  // A destructive question starts on Cancel, so Enter never deletes by accident.
  useDialogFocus(box, opts.danger ? "[data-dialog-cancel] button" : "[data-dialog-confirm] button", useCallback(() => done(false), [done]));
  return (
    <div className="dialog-scrim" onMouseDown={(e) => { if (e.target === e.currentTarget) done(false); }}>
      <div className="dialog" role="alertdialog" aria-modal="true" aria-labelledby="dialog-title" aria-describedby={opts.body ? "dialog-body" : undefined} ref={box}>
        <div id="dialog-title" className="dialog-title">{title}</div>
        {opts.body && <div id="dialog-body" className="dialog-body">{opts.body}</div>}
        <div className="dialog-buttons">
          <span data-dialog-cancel=""><Button variant="ghost" onClick={() => done(false)}>{opts.cancel ?? "Cancel"}</Button></span>
          <span data-dialog-confirm=""><Button variant={opts.danger ? "danger" : "primary"} onClick={() => done(true)}>{opts.confirm ?? "Continue"}</Button></span>
        </div>
      </div>
    </div>
  );
}

/** Ask for a yes or no, in the app's own style. */
export function ask(title: string, opts: AskOptions = {}): Promise<boolean> {
  return new Promise((resolve) => {
    const host = document.createElement("div");
    document.body.appendChild(host);
    const root = createRoot(host);
    const done = (ok: boolean) => {
      root.unmount();
      host.remove();
      resolve(ok);
    };
    root.render(<Dialog title={title} opts={opts} done={done} />);
  });
}

/** What stopping or switching the served model does to other people, said plainly. */
export function whoIsAffected(model?: string | null): string {
  return `${model ?? "The model"} stops for everyone using the box: Chat, Code, Tori, apps and anything else connected to it.`;
}

/**
 * The agent's "stop what's serving first?" refusals (409 with needs_stop),
 * phrased as a question. The message reads like "… Stop Qwen3-32B first?".
 */
export function askToStop(message: string, then = "It starts again when this ends."): Promise<boolean> {
  const model = message.match(/Stop (.+?) first\?/)?.[1];
  return ask(model ? `Stop ${model}?` : "Stop what's serving?", {
    body: `${message.replace(/\s*Stop .+? first\?\s*$/, "")} ${whoIsAffected(model)} ${then}`.trim(),
    confirm: model ? `Stop ${model}` : "Stop it",
  });
}

function TextDialog({ title, initial, done }: { title: string; initial: string; done: (v: string | null) => void }) {
  const [value, setValue] = useState(initial);
  const box = useRef<HTMLDivElement>(null);
  useDialogFocus(box, "input", useCallback(() => done(null), [done]));
  useEffect(() => { box.current?.querySelector("input")?.select(); }, []);
  return (
    <div className="dialog-scrim" onMouseDown={(e) => { if (e.target === e.currentTarget) done(null); }}>
      <div className="dialog" role="dialog" aria-modal="true" aria-labelledby="dialog-text-title" ref={box}>
        <div id="dialog-text-title" className="dialog-title">{title}</div>
        <TextInput aria-labelledby="dialog-text-title" value={value} onChange={(e) => setValue(e.target.value)} onKeyDown={(e) => { if (e.key === "Enter" && value.trim()) done(value.trim()); }} />
        <div className="dialog-buttons">
          <Button variant="ghost" onClick={() => done(null)}>Cancel</Button>
          <Button disabled={!value.trim()} onClick={() => done(value.trim())}>Save</Button>
        </div>
      </div>
    </div>
  );
}

/** Ask for a line of text (a name), in the app's own style. Null if cancelled. */
export function askText(title: string, initial = ""): Promise<string | null> {
  return new Promise((resolve) => {
    const host = document.createElement("div");
    document.body.appendChild(host);
    const root = createRoot(host);
    const done = (v: string | null) => { root.unmount(); host.remove(); resolve(v); };
    root.render(<TextDialog title={title} initial={initial} done={done} />);
  });
}
