// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Connect: the most popular open-source tools a Tenstorrent box can serve,
// each running on the box's own models. Chat apps, agents (Hermes, OpenClaw,
// OpenHands), research apps and visual builders the box installs and wires to
// its models; coding tools for your own computer that point at the box's
// OpenAI API; and the box itself as a private OpenAI-compatible API (address,
// models, app keys, code snippets) for everything else.

import { type ReactNode, useState } from "react";
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
import flowiseLogo from "../assets/logos/flowise.png";
import hermesLogo from "../assets/logos/hermes.png";
import searxngLogo from "../assets/logos/searxng.png";
import langflowLogo from "../assets/logos/langflow.png";
import continueLogo from "../assets/logos/continue.png";
import clineLogo from "../assets/logos/cline.png";
import aiderLogo from "../assets/logos/aider.png";
import { type AppKey, type Client, del, get, patch, post, put, ago } from "../api";
import { boxOnlyReason } from "../boxapps";
import { Card, copyText, Eyebrow, Page, Spark, toastError, toastOk, usePoll } from "../ui";
import { Switch } from "@tenstorrent/vesper/switch";
import { ask } from "../dialog";
import { OWNER_ONLY, OWNER_ONLY_HOST, useOwner } from "../role";

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
  /** Box-only (the default) or open to the network. */
  network?: {
    open: boolean; can_share: boolean; why_not?: string; who?: string; note?: string;
    /** Published on the network right now. */
    on_network: boolean;
    /** Running with the address it was made with, not its setting's: until it's next started here. */
    restart_to_apply: boolean;
  };
}

/** Opens a store app; from a browser on another computer, a box-only app says why it can't. */
function openBoxApp(a: BoxApp) {
  const why = boxOnlyReason(a.url!, location.hostname, !!window.ttDesktop, !!a.network?.can_share);
  if (why) toastError(new Error(why));
  else open(a.url!, a.name);
}

/** "Open to the network": off (box only) by default; on only after the owner reads who can reach it. */
function NetworkSetting({ a, onChanged }: { a: BoxApp; onChanged: () => void }) {
  const [busy, setBusy] = useState(false);
  const n = a.network;
  if (!n) return null;
  if (!n.can_share) {
    return (
      <details>
        <summary className="muted" style={{ fontSize: 12, cursor: "pointer" }}>On this box only</summary>
        <Typography variant="copy-xs" className="muted">{n.why_not}</Typography>
        {n.on_network && <Typography variant="copy-xs" style={{ color: "var(--vesper-text-error)" }}>It's still reachable from the network until you next start it here.</Typography>}
      </details>
    );
  }
  const set = async (open: boolean) => {
    if (open && !(await ask(`Open ${a.name} to the network?`, {
      body: [n.who, n.note, `It changes the next time ${a.name} starts here${a.state === "running" ? " (it's running now: restart it to apply)" : ""}. You can turn this off any time.`].filter(Boolean).join(" "),
      confirm: "Open to the network",
    }))) return;
    setBusy(true);
    try {
      await put(`/apps/${a.id}/network`, { open });
      toastOk(open ? `${a.name} opens to the network the next time it starts.` : `${a.name} goes back to this box only the next time it starts.`);
    } catch (e) { toastError(e); } finally { setBusy(false); onChanged(); }
  };
  const restart = async () => {
    setBusy(true);
    try { await post(`/apps/${a.id}/stop`); await post(`/apps/${a.id}/start`); } catch (e) { toastError(e); } finally { setBusy(false); onChanged(); }
  };
  return (
    <div className="stack" style={{ gap: 4 }}>
      <Switch size="sm" label="Open to the network" checked={n.open} disabled={busy} onChange={(e) => void set(e.target.checked)} />
      {n.restart_to_apply && (
        <div className="row wrap" style={{ gap: 6 }}>
          <Typography variant="copy-xs" style={{ color: n.on_network ? "var(--vesper-text-error)" : undefined }} className={n.on_network ? "" : "muted"}>
            {n.on_network ? "Still reachable from the network until it's restarted here." : "Still on this box only until it's restarted here."}
          </Typography>
          {a.state === "running" && <Button size="xs" variant="subtle" disabled={busy} onClick={() => void restart()}>Restart now</Button>}
        </div>
      )}
    </div>
  );
}

const BLURBS: Record<string, string> = {
  openwebui: "The most popular self-hosted chat: documents, web search, many users.",
  anythingllm: "Chat with your documents and build agents, all on the box.",
};

/** Each app's own logo (from its image on the box, or its project's repo). */
const LOGOS: Record<string, string> = {
  openwebui: openwebuiLogo, anythingllm: anythingllmLogo, lobechat: lobechatLogo,
  jupyter: jupyterLogo, flowise: flowiseLogo,
  searxng: searxngLogo, langflow: langflowLogo, continue: continueLogo, cline: clineLogo, aider: aiderLogo,
};
/** Logos that are a filled square (the project's avatar): shown edge to edge. */
const FULL = new Set(["searxng", "langflow", "continue", "cline", "aider"]);

function AppLogo({ id, name }: { id: string; name: string }) {
  const src = LOGOS[id];
  return src ? <img className={`app-logo img ${FULL.has(id) ? "full" : ""}`} src={src} alt="" /> : <div className="app-logo">{name[0]}</div>;
}

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
  const owner = useOwner();
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
            <div><Button size="sm" disabled={busy === "open" || !s.has_session || !owner} title={owner ? undefined : OWNER_ONLY_HOST} onClick={() => act("open", () => post("/hermes/open"))}>Open Hermes</Button></div>
          </>}
      </Step>
      <Step n={4} title="Connect it to your box" done={connected} active={step === 4}>
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
      {step === 6 && <Admonition variant="success" subtle size="sm">Hermes Agent is set up, running on your box's {shortName(s.serving_model)}.</Admonition>}
    </div>
  );
}

/** Hermes Agent (Nous Research): its own desktop app, set up with a guide. */
function HermesTile() {
  const owner = useOwner();
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
        {ready ? <Button size="sm" disabled={!s?.has_session || !owner} title={!owner ? OWNER_ONLY_HOST : s?.running ? "Bring Hermes to the front on the box's screen" : "Open Hermes on the box's screen"} onClick={async () => {
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

type Cat = "all" | "chat" | "agents" | "research" | "build" | "code";
const CATS: { id: Cat; label: string; about: string }[] = [
  { id: "all", label: "All", about: "" },
  { id: "chat", label: "Chat", about: "Chat apps for you and your team, with documents and web search." },
  { id: "agents", label: "Agents", about: "Assistants that remember, use tools and work through tasks." },
  { id: "research", label: "Research", about: "Deep research: questions in, cited reports out, from the web and your documents." },
  { id: "build", label: "Build", about: "Notebooks and visual builders for your own AI apps." },
  { id: "code", label: "Code", about: "Open-source coding tools on your computer, on the box's model through its OpenAI API. TT Station's own Code page is the built-in way: a coding agent in the window, in a sandbox on the box." },
];

/** The box app's category (from `/apps`) as a store category. */
function catOf(category: string): Cat {
  const c = category.toLowerCase();
  if (c.includes("research") || c.includes("search")) return "research";
  if (c.includes("agent")) return "agents";
  if (c.includes("code")) return "code";
  if (c.includes("chat")) return "chat";
  return "build";
}

// ---------------------------------------------------------------- coding tools

/** Coding tools that point at any OpenAI-compatible server: how to set each one up. */
const CODE_TOOLS: { id: string; name: string; blurb: string; homepage: string; steps: (base: string, model: string) => ReactNode }[] = [
  {
    id: "continue", name: "Continue", homepage: "https://continue.dev",
    blurb: "An open-source AI coding assistant for VS Code and JetBrains: chat, edit and autocomplete.",
    steps: (base, model) => <>
      <span className="muted" style={{ fontSize: 13 }}>In Continue's <code>config.yaml</code>, add a model:</span>
      <CodeBlock lang={shellscript} copyOnHover>{`models:
  - name: TT Station
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
    id: "aider", name: "Aider", homepage: "https://aider.chat",
    blurb: "Pair programming in your terminal: it edits the files in your git repo and commits each change.",
    steps: (base, model) => <>
      <span className="muted" style={{ fontSize: 13 }}>Run it in your repository:</span>
      <CodeBlock lang={shellscript} copyOnHover>{`aider --openai-api-base ${base} --openai-api-key <your app key> --model openai/${model}`}</CodeBlock>
    </>,
  },
  {
    id: "opencode", name: "OpenCode", homepage: "https://opencode.ai",
    blurb: "The open-source coding agent for your terminal (trending #6 on GitHub this year), with any model provider.",
    steps: (base, model) => <>
      <span className="muted" style={{ fontSize: 13 }}>Install it: <code>curl -fsSL https://opencode.ai/install | bash</code>. Then add the box in <code>~/.config/opencode/opencode.jsonc</code> (or a project's <code>opencode.json</code>) and pick it with <code>/models</code>:</span>
      <CodeBlock lang={shellscript} copyOnHover>{`{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "quietbox": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "TT Station",
      "options": { "baseURL": "${base}", "apiKey": "<your app key>" },
      "models": { "${model}": { "name": "TT Station", "limit": { "context": 131072, "output": 8192 } } }
    }
  }
}`}</CodeBlock>
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
        <Badge size="sm" variant="contrast" subtle>Code</Badge>
      </div>
      <div><Typography variant="heading-xs">{t.name}</Typography><Typography variant="copy-sm" className="muted">{t.blurb}</Typography></div>
      <div className="grow" />
      <div className="row">
        <Button size="sm" variant="subtle" onClick={guide.open}>Set it up</Button>
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

/** One entry in the store: where it's filed, how to find it, and its tile. */
interface Entry { key: string; cat: Cat; text: string; on: boolean; node: ReactNode }

export function Connect() {
  const clients = usePoll(() => get<{ clients: Client[] }>("/clients").catch(() => ({ clients: [] })), 8000);
  const apps = usePoll(() => get<{ apps: BoxApp[] }>("/apps").catch(() => ({ apps: [] })), 4000);
  const connect = usePoll(() => get<{ urls: { label: string; url: string }[]; models: { id: string }[] }>("/connect"), 30000);
  const keys = usePoll(() => get<{ keys: AppKey[] }>("/keys"), 15000);
  const owner = useOwner();
  const hermes = usePoll(() => get<Hermes>("/hermes").catch(() => null), 15000);
  const [busy, setBusy] = useState<string | null>(null);
  const [newKey, setNewKey] = useState<{ name: string; key: string } | null>(null);
  const [keyName, setKeyName] = useState("");
  const [query, setQuery] = useState("");
  // A category saved before it was taken out of the store falls back to All.
  const [cat, setCat] = useState<Cat>(() => { try { const c = localStorage.getItem("tt.connect.cat"); return CATS.find((x) => x.id === c)?.id ?? "all"; } catch { return "all"; } });
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
            {a.state === "running" && a.url && <Button size="sm" onClick={() => openBoxApp(a)}>Open</Button>}
            {(a.state === "not_installed" || a.state === "failed") && <Button size="sm" variant="subtle" disabled={busy === a.id}
              onClick={() => act(a.id, () => post(`/apps/${a.id}/start`))}>Install</Button>}
            {a.state === "stopped" && <Button size="sm" variant="subtle" onClick={() => act(a.id, () => post(`/apps/${a.id}/start`))}>Start</Button>}
            {a.state === "running" && <Button size="sm" variant="ghost" onClick={async () => {
              if (a.used_by?.length && !(await ask(`Stop ${a.name}?`, { body: `${a.used_by.join(" and ")} use${a.used_by.length > 1 ? "" : "s"} it, and won't work fully while it's stopped. Starting ${a.used_by.length > 1 ? "them" : "it"} again starts ${a.name} too.`, confirm: "Stop it" }))) return;
              act(a.id, () => post(`/apps/${a.id}/stop`));
            }}>Stop</Button>}
            {(a.state === "stopped" || a.state === "failed") && <Button size="sm" variant="ghost"
              onClick={async () => { if (await ask(`Remove ${a.name}?`, { body: "Its data is kept and comes back if you install it again.", confirm: "Remove", danger: true })) act(a.id, () => del(`/apps/${a.id}`)); }}>Remove</Button>}
          </div>
          {a.state !== "not_installed" && <NetworkSetting a={a} onChanged={() => apps.refresh()} />}
        </Card>
      ),
    })),
    ...CODE_TOOLS.map((t): Entry => ({ key: t.id, cat: "code", on: false, text: `${t.name} ${t.blurb} code editor`, node: <CodeToolTile key={t.id} t={t} base={base} model={model} /> })),
  ];
  const q = query.trim().toLowerCase();
  const shown = entries.filter((e) => (cat === "all" || e.cat === cat) && (!q || e.text.toLowerCase().includes(q)));
  const onBox = cat === "all" && !q ? entries.filter((e) => e.on) : [];
  const about = CATS.find((c) => c.id === cat)?.about;

  return (
    <Page title="Connect" subtitle="Popular open-source tools that run on the models on this box: chat, agents, research, builders and coding tools. And the box as a private OpenAI-compatible API.">
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

      <div className="section-title"><Eyebrow>Your box as an API</Eyebrow></div>
      <div className="grid grid-2" style={{ alignItems: "start" }}>
        <Card>
          <Typography variant="copy-sm" className="muted">Any app that speaks the OpenAI API works with the box: point it at this address with an app key. Requests follow whatever model is serving. From other computers the address is encrypted with the box's own certificate; if an app refuses it, trust the certificate in that app, or allow unencrypted access in Settings → Security and activity.</Typography>
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
    messages=[{"role": "user", "content": "Hello from TT Station"}],
)
print(reply.choices[0].message.content)`}</CodeBlock>
          <CodeBlock lang={shellscript} copyOnHover>{`curl ${base}/chat/completions \\
  -H "Authorization: Bearer ttq-…" -H "Content-Type: application/json" \\
  -d '{"model": "${model}", "messages": [{"role": "user", "content": "Hi"}]}'`}</CodeBlock>
          <Typography variant="copy-xs" className="muted">In VS Code (Continue, Cline) or Cursor: choose "OpenAI-compatible", paste the address and a key.</Typography>
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
