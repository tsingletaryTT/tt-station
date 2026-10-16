// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Chat with the box's model. Answers stream in as Markdown; the assistant can
// read the box (status, models, logs, documents, the web) and asks before it
// changes anything (start or stop a model, reset chips, switch modes).
// Conversations are kept on this computer.

import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { externalLinks } from "../links";
import { Button } from "@tenstorrent/vesper/button";
import { IconButton } from "@tenstorrent/vesper/icon-button";
import { Icon } from "@tenstorrent/vesper/icons";
import { Switch } from "@tenstorrent/vesper/switch";
import { Typography } from "@tenstorrent/vesper/typography";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { type ActionReport, ApiError, clientKind, fetchBlob, get, type Mode, post, runConfirmed, runOnClient, type Serve, short, streamChat, upsertAction } from "../api";
import { toastError, toastOk, usePoll, useToriShortcut } from "../ui";
import { type AskHooks, VoiceMode } from "../VoiceMode";
import { ask, askText } from "../dialog";
import { frameBatcher } from "../render";
import { useFullHeight } from "../layout";
import { JumpToLatest, useStickToBottom } from "../stickToBottom";

interface Message {
  role: "user" | "assistant";
  content: string;
  reasoning?: string;
  actions?: ActionReport[];
  model?: string;
  error?: string;
}
interface Conversation { id: string; title: string; updated: number; messages: Message[] }

const STORE = "tt.conversations";
const load = (): Conversation[] => { try { return JSON.parse(localStorage.getItem(STORE) ?? "[]"); } catch { return []; } };
const save = (c: Conversation[]) => localStorage.setItem(STORE, JSON.stringify(c.slice(0, 100)));
const NO_MESSAGES: Message[] = [];
const newId = () => Math.random().toString(36).slice(2, 10);

const STARTERS = [
  "What's my box doing right now?",
  "Which models can I run, and which is best for coding?",
  "Summarize my latest transcript.",
  "Make an image of a forest at dawn, in teal and mint.",
];

export function Chat() {
  const serve = usePoll(() => get<Serve>("/serve"), 5000);
  const [convs, setConvs] = useState<Conversation[]>(load);
  const [current, setCurrent] = useState<string | null>(convs[0]?.id ?? null);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [voice, setVoice] = useState(() => new URLSearchParams(location.search).has("voice"));
  const toriKeys = useToriShortcut();
  const [think, setThink] = useState(() => localStorage.getItem("tt.think") === "1");
  const abort = useRef<AbortController | null>(null);
  const input = useRef<HTMLTextAreaElement>(null);

  // Where Tori thinks: the chips' chat model, or the CPU when the chips are busy.
  const brain = usePoll(() => get<{ where: "chips" | "cpu" | null; model: string | null }>("/assistant/brain"), 6000);
  const conv = convs.find((c) => c.id === current) ?? null;
  const messages = conv?.messages ?? NO_MESSAGES;
  const canChat = !!brain.data?.where;
  useFullHeight();
  // Only the messages scroll: kept at the newest while you're there, left alone while you read back.
  const follow = useStickToBottom(messages, current);

  // Saved when an answer is done, not at every streamed token (each save writes
  // every conversation).
  useEffect(() => { if (!busy) save(convs); }, [convs, busy]);
  useEffect(() => {
    const open = () => setVoice(true);
    window.addEventListener("tt:voice", open);
    return () => window.removeEventListener("tt:voice", open);
  }, []);
  useEffect(() => { localStorage.setItem("tt.think", think ? "1" : "0"); }, [think]);
  // preventScroll: focusing the composer must not scroll anything to bring it into view.
  useEffect(() => { input.current?.focus({ preventScroll: true }); }, [current]);

  // Re-sorted only when a conversation's time changes, not at every streamed token.
  const update = (id: string, fn: (c: Conversation) => Conversation) =>
    setConvs((all) => {
      let moved = false;
      const next = all.map((c) => {
        if (c.id !== id) return c;
        const n = fn(c);
        moved = n.updated !== c.updated;
        return n;
      });
      return moved ? next.sort((a, b) => b.updated - a.updated) : next;
    });

  const send = async (text: string, hooks?: AskHooks) => {
    const prompt = text.trim();
    if (!prompt || !canChat) return;
    if (busy) {
      if (!hooks) return;
      abort.current?.abort();
    }
    let id = current;
    if (!id) {
      id = newId();
      const c: Conversation = { id, title: prompt.slice(0, 60), updated: Date.now(), messages: [] };
      setConvs((all) => [c, ...all]);
      setCurrent(id);
    }
    const history = [...messages, { role: "user" as const, content: prompt }];
    update(id, (c) => ({ ...c, updated: Date.now(), messages: [...history, { role: "assistant", content: "" }] }));
    setDraft("");
    follow.stick();
    setBusy(true);
    const ctrl = new AbortController();
    abort.current = ctrl;
    const patchLast = (fn: (m: Message) => Message) =>
      update(id!, (c) => ({ ...c, messages: c.messages.map((m, i) => (i === c.messages.length - 1 ? fn(m) : m)) }));
    let said = "";
    // Streamed text and thinking go into the message once per frame, not per token.
    let streamed = "";
    let thought = "";
    const batch = frameBatcher(() => {
      if (!streamed && !thought) return;
      const [t, r] = [streamed, thought];
      streamed = "";
      thought = "";
      patchLast((m) => ({ ...m, content: m.content + t, reasoning: r ? (m.reasoning ?? "") + r : m.reasoning }));
    });
    try {
      const turns = history.map((m) => ({ role: m.role, content: m.content })).slice(-24);
      // Spoken answers are shorter: the model is told so.
      const client = await clientKind();
      for await (const e of streamChat(turns, { think: hooks ? false : think, detailed: !hooks, client }, ctrl.signal)) {
        if (e.type === "action" && e.action.status === "on_client") void runOnClient(e.action);
        if (e.type === "delta") { said += e.text; hooks?.onText(said); }
        else if (e.type === "reset") { said = ""; hooks?.onReset?.(); }
        else if (e.type === "done") said = e.reply;
        if (e.type === "error") hooks?.onError(e.error);
        if (e.type === "delta") { streamed += e.text; batch.poke(); continue; }
        if (e.type === "reasoning") { thought += e.text; batch.poke(); continue; }
        batch.now();
        if (e.type === "reset") patchLast((m) => ({ ...m, content: "" }));
        else if (e.type === "action") patchLast((m) => ({ ...m, actions: upsertAction(m.actions, e.action) }));
        else if (e.type === "done") patchLast((m) => ({ ...m, content: e.reply, model: e.model }));
        else if (e.type === "error") patchLast((m) => ({ ...m, error: e.error }));
      }
      batch.now();
      hooks?.onDone(said);
    } catch (e) {
      // Superseded by a newer question (voice): its answer is the last message now.
      if (abort.current !== ctrl) { streamed = ""; thought = ""; }
      batch.now();
      if ((e as Error).name !== "AbortError") {
        patchLast((m) => ({ ...m, error: (e as Error).message }));
        hooks?.onError((e as Error).message);
      }
    } finally {
      if (abort.current === ctrl) {
        setBusy(false);
        abort.current = null;
      }
    }
  };

  const confirm = async (action: ActionReport, approve: boolean) => {
    try {
      const r = await post<{ message?: string; status?: string; run?: { tool: string; args?: Record<string, unknown> } }>("/assistant/confirm", { action_id: action.id, approve });
      const note = (await runConfirmed(r)) ?? (r as { summary?: string })?.summary ?? r?.message ?? (approve ? "Done." : "Cancelled.");
      if (!conv) return;
      update(conv.id, (c) => ({
        ...c,
        messages: c.messages.map((m) => ({
          ...m,
          actions: m.actions?.map((a) => (a.id === action.id ? { ...a, status: "done" as const, summary: note } : a)),
        })),
      }));
    } catch (e) {
      toastError(e instanceof ApiError && e.status === 404 ? new Error("That request expired. Ask again.") : e);
    }
  };

  // Stable, so finished answers (memoized) don't re-render while another streams.
  const confirmRef = useRef(confirm);
  confirmRef.current = confirm;
  const onConfirm = useCallback((a: ActionReport, approve: boolean) => confirmRef.current(a, approve), []);

  const remove = (id: string) => {
    setConvs((all) => all.filter((c) => c.id !== id));
    if (current === id) setCurrent(null);
  };

  const rename = async (id: string, current: string) => {
    const t = await askText("Rename this conversation", current);
    if (t && t.trim() && t.trim() !== current) update(id, (c) => ({ ...c, title: t.trim() }));
  };

  return (
    <div className="chat">
      <aside className="chat-list">
        <Button size="sm" variant="tertiary" iconLeft={<Icon kind="add" />} onClick={() => setCurrent(null)}>New chat</Button>
        <div style={{ height: 10 }} />
        {convs.map((c) => (
          <button key={c.id} className={c.id === current ? "on" : ""} onClick={() => setCurrent(c.id)}
            onContextMenu={async (e) => { e.preventDefault(); if (await ask(`Delete “${c.title}”?`, { body: "The conversation is removed from this computer.", confirm: "Delete", danger: true })) remove(c.id); }}
            title={c.title}>
            <span className="chat-list-title" onDoubleClick={() => void rename(c.id, c.title)}>{c.title}</span>
            <IconButton icon={<Icon kind="document" />} size="xs" variant="ghost" title="Rename conversation" aria-label={`Rename conversation ${c.title}`} onClick={() => void rename(c.id, c.title)} className="chat-rename" />
          </button>
        ))}
        {convs.length === 0 && <span className="muted" style={{ padding: 10, fontSize: 13 }}>Your conversations stay on this computer.</span>}
      </aside>

      <section className="chat-main">
        <div className="chat-head">
          <div className="row">
            <Typography variant="heading-xs">{conv?.title ?? "New chat"}</Typography>
          </div>
          <div className="row">
            <span className="muted mono" style={{ fontSize: 12 }}>
              {brain.data?.where === "chips" ? `on the chips · ${short(brain.data.model)}` : brain.data?.where === "cpu" ? `on the CPU · ${brain.data.model}` : serve.data?.phase === "starting" ? "Model loading…" : "No chat model"}
            </span>
            <Switch size="sm" label="Think first" checked={think} onChange={(e) => setThink(e.target.checked)} />
          </div>
        </div>

        <div className="scroll-pane">
          <div className="chat-scroll" ref={follow.ref} onScroll={follow.onScroll} data-testid="chat-scroll">
            <div className="chat-thread">
              {!canChat && serve.data && (
                <Admonition variant="info" subtle cta={serve.data.phase === "starting" ? undefined : { children: "Switch to chat", onClick: async () => {
                  try { await post("/modes/chat", {}); serve.refresh(); } catch (e) { toastError(e); }
                } }}>
                  {serve.data.phase === "starting" ? "A model is loading. Chat opens as soon as it's ready."
                    : serve.data.phase === "serving" ? `The box is serving ${short(serve.data.served_id ?? serve.data.model)}, which doesn't chat.`
                    : "Nothing is serving. Switch the box to Chat to talk to it."}
                </Admonition>
              )}
              {(brain.data?.where === "cpu" || (serve.data?.phase === "starting" && brain.data?.where !== "chips")) && (
                <LoadOnChips serve={serve.data} cpuModel={brain.data?.model ?? null} onStarted={() => { serve.refresh(); brain.refresh(); }} />
              )}
              {messages.length === 0 && canChat && <Welcome onPick={(p) => send(p)} />}
              {messages.map((m, i) => m.role === "user"
                ? <div key={i} className="msg-user">{m.content}</div>
                : <AssistantMessage key={i} m={m} streaming={busy && i === messages.length - 1} onConfirm={onConfirm} />)}
            </div>
          </div>
          <JumpToLatest show={follow.unseen} onClick={follow.jump} />
        </div>

        <div className="composer-wrap">
          <div className="composer">
            <textarea ref={input} rows={1} value={draft} placeholder={canChat ? "Ask your box anything" : "Chat needs a chat model"}
              disabled={!canChat}
              onChange={(e) => {
                setDraft(e.target.value);
                e.target.style.height = "auto";
                e.target.style.height = `${Math.min(220, e.target.scrollHeight)}px`;
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                  e.preventDefault();
                  send(draft);
                }
              }} />
            {!busy && <IconButton icon={<Icon kind="microphone" />} variant="ghost" aria-label="Voice mode"
              title="Voice mode: talk to the box" onClick={() => setVoice(true)} />}
            {busy
              ? <IconButton icon={<Icon kind="close" />} variant="subtle" aria-label="Stop" onClick={() => abort.current?.abort()} />
              : <IconButton icon={<Icon kind="arrow-up" />} aria-label="Send" disabled={!draft.trim() || !canChat} onClick={() => send(draft)} />}
          </div>
          <div className="composer-hint">
            <span>Runs on {serve.data ? "your QuietBox" : "the box"}: nothing leaves your network.</span>
            <span><span className="kbd">Enter</span> send · <span className="kbd">Shift Enter</span> new line · <span className="kbd">{toriKeys}</span> Tori, from anywhere</span>
          </div>
        </div>
        {voice && <VoiceMode canChat={canChat} onClose={() => { setVoice(false); history.replaceState(null, "", "/app/chat"); }} ask={(t, h) => void send(t, h)} />}
      </section>
    </div>
  );
}

/** Chat is on the CPU (the chips are busy with something else): offer the chips. */
function LoadOnChips({ serve, cpuModel, onStarted }: { serve: Serve | null; cpuModel: string | null; onStarted: () => void }) {
  const modes = usePoll(() => get<{ modes: Mode[] }>("/modes"), 15000);
  const [busy, setBusy] = useState(false);
  const [hidden, setHidden] = useState(false);
  const chat = modes.data?.modes.find((m) => m.id === "chat");
  if (hidden) return null;
  if (serve?.phase === "starting") {
    return serve.stalled_s ? (
      <Admonition variant="warning" subtle>
        {short(serve.model)} has shown no progress for {Math.round(serve.stalled_s / 60)} minutes, so its start looks stuck. Stop it in Models (its chips are reset) and try again, or start another model. Chat keeps working on the CPU meanwhile.
      </Admonition>
    ) : (
      <Admonition variant="info" subtle>
        Loading {short(serve.model)} onto the chips{serve.eta_s ? `, about ${Math.max(1, Math.round(serve.eta_s / 60))} min` : ""}. Chat keeps working on the CPU meanwhile and moves over by itself.
      </Admonition>
    );
  }
  const name = chat ? short(chat.models[0]) : "a chat model";
  const busyWith = serve?.phase === "serving" ? short(serve.served_id ?? serve.model) : null;
  const minutes = chat?.eta_s ? Math.max(1, Math.round(chat.eta_s / 60)) : null;
  return (
    <div className="chips-prompt">
      <Icon kind="blackhole" className="chips-prompt-icon" />
      <div className="stack" style={{ gap: 4, flex: "1 1 280px", minWidth: 0 }}>
        <b>Chat is on the CPU right now</b>
        <span className="muted">
          It's using {cpuModel ?? "a small CPU model"}, which is slower{busyWith ? ` because the chips are serving ${busyWith}` : ""}.
          {chat && !chat.ready ? ` ${name} needs downloading first (Models).` : ` Load ${name} on the chips for faster, better answers${minutes ? ` (about ${minutes} min)` : ""}${busyWith ? `; this stops ${busyWith}` : ""}.`}
        </span>
      </div>
      <div className="row" style={{ gap: 6, flex: "none", marginLeft: "auto" }}>
        <Button size="sm" variant="ghost" onClick={() => setHidden(true)}>Not now</Button>
        {chat?.ready && (
          <Button size="sm" disabled={busy} onClick={async () => {
            setBusy(true);
            try { await post("/modes/chat", {}); toastOk(`Loading ${name} onto the chips${minutes ? `, about ${minutes} min` : ""}.`); onStarted(); }
            catch (e) { toastError(e); } finally { setBusy(false); }
          }}>{busy ? "Loading…" : `Load ${name}`}</Button>
        )}
      </div>
    </div>
  );
}

function Welcome({ onPick }: { onPick: (p: string) => void }) {
  return (
    <div className="stack" style={{ alignItems: "center", textAlign: "center", padding: "48px 0", gap: 18 }}>
      <Icon kind="tenstorrent" style={{ width: 40, height: 40, color: "var(--vesper-text-accent)" }} />
      <Typography variant="heading-md">What can I do for you?</Typography>
      <div className="grid grid-2" style={{ width: "100%", maxWidth: 640, gap: 10 }}>
        {STARTERS.map((s) => (
          <button key={s} className="card card-click" style={{ textAlign: "left", padding: 14, font: "inherit", color: "inherit" }} onClick={() => onPick(s)}>
            {s}
          </button>
        ))}
      </div>
    </div>
  );
}

/** One answer. Memoized: while one streams, the finished ones above it keep their
 *  objects (only the last message changes) and skip rendering their Markdown. */
const AssistantMessage = memo(function AssistantMessage({ m, streaming, onConfirm }: {
  m: Message; streaming: boolean; onConfirm: (a: ActionReport, approve: boolean) => void;
}) {
  const [showThinking, setShowThinking] = useState(false);
  const pending = useMemo(() => (m.actions ?? []).filter((a) => a.status === "needs_confirmation"), [m.actions]);
  return (
    <div className="msg-assistant">
      <Icon kind="tenstorrent" className="avatar" />
      <div className="msg-body">
        {m.reasoning && (
          <div>
            <button className="eyebrow" style={{ background: "none", border: 0, cursor: "pointer", padding: 0, marginBottom: 6 }}
              onClick={() => setShowThinking((v) => !v)}>
              {showThinking ? "▾" : "▸"} Thinking
            </button>
            {showThinking && <div className="thinking">{m.reasoning}</div>}
          </div>
        )}
        {(m.actions ?? []).filter((a) => a.status !== "needs_confirmation").map((a) => (
          <span key={a.id} className="action"><Icon kind={a.status === "error" ? "warning" : a.status === "on_client" ? "spinner" : "checkmark"} width={12} height={12} />{a.summary}</span>
        ))}
        {(m.actions ?? []).flatMap((a) => a.images ?? []).map((id) => <CreationImage key={id} id={id} />)}
        <div className="markdown selectable">
          {m.content ? <Markdown remarkPlugins={[remarkGfm]} components={externalLinks}>{m.content}</Markdown>
            : streaming ? <span className="muted cursor">{m.reasoning ? "Thinking" : ""}</span> : null}
          {streaming && m.content && <span className="cursor" />}
        </div>
        {pending.map((a) => (
          <Admonition key={a.id} variant="warning" subtle size="sm" style={{ marginTop: 12 }}>
            <div className="stack" style={{ gap: 10 }}>
              <span>{a.summary}</span>
              <div className="row">
                <Button size="sm" onClick={() => onConfirm(a, true)}>Do it</Button>
                <Button size="sm" variant="ghost" onClick={() => onConfirm(a, false)}>Cancel</Button>
              </div>
            </div>
          </Admonition>
        ))}
        {m.error && <Typography variant="copy-sm" style={{ color: "var(--vesper-text-error)", marginTop: 8 }}>{m.error}</Typography>}
        {m.model && !streaming && <div className="muted mono" style={{ fontSize: 11, marginTop: 8 }}>{short(m.model)}</div>}
      </div>
    </div>
  );
});

/** A picture the assistant drew (fetched with the token, so it can't be a plain <img src>). */
function CreationImage({ id }: { id: string }) {
  const [url, setUrl] = useState<string | null>(null);
  useEffect(() => {
    let revoke: string | null = null;
    fetchBlob(`/creations/${id}`).then((blob) => {
      revoke = URL.createObjectURL(blob);
      setUrl(revoke);
    }).catch(() => {});
    return () => { if (revoke) URL.revokeObjectURL(revoke); };
  }, [id]);
  return url ? <img src={url} alt="" style={{ maxWidth: 420, borderRadius: 12, border: "1px solid var(--vesper-border-secondary)", margin: "4px 0 12px", display: "block" }} />
    : null;
}
