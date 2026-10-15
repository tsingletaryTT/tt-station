// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// TT Station's own confirmation dialog, in place of the system's plain
// window.confirm: `if (await ask("Stop the model?", { … })) …`.

import { useEffect, useRef, useState } from "react";
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

function Dialog({ title, opts, done }: { title: string; opts: AskOptions; done: (ok: boolean) => void }) {
  const yes = useRef<HTMLDivElement>(null);
  useEffect(() => {
    (yes.current?.querySelector("button") as HTMLButtonElement | null)?.focus();
    const key = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); done(false); }
    };
    window.addEventListener("keydown", key, true);
    return () => window.removeEventListener("keydown", key, true);
  }, [done]);
  return (
    <div className="dialog-scrim" onMouseDown={(e) => { if (e.target === e.currentTarget) done(false); }}>
      <div className="dialog" role="alertdialog" aria-modal="true" aria-labelledby="dialog-title">
        <div id="dialog-title" className="dialog-title">{title}</div>
        {opts.body && <div className="dialog-body">{opts.body}</div>}
        <div className="dialog-buttons">
          <Button variant="ghost" onClick={() => done(false)}>{opts.cancel ?? "Cancel"}</Button>
          <div ref={yes}><Button variant={opts.danger ? "danger" : "primary"} onClick={() => done(true)}>{opts.confirm ?? "Continue"}</Button></div>
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

/**
 * The agent's "stop what's serving first?" refusals (409 with needs_stop),
 * phrased as a question. The message reads like "… Stop Qwen3-32B first?".
 */
export function askToStop(message: string, then = "It starts again when this ends."): Promise<boolean> {
  const model = message.match(/Stop (.+?) first\?/)?.[1];
  return ask(model ? `Stop ${model}?` : "Stop what's serving?", {
    body: `${message.replace(/\s*Stop .+? first\?\s*$/, "")} ${then}`.trim(),
    confirm: model ? `Stop ${model}` : "Stop it",
  });
}

function TextDialog({ title, initial, done }: { title: string; initial: string; done: (v: string | null) => void }) {
  const [value, setValue] = useState(initial);
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const input = box.current?.querySelector("input");
    input?.focus();
    input?.select();
    const key = (e: KeyboardEvent) => { if (e.key === "Escape") { e.preventDefault(); done(null); } };
    window.addEventListener("keydown", key, true);
    return () => window.removeEventListener("keydown", key, true);
  }, [done]);
  return (
    <div className="dialog-scrim" onMouseDown={(e) => { if (e.target === e.currentTarget) done(null); }}>
      <div className="dialog" role="dialog" aria-modal="true" ref={box}>
        <div className="dialog-title">{title}</div>
        <TextInput value={value} onChange={(e) => setValue(e.target.value)} onKeyDown={(e) => { if (e.key === "Enter" && value.trim()) done(value.trim()); }} />
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
