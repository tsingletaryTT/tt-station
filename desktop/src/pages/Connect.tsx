// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Connect: an app store for the box. Open-source apps that run next to the
// models (already wired to the box), agents, research tools, editor plugins,
// terminals and hosted services with the same API, by category; and the box
// as a private OpenAI-compatible API for everything else.

import { type ReactNode, useEffect, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Badge } from "@tenstorrent/vesper/badge";
import { StatusIndicator } from "@tenstorrent/vesper/status-indicator";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Snippet } from "@tenstorrent/vesper/snippet";
import { CodeBlock } from "@tenstorrent/vesper/code-block";
import python from "@shikijs/langs/python";
import shellscript from "@shikijs/langs/shellscript";
import { Typography } from "@tenstorrent/vesper/typography";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { Sheet, useSheet } from "@tenstorrent/vesper/sheet";
import openwebuiLogo from "../assets/logos/openwebui.png";
import anythingllmLogo from "../assets/logos/anythingllm.png";
import lobechatLogo from "../assets/logos/lobechat.png";
import jupyterLogo from "../assets/logos/jupyter.svg";
import n8nLogo from "../assets/logos/n8n.svg";
import flowiseLogo from "../assets/logos/flowise.png";
import hermesLogo from "../assets/logos/hermes.png";
import termiusLogo from "../assets/logos/termius.svg";
import openrouterLogo from "../assets/logos/openrouter.svg";
import consoleLogo from "../assets/logos/tt-console.svg";
import searxngLogo from "../assets/logos/searxng.png";
import comfyuiLogo from "../assets/logos/comfyui.png";
import langflowLogo from "../assets/logos/langflow.png";
import continueLogo from "../assets/logos/continue.png";
import clineLogo from "../assets/logos/cline.png";
import aiderLogo from "../assets/logos/aider.png";
import { type AppKey, type Client, del, get, onAnotherComputer, openBoxShell, patch, post, ago } from "../api";
import { Card, copyText, Eyebrow, Page, Spark, toastError, toastOk, usePoll, writeClipboard } from "../ui";
import { Switch } from "@tenstorrent/vesper/switch";
import { ask } from "../dialog";
import { OWNER_ONLY, useOwner } from "../role";
import { navigate } from "../App";
import { AGENT_KEY } from "./Terminals";

interface BoxApp {
  id: string; name: string; blurb: string; category: string; port: number; local_only: boolean; homepage: string;
  state: "not_installed" | "installing" | "running" | "stopped" | "failed";
  step: string | null; error: string | null; url: string | null;
  login: { user?: string; password?: string; access_code?: string } | null;
  /** Other apps it sets up alongside (e.g. a search engine). */
  needs?: string[];
  /** One thing to do in the app the first time. */
  hint?: string | null;
  /** Running apps that rely on this one. */
  used_by?: string[];
  /** For apps set up in their own settings screen: what to paste there. */
  connect?: { base_url: string; api_key: string | null; model: string | null } | null;
}

const BLURBS: Record<string, string> = {
  openwebui: "The most popular self-hosted chat: documents, web search, many users.",
  anythingllm: "Chat with your documents and build agents, all on the box.",
};

/** Each app's own logo (from its image on the box, or its project's repo). */
const LOGOS: Record<string, string> = {
  openwebui: openwebuiLogo, anythingllm: anythingllmLogo, lobechat: lobechatLogo,
  jupyter: jupyterLogo, n8n: n8nLogo, flowise: flowiseLogo,
  searxng: searxngLogo, comfyui: comfyuiLogo, langflow: langflowLogo, continue: continueLogo, cline: clineLogo, aider: aiderLogo,
};
/** Logos that are a filled square (the project's avatar): shown edge to edge. */
const FULL = new Set(["searxng", "comfyui", "langflow", "continue", "cline", "aider"]);

function AppLogo({ id, name }: { id: string; name: string }) {
  const src = LOGOS[id];
  return src ? <img className={`app-logo img ${FULL.has(id) ? "full" : ""}`} src={src} alt="" /> : <div className="app-logo">{name[0]}</div>;
}

/** Hosted services that speak the same OpenAI API: for when the box is busy or you're away. */
const CLOUDS = [
  {
    id: "tt-console", name: "Tenstorrent Console", cta: "Open Console", logo: consoleLogo, url: "https://console.tenstorrent.com",
    base: "https://console.tenstorrent.com/v1",
    blurb: "Tenstorrent's cloud: the same OpenAI-compatible API, on Tenstorrent hardware in the data center.",
  },
  {
    id: "openrouter", name: "OpenRouter", cta: "Open OpenRouter", logo: openrouterLogo, url: "https://openrouter.ai",
    base: "https://openrouter.ai/api/v1",
    blurb: "Hundreds of hosted models from many providers behind one OpenAI-compatible API.",
  },
];

function openExternal(url: string) {
  if (window.ttDesktop) window.ttDesktop.openExternal(url);
  else window.open(url, "_blank", "noopener");
}

function open(url: string, title: string) {
  if (window.ttDesktop) window.ttDesktop.openAppWindow(url, title);
  else window.open(url, "_blank");
}

interface Hermes {
  installed: boolean; installing: boolean; install_failed: boolean; log: string[];
  configured_model: string | null; running: boolean; serving_model: string | null;
  serving_context: number | null; context_too_small: boolean; has_session: boolean;
  uses_box: boolean | null; provider: string | null; desktop_built: boolean; key_requests: number | null;
}

const shortName = (m: string | null) => (m ?? "").split("/").pop() ?? "";

/** A value to paste into Hermes, with a copy button. */
function PasteValue({ label, value, secret }: { label: string; value: string; secret?: boolean }) {
  const [shown, setShown] = useState(!secret);
  return (
    <div className="stack" style={{ gap: 2 }}>
      <span className="muted" style={{ fontSize: 12 }}>{label}</span>
      <div className="row" style={{ gap: 6, minWidth: 0 }}>
        <code className="grow paste-value">{shown ? value : "•".repeat(24)}</code>
        {secret && <Button size="xs" variant="ghost" onClick={() => setShown((v) => !v)}>{shown ? "Hide" : "Show"}</Button>}
        <Button size="xs" variant="subtle" onClick={() => void copyText(value, `${label} copied.`)}>Copy</Button>
      </div>
    </div>
  );
}

function Step({ n, title, done, active, children }: { n: number; title: string; done: boolean; active: boolean; children?: React.ReactNode }) {
  return (
    <div className={`finish-step ${done ? "done" : ""}`} style={{ opacity: !done && !active ? 0.55 : 1 }}>
      <span className="finish-n">{done ? "✓" : n}</span>
      <div className="grow stack" style={{ gap: 8 }}><b>{title}</b>{(active || done) && children}</div>
    </div>
  );
}

/**
 * Setting Hermes up, alongside Hermes Desktop's own first-run screens: TT
 * Station installs and opens it, hands over the box's address and a key to
 * paste in, and ticks each step off as it sees it happen.
 */
function HermesGuide({ s, refresh }: { s: Hermes; refresh: () => void }) {
  const [busy, setBusy] = useState<string | null>(null);
  const [creds, setCreds] = useState<{ base_url: string; api_key: string } | null>(null);
  const modelOk = !!s.serving_model && !s.context_too_small;
  const connected = s.uses_box === true;
  const talked = (s.key_requests ?? 0) > 0;
  const act = async (what: string, fn: () => Promise<unknown>) => {
    setBusy(what);
    try { await fn(); refresh(); } catch (e) { toastError(e); } finally { setBusy(null); }
  };
  const install = async () => {
    if (!(await ask("Install Hermes Agent?", {
      body: "Runs Nous Research's official installer (hermes-agent.nousresearch.com) on the box, with its desktop app. It goes into your home folder, needs no admin password, and takes a few minutes.",
      confirm: "Install",
    }))) return;
    act("install", () => post("/hermes/install"));
  };
  const step = !modelOk ? 1 : !s.installed ? 2 : !s.running && !connected ? 3 : !connected ? 4 : !talked ? 5 : 6;
  return (
    <div className="stack" style={{ gap: 14 }}>
      <Step n={1} title="A chat model with a long memory" done={modelOk} active={step === 1}>
        {modelOk ? <span className="muted" style={{ fontSize: 13 }}>{shortName(s.serving_model)} is serving{s.serving_context ? `, with ${Math.round(s.serving_context / 1024)}K of context` : ""}.</span>
          : <>
            <span className="muted" style={{ fontSize: 13 }}>
              {s.serving_model ? `${shortName(s.serving_model)} remembers too little for Hermes, which needs at least 64K of context.` : "Hermes runs on the model the box is serving, and needs one that remembers at least 64K of context."}
              {" "}Qwen3-32B is a good fit.
            </span>
            <div><Button size="sm" disabled={busy === "model"} onClick={() => act("model", () => post("/serve", { model: "Qwen3-32B", force: true }).then(() => toastOk("Starting Qwen3-32B. It takes a few minutes.")))}>Start Qwen3-32B</Button></div>
          </>}
      </Step>
      <Step n={2} title="Install Hermes Desktop" done={s.installed} active={step === 2}>
        {s.installed ? <span className="muted" style={{ fontSize: 13 }}>Installed.</span>
          : s.installing ? <span className="muted mono" style={{ fontSize: 12 }}>{s.log[s.log.length - 1] ?? "Installing…"}</span>
          : <>
            <span className="muted" style={{ fontSize: 13 }}>Nous Research's official installer, with the desktop app. A few minutes.</span>
            {s.install_failed && s.log.length > 0 && <div className="console" style={{ maxHeight: 100, fontSize: 11 }}>{s.log.join("\n")}</div>}
            <div><Button size="sm" disabled={busy === "install"} onClick={install}>{s.install_failed ? "Try again" : "Install"}</Button></div>
          </>}
      </Step>
      <Step n={3} title="Open Hermes Desktop" done={s.running || connected} active={step === 3}>
        {s.running || connected ? <span className="muted" style={{ fontSize: 13 }}>{s.running ? "It's open on the box's screen." : "Done."}</span>
          : <>
            <span className="muted" style={{ fontSize: 13 }}>{s.desktop_built ? "It opens on the box's screen." : "The first time, it builds the app first (a few minutes), then opens on the box's screen."}</span>
            <div><Button size="sm" disabled={busy === "open" || !s.has_session} onClick={() => act("open", () => post("/hermes/open"))}>Open Hermes</Button></div>
          </>}
      </Step>
      <Step n={4} title="Connect it to your QuietBox" done={connected} active={step === 4}>
        {connected ? <span className="muted" style={{ fontSize: 13 }}>Hermes is set to use this box.</span>
          : <>
            <ol className="guide-list">
              <li>On Hermes's “Let's get you setup with Hermes Agent” screen, click <b>I have an API key</b>.</li>
              <li>Choose <b>Local / custom endpoint</b>.</li>
              <li>Paste these in, then click <b>Connect</b>. Hermes finds the model by itself.</li>
            </ol>
            {creds ? <>
              <PasteValue label="Base URL" value={creds.base_url} />
              <PasteValue label="API key" value={creds.api_key} secret />
            </> : <div><Button size="sm" disabled={busy === "key"} onClick={() => act("key", () => post<{ base_url: string; api_key: string }>("/hermes/key").then(setCreds))}>Show what to paste</Button></div>}
            <span className="muted" style={{ fontSize: 12 }}>Past that screen already? In Hermes, open Settings → Providers → Custom Endpoints → New endpoint, and use the same two values.</span>
          </>}
      </Step>
      <Step n={5} title="Say hello" done={talked} active={step === 5}>
        <span className="muted" style={{ fontSize: 13 }}>{talked ? `Hermes is talking to your box (${s.key_requests} requests so far).` : "Send Hermes a message. This ticks off when your box answers it."}</span>
      </Step>
      {step === 6 && <Admonition variant="success" subtle size="sm">Hermes Agent is set up, running on your QuietBox's {shortName(s.serving_model)}.</Admonition>}
    </div>
  );
}

interface SshInfo {
  listening: boolean; user: string; port: number; name: string; hosts: { label: string; host: string }[];
  keys: { label: string; type: string }[]; command: string; link: string; config: string; qr_svg: string | null;
}

/** Signing in to the box over SSH: the addresses, and the keys TT Station has let in. */
function SshCard({ ssh }: { ssh: SshInfo }) {
  return (
    <Card>
      <div className="row-between">
        <Eyebrow>SSH into the box</Eyebrow>
        <StatusIndicator label={ssh.listening ? "SSH is on" : "SSH is off"} state={ssh.listening ? "ready" : "error"} />
      </div>
      {!ssh.listening && <Admonition variant="warning" subtle size="sm">The box's SSH server isn't running. Turn it on in a terminal on the box: <code>sudo systemctl enable --now ssh</code></Admonition>}
      <div className="stack" style={{ gap: 8 }}>
        {ssh.hosts.map((h) => <PasteValue key={h.host} label={h.label} value={`ssh ${ssh.user}@${h.host}`} />)}
      </div>
      {ssh.keys.length > 0 && <span className="muted" style={{ fontSize: 12 }}>Keys added through TT Station: {ssh.keys.map((k) => k.label).join(", ")}</span>}
      <FromThisComputer />
    </Card>
  );
}

/** In the desktop app on another computer: open the box's shell there, no setup. */
function FromThisComputer() {
  const [here, setHere] = useState(false);
  const [command, setCommand] = useState<string | null>(null);
  useEffect(() => { void onAnotherComputer().then(setHere); }, []);
  if (!here) return null;
  const run = async (what: "terminal" | "vscode") => { const why = await openBoxShell(what); if (why) toastError(new Error(why)); };
  const copy = async () => {
    const r = await window.ttDesktop?.ssh?.("command");
    if (!r?.ok || !r.command) { toastError(new Error(r?.error ?? "It isn't ready.")); return; }
    setCommand(r.command);
    if (await writeClipboard(r.command)) toastOk("Copied.");
  };
  return (
    <div className="stack" style={{ gap: 8 }}>
      <Eyebrow>From this computer</Eyebrow>
      <span className="muted" style={{ fontSize: 13 }}>TT Station makes a key just for this (~/.ssh/tt-station_ed25519) and lets it into the box the first time: no passwords.</span>
      <div className="row wrap" style={{ gap: 8 }}>
        <Button size="sm" onClick={() => run("terminal")}>Open Terminal</Button>
        <Button size="sm" variant="subtle" onClick={() => run("vscode")}>Open in VS Code</Button>
        <Button size="sm" variant="ghost" onClick={copy}>Copy the ssh command</Button>
        <Button size="sm" variant="ghost" onClick={() => navigate("terminals")}>Command line in TT Station</Button>
      </div>
      {command && <code className="pair-link" title={command}>{command}</code>}
    </div>
  );
}

/**
 * Termius (an SSH app for desktop and phone): its documented ways in are a
 * new host typed in, or an import of ~/.ssh/config. TT Station lets its key
 * in and hands over every value, with a QR for the phone (an ssh:// link,
 * which opens in whichever SSH app handles those links).
 */
function TermiusGuide({ ssh, refresh }: { ssh: SshInfo; refresh: () => void }) {
  const owner = useOwner();
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const keyIn = ssh.keys.some((k) => k.label.toLowerCase().startsWith("termius"));
  const addKey = async () => {
    setBusy(true);
    try {
      const r = await post<{ already_present: boolean }>("/ssh/authorize", { public_key: key.trim(), label: "termius" });
      toastOk(r.already_present ? "That key could already sign in." : "Termius's key can sign in now.");
      setKey(""); refresh();
    } catch (e) { toastError(e); } finally { setBusy(false); }
  };
  return (
    <div className="stack" style={{ gap: 14 }}>
      <div className={`finish-step ${keyIn ? "done" : ""}`}>
        <span className="finish-n">{keyIn ? "✓" : 1}</span>
        <div className="grow stack" style={{ gap: 8 }}>
          <b>Let Termius in with its key</b>
          {keyIn ? <span className="muted" style={{ fontSize: 13 }}>Termius's key is added, so it signs in without a password.</span> : <>
            <span className="muted" style={{ fontSize: 13 }}>In Termius, open <b>Keychain</b>, pick your key (or <b>Generate</b> one) and copy its <b>public key</b>. Paste it here.</span>
            <TextInput size="sm" value={key} onChange={(e) => setKey(e.target.value)} placeholder="ssh-ed25519 AAAA… (the public key, never the private one)" />
            <div><Button size="sm" disabled={busy || !owner || !/^(ssh-|ecdsa-|sk-)/.test(key.trim())} onClick={addKey}>Add the key</Button></div>
            {!owner && <span className="muted" style={{ fontSize: 12 }}>{OWNER_ONLY}</span>}
          </>}
        </div>
      </div>
      <div className="finish-step">
        <span className="finish-n">2</span>
        <div className="grow stack" style={{ gap: 10 }}>
          <b>Add this box to Termius</b>
          <span className="muted" style={{ fontSize: 13 }}>In Termius: <b>New Host</b>, then fill in:</span>
          <PasteValue label="Address" value={ssh.hosts[0]?.host ?? `${ssh.name}.local`} />
          <PasteValue label="Username" value={ssh.user} />
          <PasteValue label="Port" value={String(ssh.port)} />
          <span className="muted" style={{ fontSize: 12 }}>Then set its key under <b>Credentials</b>. The box is reachable on your home network.</span>
          {ssh.qr_svg && (
            <div className="row" style={{ gap: 12, alignItems: "flex-start" }}>
              <img src={`data:image/svg+xml;utf8,${encodeURIComponent(ssh.qr_svg)}`} alt="QR code of the box's SSH link" style={{ width: 132, height: 132, borderRadius: 8 }} />
              <span className="muted" style={{ fontSize: 12 }}>On your phone: scan this with the camera. It's an <code>ssh://</code> link, so it opens in your SSH app if that app handles those links; if nothing opens, use the values above.</span>
            </div>
          )}
          <details>
            <summary className="muted" style={{ fontSize: 12, cursor: "pointer" }}>On a computer: import it instead</summary>
            <div className="stack" style={{ gap: 6, marginTop: 6 }}>
              <span className="muted" style={{ fontSize: 12 }}>Add this to <code>~/.ssh/config</code>, then in Termius: <b>New Host ▾ → Import → ~/.ssh</b>.</span>
              <CodeBlock lang={shellscript} copyOnHover>{ssh.config}</CodeBlock>
            </div>
          </details>
        </div>
      </div>
      <div className="finish-step">
        <span className="finish-n">3</span>
        <div className="grow stack" style={{ gap: 6 }}>
          <b>Connect</b>
          <span className="muted" style={{ fontSize: 13 }}>Open the host in Termius. On a first connect it asks you to trust the box's fingerprint: that's expected.</span>
        </div>
      </div>
    </div>
  );
}

function TermiusTile({ ssh, refresh }: { ssh: SshInfo; refresh: () => void }) {
  const guide = useSheet();
  const keyIn = ssh.keys.some((k) => k.label.toLowerCase().startsWith("termius"));
  return (
    <Card className="app-tile">
      <div className="row-between">
        <img className="app-logo img" src={termiusLogo} alt="" />
        <Badge size="sm" variant="contrast" subtle>Terminal</Badge>
      </div>
      <div><Typography variant="heading-xs">Termius</Typography><Typography variant="copy-sm" className="muted">An SSH app for your computer and phone: a terminal on the box from anywhere, with its key so you never type a password.</Typography></div>
      {keyIn && <Typography variant="copy-xs" className="muted">Termius's key is added.</Typography>}
      <div className="grow" />
      <div className="row"><Button size="sm" variant="subtle" onClick={guide.open}>{keyIn ? "Setup guide" : "Connect Termius"}</Button></div>
      <Sheet ref={guide.ref} popover title="Connect Termius" description="Add the box to Termius on your computer or phone.">
        <TermiusGuide ssh={ssh} refresh={refresh} />
      </Sheet>
    </Card>
  );
}

/** Hermes Agent (Nous Research): its own desktop app, set up with a guide. */
function HermesTile() {
  const h = usePoll(() => get<Hermes>("/hermes"), 3000);
  const guide = useSheet();
  const s = h.data;
  const ready = !!s && s.installed && s.uses_box === true;
  const note = !s ? ""
    : s.installing ? "Installing…"
    : !s.installed ? ""
    : s.uses_box === false ? `Set to use ${s.provider ?? "another service"}, not this box.`
    : s.uses_box !== true ? "Installed, not connected to the box yet."
    : `Uses this box's ${shortName(s.serving_model ?? s.configured_model)}`;
  return (
    <Card className="app-tile">
      <div className="row-between">
        <img className="app-logo img full" src={hermesLogo} alt="" />
        <div className="row" style={{ gap: 6 }}>
          <Badge size="sm" variant="contrast" subtle>Agent</Badge>
          {s?.installed && <StatusIndicator label={s.running ? "Open" : ready ? "Ready" : "Set up"} state={s.running || ready ? "ready" : "queued"} />}
          {s?.installing && <StatusIndicator label="Installing" state="progress" animated />}
        </div>
      </div>
      <div><Typography variant="heading-xs">Hermes Agent</Typography><Typography variant="copy-sm" className="muted">Nous Research's AI agent: it remembers, learns skills and works through tasks with tools, in its own desktop app.</Typography></div>
      {note && <Typography variant="copy-xs" className="muted">{note}</Typography>}
      <div className="grow" />
      <div className="row">
        {ready ? <Button size="sm" disabled={!s?.has_session} title={s?.running ? "Bring Hermes to the front on the box's screen" : "Open Hermes on the box's screen"} onClick={async () => {
            try {
              const r = await post<{ already_open?: boolean; building?: boolean }>("/hermes/open");
              // Say what actually happened: a new window, or a running one asked to come forward
              // (GNOME may show "Hermes is ready" at the top instead of switching on its own).
              toastOk(r.already_open ? "Hermes is already open. If it doesn't come to the front, click “Hermes is ready” at the top of the screen."
                : r.building ? "Building Hermes Desktop the first time (a few minutes), then it opens on the box's screen."
                : "Opening Hermes on the box's screen.");
              h.refresh();
            } catch (e) { toastError(e); }
          }}>{s?.running ? "Show Hermes" : "Open"}</Button>
          : <Button size="sm" variant="subtle" disabled={!s} onClick={guide.open}>Set up</Button>}
        {ready && <Button size="sm" variant="ghost" onClick={guide.open}>Setup guide</Button>}
      </div>
      <Sheet ref={guide.ref} popover title="Set up Hermes Agent" description="Follow along next to Hermes Desktop. Each step ticks off when TT Station sees it happen.">
        {s && <HermesGuide s={s} refresh={h.refresh} />}
      </Sheet>
    </Card>
  );
}

// ---------------------------------------------------------------- categories

type Cat = "all" | "chat" | "agents" | "research" | "media" | "build" | "automate" | "code" | "skills" | "terminal" | "cloud";
const CATS: { id: Cat; label: string; about: string }[] = [
  { id: "all", label: "All", about: "" },
  { id: "chat", label: "Chat", about: "Chat apps for you and your team, with documents and web search." },
  { id: "agents", label: "Agents", about: "Assistants that remember, use tools and work through tasks." },
  { id: "research", label: "Research", about: "Deep research: questions in, cited reports out, from the web and your documents." },
  { id: "media", label: "Images & video", about: "Make and edit images and video." },
  { id: "build", label: "Build", about: "Notebooks and visual builders for your own AI apps." },
  { id: "automate", label: "Automate", about: "Wire the box's model into email, calendars and other services." },
  { id: "code", label: "Code", about: "Coding agents on the box's model: open one in its own terminal on the box, or set it up on your computer. TT Station's own Code page does this in the window, in a sandbox." },
  { id: "skills", label: "Skills", about: "Skill packs trending on GitHub: instructions that make the agents on your box (Hermes, OpenCode, Pi, Claude Code) better at their jobs." },
  { id: "terminal", label: "Terminal", about: "A shell on the box from your other devices." },
  { id: "cloud", label: "Cloud", about: "Hosted services with the same API, for when the chips are busy or you're away." },
];

/** The box app's category (from `/apps`) as a store category. */
function catOf(category: string): Cat {
  const c = category.toLowerCase();
  if (c.includes("research") || c.includes("search")) return "research";
  if (c.includes("image") || c.includes("video") || c.includes("media")) return "media";
  if (c.includes("automat")) return "automate";
  if (c.includes("agent")) return "agents";
  if (c.includes("code")) return "code";
  if (c.includes("chat")) return "chat";
  return "build";
}

// ---------------------------------------------------------------- editor and terminal tools

/** Coding tools that point at any OpenAI-compatible server: how to set each one up. */
const CODE_TOOLS: { id: string; name: string; blurb: string; homepage: string; cat?: Cat; badge?: string; onBox?: string; steps: (base: string, model: string) => ReactNode }[] = [
  {
    id: "claudecode", name: "Claude Code", homepage: "https://claude.com/claude-code", onBox: "claude",
    blurb: "Anthropic's coding agent, on the box's model: the box speaks its API, so no Anthropic account is needed. TT Station's Code page uses it too, in a sandbox.",
    steps: (base) => <>
      <span className="muted" style={{ fontSize: 13 }}>On your computer (Claude Code installed), point it at the box for a session:</span>
      <CodeBlock lang={shellscript} copyOnHover>{`export ANTHROPIC_BASE_URL=${base.replace(/\/v1$/, "")}
export ANTHROPIC_AUTH_TOKEN=<your app key>
export ANTHROPIC_MODEL=quietbox
claude`}</CodeBlock>
      <span className="muted" style={{ fontSize: 12 }}>The box's address is encrypted with its own certificate. If Claude Code refuses it, allow unencrypted access in Settings → Pairing and security and use the http address instead.</span>
    </>,
  },
  {
    id: "continue", name: "Continue", homepage: "https://continue.dev",
    blurb: "An open-source AI coding assistant for VS Code and JetBrains: chat, edit and autocomplete.",
    steps: (base, model) => <>
      <span className="muted" style={{ fontSize: 13 }}>In Continue's <code>config.yaml</code>, add a model:</span>
      <CodeBlock lang={shellscript} copyOnHover>{`models:
  - name: QuietBox
    provider: openai
    model: ${model}
    apiBase: ${base}
    apiKey: <your app key>`}</CodeBlock>
    </>,
  },
  {
    id: "cline", name: "Cline", homepage: "https://cline.bot",
    blurb: "An open-source coding agent in VS Code that plans, edits files and runs commands with your OK.",
    steps: (base, model) => <ol className="guide-list">
      <li>In Cline's settings, set <b>API Provider</b> to <b>OpenAI Compatible</b>.</li>
      <li><b>Base URL</b>: <code>{base}</code></li>
      <li><b>API Key</b>: your app key. <b>Model ID</b>: <code>{model}</code></li>
    </ol>,
  },
  {
    id: "aider", name: "Aider", homepage: "https://aider.chat", onBox: "aider",
    blurb: "Pair programming in your terminal: it edits the files in your git repo and commits each change.",
    steps: (base, model) => <>
      <span className="muted" style={{ fontSize: 13 }}>Run it in your repository:</span>
      <CodeBlock lang={shellscript} copyOnHover>{`aider --openai-api-base ${base} --openai-api-key <your app key> --model openai/${model}`}</CodeBlock>
    </>,
  },
  {
    id: "opencode", name: "OpenCode", homepage: "https://opencode.ai", onBox: "opencode",
    blurb: "The open-source coding agent for your terminal (trending #6 on GitHub this year), with any model provider.",
    steps: (base, model) => <>
      <span className="muted" style={{ fontSize: 13 }}>Install it: <code>curl -fsSL https://opencode.ai/install | bash</code>. Then add the box in <code>~/.config/opencode/opencode.jsonc</code> (or a project's <code>opencode.json</code>) and pick it with <code>/models</code>:</span>
      <CodeBlock lang={shellscript} copyOnHover>{`{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "quietbox": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "QuietBox",
      "options": { "baseURL": "${base}", "apiKey": "<your app key>" },
      "models": { "${model}": { "name": "QuietBox", "limit": { "context": 131072, "output": 8192 } } }
    }
  }
}`}</CodeBlock>
    </>,
  },
  {
    id: "pi", name: "Pi", homepage: "https://pi.dev",
    blurb: "A small, hackable coding agent for the terminal, from the Pi toolkit trending on GitHub this year.",
    steps: (base, model) => <>
      <span className="muted" style={{ fontSize: 13 }}>Install it: <code>npm i -g --ignore-scripts @earendil-works/pi-coding-agent</code>. Add the box in <code>~/.pi/agent/models.json</code>, then pick it with <code>/model</code>:</span>
      <CodeBlock lang={shellscript} copyOnHover>{`{
  "providers": {
    "quietbox": {
      "baseUrl": "${base}",
      "api": "openai-completions",
      "apiKey": "<your app key>",
      "models": [{ "id": "${model}" }]
    }
  }
}`}</CodeBlock>
    </>,
  },
  {
    id: "ccswitch", name: "CC Switch", homepage: "https://github.com/farion1231/cc-switch",
    blurb: "A desktop app that points Claude Code, Codex, OpenCode and others at any provider, and translates for the ones that can't talk to the box directly.",
    steps: (base, model) => <ol className="guide-list">
      <li>Install it from its releases page (Linux .deb, .rpm or AppImage; also macOS and Windows).</li>
      <li>Add a custom provider: base URL <code>{base}</code>, your app key, model <code>{model}</code>.</li>
      <li>For <b>Claude Code</b> and <b>Codex</b>: under Advanced, set the upstream format to <b>Chat Completions</b> and start its local routing. They speak other APIs (Anthropic Messages, OpenAI Responses), and CC Switch translates. Keep it running while you use them.</li>
    </ol>,
  },
  {
    id: "tradingagents", name: "TradingAgents", homepage: "https://github.com/TauricResearch/TradingAgents", cat: "research", badge: "Research",
    blurb: "A team of AI analysts (fundamentals, news, sentiment, risk) debates a stock and makes a call. Trending this year; for research, not advice.",
    steps: (base, model) => <>
      <span className="muted" style={{ fontSize: 13 }}>Python 3.11 or newer:</span>
      <CodeBlock lang={shellscript} copyOnHover>{`git clone https://github.com/TauricResearch/TradingAgents && cd TradingAgents && pip install .
export TRADINGAGENTS_LLM_PROVIDER=openai_compatible
export TRADINGAGENTS_LLM_BACKEND_URL=${base}
export OPENAI_COMPATIBLE_API_KEY=<your app key>
export TRADINGAGENTS_QUICK_THINK_LLM=${model}
export TRADINGAGENTS_DEEP_THINK_LLM=${model}
tradingagents`}</CodeBlock>
    </>,
  },
  {
    id: "graphify", name: "Graphify", homepage: "https://github.com/Graphify-Labs/graphify", cat: "build", badge: "Build",
    blurb: "Maps a codebase or a folder of documents into a knowledge graph you can explore and your agents can query. Trending this year.",
    steps: (base, model) => <>
      <span className="muted" style={{ fontSize: 13 }}>Install it (<code>uv tool install graphifyy</code>, two y's), then build a graph with the box's model:</span>
      <CodeBlock lang={shellscript} copyOnHover>{`OPENAI_BASE_URL=${base} OPENAI_API_KEY=<your app key> OPENAI_MODEL=${model} \\
  graphify extract ./docs --backend openai`}</CodeBlock>
      <span className="muted" style={{ fontSize: 12 }}>It writes <code>graph.html</code> to open in a browser, and <code>graph.json</code> for agents.</span>
    </>,
  },
];

/** A tool you set up yourself: what to paste, and a key made for it. */
function CodeToolTile({ t, base, model }: { t: (typeof CODE_TOOLS)[number]; base: string; model: string }) {
  const guide = useSheet();
  const owner = useOwner();
  const [key, setKey] = useState<string | null>(null);
  const makeKey = async () => {
    try { const r = await post<{ key: string }>("/keys", { name: t.name }); setKey(r.key); } catch (e) { toastError(e); }
  };
  return (
    <Card className="app-tile">
      <div className="row-between">
        <AppLogo id={t.id} name={t.name} />
        <Badge size="sm" variant="contrast" subtle>{t.badge ?? "Code"}</Badge>
      </div>
      <div><Typography variant="heading-xs">{t.name}</Typography><Typography variant="copy-sm" className="muted">{t.blurb}</Typography></div>
      <div className="grow" />
      <div className="row">
        {t.onBox && <Button size="sm" title="Its own terminal app, running on the box, shown in TT Station" onClick={() => { try { localStorage.setItem(AGENT_KEY, t.onBox!); } catch { /* private */ } navigate("terminals"); }}>Open on the box</Button>}
        <Button size="sm" variant="subtle" onClick={guide.open}>{t.onBox ? "On your computer" : "Set it up"}</Button>
        <Button size="sm" variant="ghost" onClick={() => openExternal(t.homepage)}>Website</Button>
      </div>
      <Sheet ref={guide.ref} popover title={`Use ${t.name} with your box`} description="It runs on your computer and talks to the box's model.">
        <div className="stack" style={{ gap: 12 }}>
          {t.steps(base, model)}
          {key ? <PasteValue label={`App key for ${t.name}`} value={key} secret />
            : <div className="stack" style={{ gap: 4 }}><div><Button size="sm" disabled={!owner} onClick={makeKey}>Make an app key for {t.name}</Button></div>{!owner && <span className="muted" style={{ fontSize: 12 }}>{OWNER_ONLY}</span>}</div>}
          <span className="muted" style={{ fontSize: 12 }}>The key is shown once, and you can revoke it under "Your box as an API". Requests follow whatever model the box is serving.</span>
        </div>
      </Sheet>
    </Card>
  );
}

/**
 * What research needs from the box, above the Research apps: each app runs the
 * research loop (plan, search, read, reflect, write a cited report); the box
 * gives it the model, a private search engine and fast reading of long pages.
 */
function ResearchNote() {
  const serve = usePoll(() => get<{ phase: string; served_id: string | null; model: string | null }>("/serve").catch(() => null), 15000);
  const m = serve.data?.served_id ?? serve.data?.model;
  return (
    <Admonition variant="info" subtle size="sm">
      A research app plans the questions, searches, reads dozens of pages and writes a cited report, over minutes. It runs on the model this box serves
      {m ? <> (now <b>{m.split("/").pop()}</b>)</> : null}, which should handle tool calls and a long context; Qwen3-32B is a good start.
      Searches go through SearXNG on the box, a private search engine set up with the first research app, so your questions aren't tied to an account.
    </Admonition>
  );
}

// ---------------------------------------------------------------- skill packs

/** Trending agent skill packs, and how each agent takes them. `hermes` packs install in one click. */
const SKILL_PACKS: { id: string; name: string; repo: string; blurb: string; hermes?: boolean; note?: string; how: [string, string][] }[] = [
  {
    id: "superpowers", name: "Superpowers", repo: "obra/superpowers", hermes: true,
    blurb: "A complete way of working for coding agents: brainstorm, plan, test first, review. The most-starred skills pack this year.",
    how: [["Pi", "pi install git:github.com/obra/superpowers"], ["Claude Code", "/plugin install superpowers@claude-plugins-official"], ["OpenCode", "Follow .opencode/INSTALL.md in the repo"]],
  },
  {
    id: "mattpocock", name: "Matt Pocock's skills", repo: "mattpocock/skills",
    blurb: "The skills a well-known TypeScript teacher uses every day: planning, refactoring, writing tests, explaining code.",
    how: [["OpenCode", "npx skills@latest add mattpocock/skills -a opencode"], ["Pi", "npx skills@latest add mattpocock/skills -a pi"], ["Claude Code", "claude plugin install mattpocock-skills@claude-plugins-official"]],
  },
  {
    id: "gstack", name: "gstack", repo: "garrytan/gstack",
    blurb: "23 opinionated tools that give an agent a whole team's roles: product, design, engineering, QA and release. Needs Bun.",
    how: [["OpenCode", "git clone --depth 1 https://github.com/garrytan/gstack.git ~/gstack && cd ~/gstack && ./setup --host opencode"], ["Claude Code", "git clone --depth 1 https://github.com/garrytan/gstack.git ~/.claude/skills/gstack && cd ~/.claude/skills/gstack && ./setup"]],
  },
  {
    id: "uiux", name: "UI/UX Pro Max", repo: "nextlevelbuilder/ui-ux-pro-max-skill",
    blurb: "Design know-how for agents that build interfaces: layouts, color, type and accessibility, across web and mobile.",
    how: [["OpenCode", "npm i -g ui-ux-pro-max-cli && uipro init --ai opencode"], ["Claude Code", "npm i -g ui-ux-pro-max-cli && uipro init --ai claude"]],
  },
  {
    id: "anthropic", name: "Anthropic's skills", repo: "anthropics/skills",
    blurb: "Anthropic's public skills, including making Word, PDF, PowerPoint and Excel files.",
    note: "The document skills are source-available, not open source: fine to use, check before you share them.",
    how: [["Claude Code", "/plugin marketplace add anthropics/skills"], ["Others", "Copy a skill's folder into ~/.agents/skills"]],
  },
];

function SkillTile({ p }: { p: (typeof SKILL_PACKS)[number] }) {
  const sk = usePoll(() => get<{ hermes: boolean; in_hermes: string[] }>("/skills").catch(() => null), 60000);
  const guide = useSheet();
  const [busy, setBusy] = useState(false);
  const inHermes = sk.data?.in_hermes.includes(p.id);
  const addToHermes = async () => {
    setBusy(true);
    try { const r = await post<{ note: string }>(`/skills/${p.id}/hermes`); toastOk(`${p.name} is in Hermes. ${r.note}`); sk.refresh(); }
    catch (e) { toastError(e); } finally { setBusy(false); }
  };
  return (
    <Card className="app-tile">
      <div className="row-between">
        <div className="app-logo">✦</div>
        <div className="row" style={{ gap: 6 }}>
          <Badge size="sm" variant="contrast" subtle>Skills</Badge>
          {inHermes && <StatusIndicator label="In Hermes" state="ready" />}
        </div>
      </div>
      <div><Typography variant="heading-xs">{p.name}</Typography><Typography variant="copy-sm" className="muted">{p.blurb}</Typography></div>
      {p.note && <Typography variant="copy-xs" className="muted">{p.note}</Typography>}
      <div className="grow" />
      <div className="row">
        {p.hermes && sk.data?.hermes && !inHermes && <Button size="sm" disabled={busy} onClick={addToHermes}>{busy ? "Adding…" : "Add to Hermes"}</Button>}
        <Button size="sm" variant={p.hermes && sk.data?.hermes && !inHermes ? "ghost" : "subtle"} onClick={guide.open}>Add to an agent</Button>
      </div>
      <Sheet ref={guide.ref} popover title={`Add ${p.name}`} description={`From github.com/${p.repo}. Skills run inside your agent, on the box's model.`}>
        <div className="stack" style={{ gap: 12 }}>
          {p.hermes && <span className="muted" style={{ fontSize: 13 }}><b>Hermes Agent</b>: use “Add to Hermes” on the tile{sk.data?.hermes ? "" : " (once Hermes is set up in Agents)"}.</span>}
          {p.how.map(([agent, cmd]) => (
            <div key={agent} className="stack" style={{ gap: 4 }}>
              <b style={{ fontSize: 13 }}>{agent}</b>
              {/^(npx|npm|git|pi |claude|\/plugin|uv )/.test(cmd) ? <CodeBlock lang={shellscript} copyOnHover>{cmd}</CodeBlock> : <span className="muted" style={{ fontSize: 13 }}>{cmd}</span>}
            </div>
          ))}
          <div><Button size="sm" variant="ghost" onClick={() => openExternal(`https://github.com/${p.repo}`)}>Open on GitHub</Button></div>
        </div>
      </Sheet>
    </Card>
  );
}

/** One entry in the store: where it's filed, how to find it, and its tile. */
interface Entry { key: string; cat: Cat; text: string; on: boolean; node: ReactNode }

export function Connect() {
  const clients = usePoll(() => get<{ clients: Client[] }>("/clients").catch(() => ({ clients: [] })), 8000);
  const apps = usePoll(() => get<{ apps: BoxApp[] }>("/apps").catch(() => ({ apps: [] })), 4000);
  const connect = usePoll(() => get<{ urls: { label: string; url: string }[]; models: { id: string }[] }>("/connect"), 30000);
  const keys = usePoll(() => get<{ keys: AppKey[] }>("/keys"), 15000);
  const owner = useOwner();
  const ssh = usePoll(() => get<SshInfo>("/ssh/info"), 30000);
  const hermes = usePoll(() => get<Hermes>("/hermes").catch(() => null), 15000);
  const [busy, setBusy] = useState<string | null>(null);
  const [newKey, setNewKey] = useState<{ name: string; key: string } | null>(null);
  const [keyName, setKeyName] = useState("");
  const [query, setQuery] = useState("");
  const [cat, setCat] = useState<Cat>(() => { try { return (localStorage.getItem("tt.connect.cat") as Cat) || "all"; } catch { return "all"; } });
  const pickCat = (c: Cat) => { setCat(c); try { localStorage.setItem("tt.connect.cat", c); } catch { /* private */ } };

  const act = async (id: string, fn: () => Promise<unknown>) => {
    setBusy(id);
    try { await fn(); } catch (e) { toastError(e); } finally { setBusy(null); clients.refresh(); apps.refresh(); }
  };

  const base = connect.data?.urls?.[0]?.url ?? `http://${location.hostname}:${location.port}/v1`;
  const model = connect.data?.models?.[0]?.id ?? "the-served-model";

  const hermesOn = !!hermes.data?.running;
  const entries: Entry[] = [
    ...(clients.data?.clients ?? []).map((c): Entry => ({
      key: c.tool, cat: "chat", on: c.running, text: `${c.name} ${BLURBS[c.tool] ?? ""}`,
      node: (
        <Card key={c.tool} className="app-tile">
          <div className="row-between">
            <AppLogo id={c.tool} name={c.name} />
            <div className="row" style={{ gap: 6 }}>
              <Badge size="sm" variant="contrast" subtle>Chat</Badge>
              <StatusIndicator label={c.running ? "Running" : "Stopped"} state={c.running ? "ready" : "queued"} />
            </div>
          </div>
          <div><Typography variant="heading-xs">{c.name}</Typography><Typography variant="copy-sm" className="muted">{BLURBS[c.tool] ?? ""}</Typography></div>
          <div className="grow" />
          <div className="row">
            {c.running && c.url && <Button size="sm" onClick={() => open(c.url!, c.name)}>Open</Button>}
            {!c.running && <Button size="sm" variant="subtle" disabled={busy === c.tool || !c.available}
              onClick={() => act(c.tool, () => post(`/clients/${c.tool}/start`))}>{busy === c.tool ? "Starting…" : "Start"}</Button>}
            {c.running && <Button size="sm" variant="ghost" disabled={busy === c.tool} onClick={() => act(c.tool, () => post(`/clients/${c.tool}/stop`))}>Stop</Button>}
          </div>
        </Card>
      ),
    })),
    { key: "hermes", cat: "agents", on: hermesOn, text: "Hermes Agent Nous Research agent memory skills tools", node: <HermesTile key="hermes" /> },
    ...(apps.data?.apps ?? []).map((a): Entry => ({
      key: a.id, cat: catOf(a.category), on: a.state === "running" || a.state === "installing", text: `${a.name} ${a.blurb} ${a.category}`,
      node: (
        <Card key={a.id} className="app-tile">
          <div className="row-between">
            <AppLogo id={a.id} name={a.name} />
            <div className="row" style={{ gap: 6 }}>
              <Badge size="sm" variant="contrast" subtle>{a.category}</Badge>
              {a.state !== "not_installed" && <StatusIndicator
                label={a.state === "installing" ? a.step ?? "Installing" : a.state === "failed" ? "Failed" : a.state === "running" ? "Running" : "Stopped"}
                state={a.state === "running" ? "ready" : a.state === "installing" ? "progress" : a.state === "failed" ? "error" : "queued"}
                animated={a.state === "installing"} />}
            </div>
          </div>
          <div><Typography variant="heading-xs">{a.name}</Typography><Typography variant="copy-sm" className="muted">{a.blurb}</Typography></div>
          {a.needs && a.needs.length > 0 && a.state === "not_installed" && <Typography variant="copy-xs" className="muted">Also sets up {a.needs.join(", ")}.</Typography>}
          {a.error && <Typography variant="copy-xs" style={{ color: "var(--vesper-text-error)" }}>{a.error}</Typography>}
          {a.hint && a.state === "running" && <Typography variant="copy-xs" className="muted">First time: {a.hint}</Typography>}
          {a.connect && (
            <details>
              <summary className="muted" style={{ fontSize: 12, cursor: "pointer" }}>What to paste into {a.name}</summary>
              <div className="stack" style={{ gap: 6, marginTop: 6 }}>
                <PasteValue label="Base URL" value={a.connect.base_url} />
                {a.connect.api_key && <PasteValue label="API key" value={a.connect.api_key} secret />}
                {a.connect.model && <PasteValue label="Model" value={a.connect.model} />}
              </div>
            </details>
          )}
          {a.login && a.state === "running" && (
            <span className="muted mono" style={{ fontSize: 12 }}>
              {a.login.user ? `Sign in: ${a.login.user} / ${a.login.password}` : `Access code: ${a.login.access_code}`}
            </span>
          )}
          <div className="grow" />
          <div className="row">
            {a.state === "running" && a.url && <Button size="sm" onClick={() => open(a.url!, a.name)}>Open</Button>}
            {(a.state === "not_installed" || a.state === "failed") && <Button size="sm" variant="subtle" disabled={busy === a.id}
              onClick={() => act(a.id, () => post(`/apps/${a.id}/start`))}>Install</Button>}
            {a.state === "stopped" && <Button size="sm" variant="subtle" onClick={() => act(a.id, () => post(`/apps/${a.id}/start`))}>Start</Button>}
            {a.state === "running" && <Button size="sm" variant="ghost" onClick={async () => {
              if (a.used_by?.length && !(await ask(`Stop ${a.name}?`, { body: `${a.used_by.join(" and ")} use${a.used_by.length > 1 ? "" : "s"} it, and won't work fully while it's stopped. Starting ${a.used_by.length > 1 ? "them" : "it"} again starts ${a.name} too.`, confirm: "Stop it" }))) return;
              act(a.id, () => post(`/apps/${a.id}/stop`));
            }}>Stop</Button>}
            {(a.state === "stopped" || a.state === "failed") && <Button size="sm" variant="ghost"
              onClick={async () => { if (await ask(`Remove ${a.name}?`, { body: "Its data is kept and comes back if you install it again.", confirm: "Remove", danger: true })) act(a.id, () => del(`/apps/${a.id}`)); }}>Remove</Button>}
            {a.local_only && <span className="muted" style={{ fontSize: 11 }}>This computer only</span>}
          </div>
        </Card>
      ),
    })),
    ...CODE_TOOLS.map((t): Entry => ({ key: t.id, cat: t.cat ?? "code", on: false, text: `${t.name} ${t.blurb} code editor`, node: <CodeToolTile key={t.id} t={t} base={base} model={model} /> })),
    ...SKILL_PACKS.map((p): Entry => ({ key: `skill-${p.id}`, cat: "skills", on: false, text: `${p.name} ${p.blurb} skills`, node: <SkillTile key={p.id} p={p} /> })),
    ...(ssh.data ? [{ key: "termius", cat: "terminal" as Cat, on: false, text: "Termius SSH terminal phone", node: <TermiusTile key="termius" ssh={ssh.data} refresh={ssh.refresh} /> }] : []),
    ...CLOUDS.map((c): Entry => ({
      key: c.id, cat: "cloud", on: false, text: `${c.name} ${c.blurb} cloud`,
      node: (
        <Card key={c.id} className="app-tile">
          <div className="row-between">
            <img className="app-logo img" src={c.logo} alt="" />
            <Badge size="sm" variant="contrast" subtle>Cloud</Badge>
          </div>
          <div><Typography variant="heading-xs">{c.name}</Typography><Typography variant="copy-sm" className="muted">{c.blurb}</Typography></div>
          <div className="row" style={{ gap: 6, minWidth: 0 }}>
            <code className="grow" style={{ fontSize: 12, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", minWidth: 0 }}>{c.base}</code>
            <Button size="xs" variant="ghost" onClick={() => void copyText(c.base, "Address copied.")}>Copy</Button>
          </div>
          <div className="grow" />
          <div className="row"><Button size="sm" variant="subtle" onClick={() => openExternal(c.url)}>{c.cta}</Button></div>
        </Card>
      ),
    })),
  ];
  const q = query.trim().toLowerCase();
  const shown = entries.filter((e) => (cat === "all" || e.cat === cat) && (!q || e.text.toLowerCase().includes(q)));
  const onBox = cat === "all" && !q ? entries.filter((e) => e.on) : [];
  const about = CATS.find((c) => c.id === cat)?.about;

  return (
    <Page title="Connect" subtitle="Apps for your box: chat, agents, research, images, coding and more, each wired to the models on this box. And the box as a private OpenAI-compatible API.">
      <div className="row-between wrap">
        <div style={{ width: 320 }}><TextInput value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Search apps" /></div>
        <div className="pill-tabs wrap-tabs" role="tablist" aria-label="App categories">
          {CATS.map((c) => {
            const n = c.id === "all" ? entries.length : entries.filter((e) => e.cat === c.id).length;
            return n === 0 && c.id !== "all" ? null : (
              <button key={c.id} role="tab" aria-selected={cat === c.id} className={cat === c.id ? "on" : ""} onClick={() => pickCat(c.id)}>
                {c.label} <span className="muted">{n}</span>
              </button>
            );
          })}
        </div>
      </div>

      {onBox.length > 0 && <>
        <div className="section-title"><Eyebrow>On your box</Eyebrow><span className="muted">Running now. Each opens in its own window and talks only to your box.</span></div>
        <div className="grid grid-3">{onBox.map((e) => e.node)}</div>
      </>}

      <div className="section-title">
        <Eyebrow>{cat === "all" ? "Everything" : CATS.find((c) => c.id === cat)?.label}</Eyebrow>
        {about && <span className="muted">{about}</span>}
      </div>
      {cat === "research" && <ResearchNote />}
      <div className="grid grid-3">{shown.filter((e) => !onBox.includes(e)).map((e) => e.node)}</div>
      {shown.length === 0 && <span className="muted">Nothing matches.</span>}
      {(cat === "terminal" || cat === "all") && !q && ssh.data && <div className="grid grid-2" style={{ alignItems: "start" }}><SshCard ssh={ssh.data} /></div>}

      <div className="section-title"><Eyebrow>Your box as an API</Eyebrow></div>
      <div className="grid grid-2" style={{ alignItems: "start" }}>
        <Card>
          <Typography variant="copy-sm" className="muted">Any app that speaks the OpenAI API works with the box: point it at this address with an app key. Requests follow whatever model is serving. From other computers the address is encrypted with the box's own certificate; if an app refuses it, trust the certificate in that app, or allow unencrypted access in Settings → Pairing and security.</Typography>
          {(connect.data?.urls ?? []).map((u) => (
            <div key={u.url} className="stack" style={{ gap: 4 }}><Eyebrow>{u.label}</Eyebrow><Snippet>{u.url}</Snippet></div>
          ))}
          <Eyebrow>App keys</Eyebrow>
          {newKey && (
            <Admonition variant="success" subtle size="sm">
              <div className="stack" style={{ gap: 6 }}><span>Key for <b>{newKey.name}</b>. Copy it now: it isn't shown again.</span><Snippet>{newKey.key}</Snippet></div>
            </Admonition>
          )}
          <div className="list">
            {(keys.data?.keys ?? []).map((k) => <KeyRow key={k.id} k={k} owner={owner} onChanged={() => keys.refresh()} />)}
          </div>
          {!owner && <Typography variant="copy-xs" className="muted">{OWNER_ONLY}</Typography>}
          <div className="row">
            <div className="grow"><TextInput size="sm" value={keyName} disabled={!owner} onChange={(e) => setKeyName(e.target.value)} placeholder="Name it after the app, e.g. Cursor" /></div>
            <Button size="sm" variant="subtle" disabled={!owner || !keyName.trim()} onClick={async () => {
              try {
                const r = await post<{ key: string; info: AppKey }>("/keys", { name: keyName.trim() });
                setNewKey({ name: r.info.name, key: r.key });
                setKeyName("");
                keys.refresh();
              } catch (e) { toastError(e); }
            }}>New key</Button>
          </div>
        </Card>
        <Card>
          <Eyebrow>Use it from code</Eyebrow>
          <CodeBlock lang={python} copyOnHover>{`from openai import OpenAI

client = OpenAI(base_url="${base}", api_key="ttq-…")
reply = client.chat.completions.create(
    model="${model}",
    messages=[{"role": "user", "content": "Hello from my QuietBox"}],
)
print(reply.choices[0].message.content)`}</CodeBlock>
          <CodeBlock lang={shellscript} copyOnHover>{`curl ${base}/chat/completions \\
  -H "Authorization: Bearer ttq-…" -H "Content-Type: application/json" \\
  -d '{"model": "${model}", "messages": [{"role": "user", "content": "Hi"}]}'`}</CodeBlock>
          <Typography variant="copy-xs" className="muted">In VS Code (Continue, Cline) or Cursor: choose "OpenAI-compatible", paste the address and a key. The same code works with Tenstorrent Console or OpenRouter: change the address and the key.</Typography>
        </Card>
      </div>
    </Page>
  );
}

const SCOPE_LABELS: [string, string][] = [["chat", "Chat and completions"], ["images", "Images"], ["embeddings", "Embeddings and reranking"]];
const tokens = (n: number) => n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${(n / 1e3).toFixed(n >= 1e4 ? 0 : 1)}K` : `${n}`;

/** One app key: who it's for, how much it's used, and its limits. */
function KeyRow({ k, owner, onChanged }: { k: AppKey; owner: boolean; onChanged: () => void }) {
  const sheet = useSheet();
  const [rpm, setRpm] = useState("");
  const [perDay, setPerDay] = useState("");
  const [scopes, setScopes] = useState<string[]>([]);
  const [paused, setPaused] = useState(false);
  const [busy, setBusy] = useState(false);
  const open = () => {
    setRpm(k.limits?.requests_per_minute ? String(k.limits.requests_per_minute) : "");
    setPerDay(k.limits?.tokens_per_day ? String(k.limits.tokens_per_day) : "");
    setScopes(k.scopes?.length ? k.scopes : SCOPE_LABELS.map(([s]) => s));
    setPaused(k.paused);
    sheet.open();
  };
  const save = async () => {
    const n = (v: string) => { const x = parseInt(v.replace(/[^0-9]/g, ""), 10); return Number.isFinite(x) && x > 0 ? x : null; };
    if (scopes.length === 0) { toastError(new Error("Leave it at least one thing to use, or pause it instead.")); return; }
    setBusy(true);
    try {
      await patch(`/keys/${k.id}`, { limits: { requests_per_minute: n(rpm), tokens_per_day: n(perDay) }, scopes, paused });
      sheet.close(); onChanged(); toastOk(`Saved ${k.name}.`);
    } catch (e) { toastError(e); }
    finally { setBusy(false); }
  };
  const togglePause = async () => {
    try { await patch(`/keys/${k.id}`, { paused: !k.paused }); onChanged(); } catch (e) { toastError(e); }
  };
  const revoke = async () => {
    if (!(await ask(`Revoke the key for ${k.name}?`, { body: "Apps using it stop working right away.", confirm: "Revoke", danger: true }))) return;
    try { await del(`/keys/${k.id}`); onChanged(); toastOk("Revoked."); } catch (e) { toastError(e); }
  };
  const today = k.today ? k.today.input_tokens + k.today.output_tokens : 0;
  const days = k.days ?? [];
  const budget = k.limits?.tokens_per_day ?? null;
  const limits = [
    k.limits?.requests_per_minute ? `${k.limits.requests_per_minute}/min` : null,
    budget ? `${tokens(budget)} tokens/day` : null,
    k.scopes?.length ? SCOPE_LABELS.filter(([s]) => k.scopes.includes(s)).map(([, l]) => l.split(" ")[0]).join(" + ") + " only" : null,
  ].filter(Boolean);
  return (
    <div className={`list-row key-row ${k.paused ? "paused" : ""}`}>
      <div className="grow" style={{ minWidth: 0 }}>
        <div className="row" style={{ gap: 6 }}>
          <b>{k.name}</b>
          {k.paused && <Badge size="sm" variant="warning" subtle>Paused</Badge>}
          {k.expires != null && <Badge size="sm" variant={k.expires < Date.now() / 1000 ? "warning" : "contrast"} subtle>{k.expires < Date.now() / 1000 ? "Expired" : `Expires ${new Date(k.expires * 1000).toLocaleDateString()}`}</Badge>}
          {limits.map((l) => <Badge key={l} size="sm" variant="contrast" subtle>{l}</Badge>)}
        </div>
        <div className="muted mono" style={{ fontSize: 12 }}>
          {k.hint} · {tokens(today)} tokens today{budget ? ` of ${tokens(budget)}` : ""} · {k.requests} requests{k.last_used ? ` · used ${ago(k.last_used)}` : ""}
        </div>
      </div>
      {days.some(([, u]) => u.input_tokens + u.output_tokens > 0) && (
        <div className="key-spark">
          <Spark values={days.map(([, u]) => u.input_tokens + u.output_tokens)} labels={days.map(([d]) => d)} format={(v) => `${tokens(v)} tokens`} height={28} title="Tokens a day, last 14 days" />
        </div>
      )}
      <Button size="xs" variant="subtle" disabled={!owner} onClick={open}>Limits</Button>
      <Button size="xs" variant="ghost" disabled={!owner} onClick={togglePause}>{k.paused ? "Resume" : "Pause"}</Button>
      <Button size="xs" variant="ghost" disabled={!owner} onClick={revoke}>Revoke</Button>
      <Sheet ref={sheet.ref} popover title={`Limits for ${k.name}`} description="For sharing the box: keep one app or person from crowding out the others. Leave a limit empty for none.">
        <div className="stack" style={{ gap: 14 }}>
          <label className="stack" style={{ gap: 4 }}>
            <span>Requests a minute</span>
            <TextInput size="sm" inputMode="numeric" value={rpm} onChange={(e) => setRpm(e.target.value)} placeholder="No limit" />
          </label>
          <label className="stack" style={{ gap: 4 }}>
            <span>Tokens a day</span>
            <TextInput size="sm" inputMode="numeric" value={perDay} onChange={(e) => setPerDay(e.target.value)} placeholder="No limit" />
            <span className="muted" style={{ fontSize: 12 }}>Prompt and answer together. Resets at midnight on the box. Today so far: {tokens(today)}.</span>
          </label>
          <div className="stack" style={{ gap: 6 }}>
            <span>It can use</span>
            {SCOPE_LABELS.map(([s, label]) => (
              <div key={s} className="row-between">
                <span>{label}</span>
                <Switch size="sm" checked={scopes.includes(s)} aria-label={label}
                  onChange={(e) => setScopes((cur) => e.target.checked ? [...new Set([...cur, s])] : cur.filter((x) => x !== s))} />
              </div>
            ))}
          </div>
          <div className="row-between">
            <span>Paused <span className="muted" style={{ fontSize: 12 }}>(turned away until you resume it)</span></span>
            <Switch size="sm" checked={paused} aria-label="Paused" onChange={(e) => setPaused(e.target.checked)} />
          </div>
          <div><Button disabled={busy} onClick={save}>{busy ? "Saving…" : "Save"}</Button></div>
        </div>
      </Sheet>
    </div>
  );
}
