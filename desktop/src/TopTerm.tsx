// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The real tt-toplike, inside TT Station: the agent runs `tt-toplike --remote`
// against its own telemetry in a terminal and streams it here (see
// `toplike_term.rs`); xterm.js draws it exactly as a terminal would. Every
// view is tt-toplike's own, and tt-toplike's keys work (v, i, l, /explain…).

import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import "@xterm/xterm/css/xterm.css";
import { Button } from "@tenstorrent/vesper/button";
import { get, wsProtocols } from "./api";

const VIEWS: { id: string; label: string; hint: string }[] = [
  { id: "arcade", label: "Arcade", hint: "Everything at once, with the @ hero" },
  { id: "starfield", label: "Starfield", hint: "Tensix cores as stars" },
  { id: "castle", label: "Memory castle", hint: "DDR → L2 → L1 → Tensix" },
  { id: "flow", label: "Memory flow", hint: "GDDR channels in motion" },
  { id: "defrag", label: "Defrag", hint: "Model weights filling memory" },
  { id: "inference", label: "Inference", hint: "The inference server monitor" },
  { id: "insights", label: "Insights", hint: "Each chip, in plain words" },
  { id: "hivemind", label: "Hivemind", hint: "Who's touching which chip" },
];

export function TopTerm() {
  const [view, setView] = useState(() => { try { return localStorage.getItem("tt.toplike.view") ?? "arcade"; } catch { return "arcade"; } });
  const [installed, setInstalled] = useState<boolean | null>(null);
  const [state, setState] = useState<"connecting" | "live" | "ended">("connecting");
  const host = useRef<HTMLDivElement>(null);

  useEffect(() => { get<{ installed: boolean }>("/toplike").then((r) => setInstalled(r.installed)).catch(() => setInstalled(false)); }, []);
  useEffect(() => { try { localStorage.setItem("tt.toplike.view", view); } catch { /* private */ } }, [view]);

  useEffect(() => {
    if (!installed || !host.current) return;
    const term = new Terminal({
      fontFamily: "'IBM Plex Mono', 'DejaVu Sans Mono', 'Noto Sans Mono', monospace",
      fontSize: 13,
      lineHeight: 1,
      cursorBlink: false,
      cursorStyle: "bar",
      allowProposedApi: true,
      theme: { background: "#000000", foreground: "#d8e6e3", cursor: "#000000" },
      scrollback: 0,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current);
    try { term.loadAddon(new WebglAddon()); } catch { /* canvas renderer is fine */ }
    fit.fit();
    setState("connecting");
    const url = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/toplike/term?mode=${view}&cols=${term.cols}&rows=${term.rows}`;
    const ws = new WebSocket(url, wsProtocols());
    ws.binaryType = "arraybuffer";
    ws.onmessage = (m) => {
      setState("live");
      term.write(typeof m.data === "string" ? m.data : new Uint8Array(m.data));
    };
    ws.onclose = () => setState("ended");
    const input = term.onData((data) => { if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ type: "input", data })); });
    const resize = new ResizeObserver(() => {
      fit.fit();
      if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
    });
    resize.observe(host.current);
    return () => { resize.disconnect(); input.dispose(); ws.close(); term.dispose(); };
  }, [installed, view]);

  if (installed === false) {
    return <div className="muted">This hardware view isn't installed on this box.</div>;
  }
  return (
    <div className="stack" style={{ gap: 10 }}>
      <div className="toplike-tabs" role="tablist">
        {VIEWS.map((v) => (
          <button key={v.id} role="tab" aria-selected={view === v.id} className={view === v.id ? "on" : ""} title={v.hint} onClick={() => setView(v.id)}>{v.label}</button>
        ))}
        <span className={`toplike-live ${state === "live" ? "on" : ""}`}>{state === "live" ? "live" : state === "ended" ? "ended" : "starting…"}</span>
        {window.ttDesktop && (
          <Button size="xs" variant="ghost" onClick={() => window.ttDesktop!.openToplike(view)}>Open in its own window</Button>
        )}
      </div>
      <div ref={host} className="topterm" onClick={(e) => (e.currentTarget.querySelector("textarea") as HTMLTextAreaElement | null)?.focus()} />
      <div className="muted" style={{ fontSize: 12 }}>
        The real tt-toplike, reading this box's live telemetry. Click it to use its keys: <span className="kbd">v</span> next view, <span className="kbd">i</span> inference, <span className="kbd">l</span> legend, <span className="kbd">/explain</span> what each signal means.
      </div>
    </div>
  );
}
