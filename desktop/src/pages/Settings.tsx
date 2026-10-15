// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Settings: the desktop app, power, the household, schedules, energy, remote
// access and the box's software.

import { useEffect, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Switch } from "@tenstorrent/vesper/switch";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Select } from "@tenstorrent/vesper/select";
import { Badge } from "@tenstorrent/vesper/badge";
import { Typography } from "@tenstorrent/vesper/typography";
import { useModal, Modal } from "@tenstorrent/vesper/modal";
import { del, delWith, get, type Mode, patch, type Person, post, put, type Schedule } from "../api";
import { ThemeSwitcher } from "@tenstorrent/vesper/theme-switcher";
import { Card, copyText, Eyebrow, Page, readable, Spark, toastError, toastOk, usePoll } from "../ui";
import { Speaker, type VoiceStatus } from "../voice";
import { ask, askText } from "../dialog";

type Section = "general" | "tori" | "models" | "power" | "people" | "phone" | "about";
const SECTIONS: { id: Section; label: string; hint: string }[] = [
  { id: "general", label: "General", hint: "This app, start at login, keys" },
  { id: "tori", label: "Tori and voice", hint: "Shortcut, voice, speed" },
  { id: "models", label: "Models", hint: "Which models the box uses" },
  { id: "power", label: "Power and energy", hint: "Power, schedules, energy use" },
  { id: "people", label: "People", hint: "You and your household" },
  { id: "phone", label: "Pairing and security", hint: "Phones, other computers, SSH keys, encryption" },
  { id: "about", label: "About", hint: "Software on the box" },
];

export function Settings() {
  const [section, setSection] = useState<Section>(() => {
    try {
      const saved = localStorage.getItem("tt.settings.section") as Section | null;
      return saved && SECTIONS.some((x) => x.id === saved) ? saved : "general";
    } catch { return "general"; }
  });
  useEffect(() => { try { localStorage.setItem("tt.settings.section", section); } catch { /* private */ } }, [section]);
  const current = SECTIONS.find((x) => x.id === section) ?? SECTIONS[0];
  return (
    <Page title="Settings">
      <div className="settings">
        <nav className="settings-nav" aria-label="Settings sections">
          {SECTIONS.map((x) => (
            <button key={x.id} className={x.id === section ? "on" : ""} aria-current={x.id === section ? "page" : undefined} onClick={() => setSection(x.id)}>
              <span>{x.label}</span>
              <span className="muted">{x.hint}</span>
            </button>
          ))}
        </nav>
        <div className="settings-body" key={section}>
          <Typography variant="heading-sm">{current.label}</Typography>
          {section === "general" && <Desktop />}
          {section === "tori" && <><ToriCard /><Voice /></>}
          {section === "models" && <ModelPrefs />}
          {section === "power" && <><Power /><IdleCard /><Schedules /><Energy /></>}
          {section === "people" && <Household />}
          {section === "phone" && <Phone />}
          {section === "about" && <><Updates /><Software /></>}
        </div>
      </div>
    </Page>
  );
}

function ToriCard() {
  return (
    <Card>
      <Eyebrow>Tori</Eyebrow>
      <Typography variant="copy-sm" className="muted">Tori is the assistant you summon over whatever you're doing: she listens straight away, or type. Hold Space to talk, Esc to put her away.</Typography>
      <Shortcuts />
    </Card>
  );
}

function Desktop() {
  const [atLogin, setAtLogin] = useState<boolean | null>(null);
  useEffect(() => { window.ttDesktop?.startAtLogin().then(setAtLogin); }, []);
  return (
    <Card>
      <Eyebrow>This app</Eyebrow>
      <div className="row-between wrap" style={{ gap: 12 }}>
        <span>Appearance</span>
        <ThemeSwitcher size="sm" />
      </div>
      {atLogin !== null && (
        <Switch label="Open TT Station when I log in (it waits in the tray)" checked={atLogin}
          onChange={async (e) => setAtLogin(await window.ttDesktop!.startAtLogin(e.target.checked))} />
      )}
      <div className="list">
        {[["Ctrl 1 – 8", "Jump between places"], ["Ctrl Enter", "Make (in Studio)"]].map(([k, v]) => (
          <div key={k} className="list-row"><span className="kbd">{k}</span><span className="muted">{v}</span></div>
        ))}
      </div>
      <Typography variant="copy-xs" className="muted">Closing the window keeps TT Station in the tray, so notifications still arrive. Quit from the tray icon.</Typography>
    </Card>
  );
}

/** A key press → a GNOME binding, or null for a lone modifier. */
function bindingOf(e: KeyboardEvent): string | null {
  const code = e.code;
  let key: string | null = null;
  if (/^Key[A-Z]$/.test(code)) key = code.slice(3).toLowerCase();
  else if (/^Digit\d$/.test(code)) key = code.slice(5);
  else if (/^F\d{1,2}$/.test(code)) key = code;
  else key = ({ Space: "space", Enter: "Return", Backquote: "grave", Slash: "slash", Period: "period", Comma: "comma",
    Semicolon: "semicolon", Quote: "apostrophe", BracketLeft: "bracketleft", BracketRight: "bracketright",
    Minus: "minus", Equal: "equal", Backslash: "backslash" } as Record<string, string>)[code] ?? null;
  if (!key) return null;
  return `${e.ctrlKey ? "<Control>" : ""}${e.altKey ? "<Alt>" : ""}${e.shiftKey ? "<Shift>" : ""}${e.metaKey ? "<Super>" : ""}${key}`;
}

/** Tori's shortcut: shown, and changed by pressing the new one. */
function Shortcuts() {
  const [info, setInfo] = useState<{ binding: string; installed: boolean; dropdown: boolean } | null>(null);
  const [capturing, setCapturing] = useState(false);
  useEffect(() => { window.ttDesktop?.toriShortcut?.().then(setInfo).catch(() => setInfo(null)); }, []);
  useEffect(() => {
    if (!capturing) return;
    const key = async (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.key === "Escape") { setCapturing(false); return; }
      const binding = bindingOf(e);
      if (!binding) return; // still holding modifiers
      setCapturing(false);
      try { setInfo(await window.ttDesktop!.toriShortcut(binding)); window.dispatchEvent(new CustomEvent("tt:shortcut", { detail: binding })); toastOk(`Tori now opens with ${readable(binding).join(" + ")}.`); }
      catch (err) { toastError(err); }
    };
    window.addEventListener("keydown", key, true);
    return () => window.removeEventListener("keydown", key, true);
  }, [capturing]);
  if (!info) return null;
  return (
    <div className="stack" style={{ gap: 8 }}>
      <div className="row-between">
        <div>
          <b>Tori's shortcut</b>
          <div className="muted" style={{ fontSize: 12 }}>
            Opens Tori's window from anywhere.
          </div>
        </div>
        <div className="row" style={{ gap: 8 }}>
          {capturing
            ? <span className="kbd shortcut-capture">Press the new shortcut… (Esc to cancel)</span>
            : <span className="row" style={{ gap: 4 }}>{readable(info.binding).map((k) => <span key={k} className="kbd">{k}</span>)}</span>}
          <Button size="sm" variant="subtle" onClick={() => setCapturing((c) => !c)}>{capturing ? "Cancel" : "Change"}</Button>
        </div>
      </div>
      {!info.installed && (
        <Button size="sm" variant="ghost" onClick={async () => { try { await window.ttDesktop!.shortcuts(true); setInfo(await window.ttDesktop!.toriShortcut()); } catch (e) { toastError(e); } }}>
          Turn the shortcut on
        </Button>
      )}
    </div>
  );
}

interface Choice { name: string; ready: boolean; tools: boolean; params_b: number | null }
interface PrefsInfo {
  prefs: { chat?: string; image?: string; video?: string; transcribe?: string; cpu_chat?: string };
  choices: { chat: Choice[]; image: Choice[]; video: Choice[]; speech: Choice[] };
  cpu_chat: string[];
  cpu_default: string;
}

/** Which models the box's modes, Chat and Tori use. */
function ModelPrefs() {
  const info = usePoll(() => get<PrefsInfo>("/preferences/models"), 60000);
  const voice = usePoll(() => get<VoiceStatus>("/voice").catch(() => null), 60000);
  const set = async (slot: string, value: string) => {
    try {
      await put("/preferences/models", { [slot]: value === "auto" ? "" : value });
      info.refresh();
      toastOk(value === "auto" ? "Back to automatic." : `Saved. The box uses ${value} from now on.`);
    } catch (e) { toastError(e); }
  };
  const d = info.data;
  if (!d) return null;
  const options = (list: Choice[], chat = false) => [
    { value: "auto", label: "Automatic" },
    ...list.map((c) => ({
      value: c.name,
      label: `${c.name}${c.params_b ? ` · ${Math.round(c.params_b)}B` : ""}${c.ready ? "" : " · needs download"}${chat && !c.tools ? " · can't use tools" : ""}`,
    })),
  ];
  const row = (label: string, hint: string, slot: keyof PrefsInfo["prefs"], list: Choice[], chat = false) => (
    <div className="pref-row" key={slot}>
      <div><b>{label}</b><div className="muted">{hint}</div></div>
      <Select size="sm" value={d.prefs[slot] ?? "auto"} onValueChange={(v) => v && set(slot, v)} options={options(list, chat)} aria-label={label} />
    </div>
  );
  return (
    <Card>
      <Eyebrow>Models</Eyebrow>
      <Typography variant="copy-sm" className="muted">What the box loads when you switch modes, and what Tori and Chat think with.</Typography>
      <div className="pref-grid">
        {row("Chat", "Chat mode, Chat and Tori", "chat", d.choices.chat, true)}
        {row("Images", "Images mode and Studio", "image", d.choices.image)}
        {row("Video", "Video mode", "video", d.choices.video)}
        {row("Speech to text", "Transcribe mode (on the chips)", "transcribe", d.choices.speech)}
        <div className="pref-row">
          <div><b>While the chips are busy</b><div className="muted">Tori and Chat on the CPU</div></div>
          <Select size="sm" value={d.prefs.cpu_chat ?? "auto"} onValueChange={(v) => v && set("cpu_chat", v)} aria-label="CPU model"
            options={[{ value: "auto", label: `Automatic (${d.cpu_default})` }, ...d.cpu_chat.map((m) => ({ value: m, label: m }))]} />
        </div>
        {voice.data?.state === "ready" && (
          <div className="pref-row">
            <div><b>Voice</b><div className="muted">How Tori and voice mode sound</div></div>
            <Select size="sm" value={voice.data.voice} aria-label="Voice"
              onValueChange={async (v) => { if (!v) return; try { await put("/voice", { voice: v }); voice.refresh(); preview ??= new Speaker(); preview.voice = v; preview.begin(); preview.finish("Hi, this is how I'll sound."); } catch (e) { toastError(e); } }}
              options={voice.data.voices.filter((x) => x.english).map((x) => ({ value: x.id, label: `${x.name} · ${x.accent} ${x.gender}${x.favourite ? " ★" : ""}` }))} />
          </div>
        )}
      </div>
      <Typography variant="copy-xs" className="muted">Changes apply the next time a mode starts. Download models in Models.</Typography>
    </Card>
  );
}

let preview: Speaker | null = null;

function Voice() {
  const status = usePoll(() => get<VoiceStatus>("/voice"), 3000);
  const v = status.data;
  const [speed, setSpeed] = useState<number | null>(null);
  const say = (voice: string) => {
    preview ??= new Speaker();
    preview.voice = voice;
    preview.begin();
    preview.finish("Hi, I'm Tori. Ask me anything, and I'll answer right here, on your QuietBox.");
  };
  const choose = async (body: { voice?: string; speed?: number }) => {
    try { await put("/voice", body); status.refresh(); } catch (e) { toastError(e); }
  };
  return (
    <Card>
      <div className="row-between">
        <Eyebrow>Voice</Eyebrow>
        {v && <Badge size="sm" subtle variant={v.state === "ready" ? "mint" : v.state === "failed" ? "danger" : "contrast"}>
          {v.state === "ready" ? "Ready" : v.state === "installing" ? v.step ?? "Setting up" : v.state === "failed" ? "Failed" : v.state === "not_installed" ? "Not set up" : "Starting"}
        </Badge>}
      </div>
      <Typography variant="copy-sm" className="muted">
        Kokoro speaks and Whisper listens, both on the box's CPU, so voice works while the chips run your model. Used by voice mode here and on your phone.
      </Typography>
      {v?.error && <Typography variant="copy-sm" style={{ color: "var(--vesper-text-error)" }}>{v.error}</Typography>}
      {v && (v.state === "not_installed" || v.state === "failed" || v.state === "stopped") && (
        <div><Button size="sm" onClick={async () => { try { await post("/voice/setup"); status.refresh(); } catch (e) { toastError(e); } }}>
          {v.state === "not_installed" ? "Set up (about 7 GB, one time)" : "Start it"}</Button></div>
      )}
      {v?.state === "ready" && (
        <>
          <div className="list" style={{ maxHeight: 260, overflow: "auto" }}>
            {v.voices.filter((x) => x.english).map((x) => (
              <div key={x.id} className="list-row">
                <div className="grow">
                  <b>{x.name}</b>{x.favourite && <span className="muted"> ★</span>}
                  <div className="muted" style={{ fontSize: 12 }}>{x.accent} {x.gender}</div>
                </div>
                <Button size="xs" variant="ghost" onClick={() => say(x.id)}>Play</Button>
                {v.voice === x.id ? <Badge size="sm" variant="mint" subtle>In use</Badge>
                  : <Button size="xs" variant="subtle" onClick={() => choose({ voice: x.id })}>Use</Button>}
              </div>
            ))}
          </div>
          <label className="row" style={{ gap: 12, fontSize: 13 }}>
            <span className="muted">Speed</span>
            <input type="range" min={0.7} max={1.4} step={0.05} value={speed ?? v.speed} style={{ flex: 1 }}
              onChange={(e) => setSpeed(Number(e.target.value))}
              onPointerUp={() => speed != null && choose({ speed })} onKeyUp={() => speed != null && choose({ speed })} />
            <span className="mono" style={{ width: 40 }}>{(speed ?? v.speed).toFixed(2)}×</span>
          </label>
        </>
      )}
    </Card>
  );
}

function Power() {
  const info = usePoll(() => get<{ can_power_off: string; can_reboot: string; can_suspend: string; headless_power: boolean }>("/power/info"), 60000);
  const modal = useModal();
  const [action, setAction] = useState<{ id: string; label: string; detail: string } | null>(null);
  const ask = (id: string, label: string, detail: string) => { setAction({ id, label, detail }); modal.open(); };
  return (
    <Card>
      <Eyebrow>Power</Eyebrow>
      <div className="row wrap">
        <Button size="sm" variant="subtle" disabled={info.data?.can_suspend !== "yes"} onClick={() => ask("suspend", "Sleep", "The box sleeps; wake it from the phone app or its power button.")}>Sleep</Button>
        <Button size="sm" variant="subtle" disabled={info.data?.can_reboot !== "yes"} onClick={() => ask("reboot", "Restart", "The box restarts. TT Station reconnects in a minute or two.")}>Restart</Button>
        <Button size="sm" variant="ghost" disabled={info.data?.can_power_off !== "yes"} onClick={() => ask("shutdown", "Shut down", "The box turns off.")}>Shut down</Button>
        <Button size="sm" variant="ghost" onClick={() => ask("reset_chips", "Reset the chips", "Stops serving and resets the four Blackhole chips.")}>Reset chips</Button>
      </div>
      <Modal ref={modal.ref} title={`${action?.label ?? ""}?`} description={action?.detail ?? ""}
        buttons={[{ children: "Cancel", onClick: modal.close }, { children: action?.label ?? "OK", variant: "danger", onClick: async () => {
          modal.close();
          try { await post("/power", { action: action!.id }); toastOk(`${action!.label}…`); } catch (e) { toastError(e); }
        } }]} />
    </Card>
  );
}

function Household() {
  const people = usePoll(() => get<{ people: Person[]; me: string | null }>("/people"), 30000);
  const [name, setName] = useState("");
  const run = async (fn: () => Promise<{ people: Person[]; me: string | null }>) => {
    try { people.setData(await fn()); } catch (e) { toastError(e); }
  };
  const list = people.data;
  return (
    <Card>
      <Eyebrow>You and your household</Eyebrow>
      <Typography variant="copy-sm" className="muted">Each person gets their own memories with the assistant. A person is just a name on the box.</Typography>
      {list && (
        <Select value={list.me ?? "anyone"} onValueChange={(v) => run(() => put("/people/me", { person: v === "anyone" ? null : v }))}
          options={[{ label: "This computer is: anyone (shared)", value: "anyone" }, ...list.people.map((p) => ({ label: `This computer is: ${p.name}`, value: p.id }))]} />
      )}
      <div className="list">
        {(list?.people ?? []).map((p) => (
          <div key={p.id} className="list-row">
            <span className="grow">{p.name}{list?.me === p.id && <Badge size="sm" subtle variant="mint" style={{ marginLeft: 8 }}>you</Badge>}</span>
            <Button size="xs" variant="ghost" onClick={async () => { const n = await askText(`Rename ${p.name}`, p.name); if (n) run(() => patch(`/people/${p.id}`, { name: n })); }}>Rename</Button>
            <Button size="xs" variant="ghost" onClick={async () => { if (await ask(`Remove ${p.name}?`, { body: "Their pairing stops working and Tori forgets what she remembered about them.", confirm: "Remove", danger: true })) run(() => del(`/people/${p.id}`)); }}>Remove</Button>
          </div>
        ))}
      </div>
      <div className="row">
        <div className="grow"><TextInput size="sm" value={name} onChange={(e) => setName(e.target.value)} placeholder="Add someone" /></div>
        <Button size="sm" variant="subtle" disabled={!name.trim()} onClick={() => { const n = name.trim(); setName(""); run(() => post("/people", { name: n, me: !list?.me })); }}>Add</Button>
      </div>
    </Card>
  );
}

const DAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

function Schedules() {
  const schedules = usePoll(() => get<{ schedules: Schedule[] }>("/schedules"), 30000);
  const modes = usePoll(() => get<{ modes: Mode[] }>("/modes"), 60000);
  const [mode, setMode] = useState("chat");
  const [time, setTime] = useState("08:00");
  const [days, setDays] = useState<number[]>([1, 2, 3, 4, 5]);
  const title = (id: string) => modes.data?.modes.find((m) => m.id === id)?.title ?? id;
  const set = async (fn: () => Promise<{ schedules: Schedule[] }>) => { try { schedules.setData(await fn()); } catch (e) { toastError(e); } };
  return (
    <Card>
      <Eyebrow>Schedules</Eyebrow>
      <Typography variant="copy-sm" className="muted">Switch the box's mode at set times. Never interrupts a silicon check, a bring-up or a transcription.</Typography>
      <div className="list">
        {(schedules.data?.schedules ?? []).map((s) => (
          <div key={s.id} className="list-row">
            <span className="mono" style={{ fontSize: 18 }}>{s.time}</span>
            <div className="grow"><b>{title(s.mode)}</b><div className="muted" style={{ fontSize: 12 }}>{s.days.map((d) => DAYS[d]).join(" ")}</div></div>
            <Switch size="sm" aria-label="Enabled" checked={s.enabled} onChange={(e) => set(() => patch(`/schedules/${s.id}`, { enabled: e.target.checked }))} />
            <Button size="xs" variant="ghost" onClick={() => set(() => del(`/schedules/${s.id}`))}>Delete</Button>
          </div>
        ))}
      </div>
      <div className="row wrap">
        <Select size="sm" value={mode} onValueChange={(v) => setMode(v ?? "chat")} options={(modes.data?.modes ?? []).map((m) => ({ label: m.title, value: m.id }))} />
        <div style={{ width: 120 }}><TextInput size="sm" type="time" value={time} onChange={(e) => setTime(e.target.value)} /></div>
        <div className="row" style={{ gap: 4 }}>
          {DAYS.map((d, i) => (
            <button key={d} className={`kbd`} style={{ cursor: "pointer", background: days.includes(i) ? "var(--vesper-teal-400)" : "transparent", color: "var(--vesper-text-primary)" }}
              onClick={() => setDays((x) => (x.includes(i) ? x.filter((y) => y !== i) : [...x, i].sort()))}>{d[0]}</button>
          ))}
        </div>
        <Button size="sm" variant="subtle" disabled={!days.length} onClick={() => set(() => post("/schedules", { mode, time, days }))}>Add</Button>
      </div>
    </Card>
  );
}

function Energy() {
  const report = usePoll(() => get<{ months: { month: string; energy_kwh: number; tokens: number }[]; days: { day: string; energy_kwh: number }[] }>("/energy"), 60000);
  const [price, setPrice] = useState(() => Number(localStorage.getItem("tt.price") ?? "0.15"));
  useEffect(() => { localStorage.setItem("tt.price", String(price)); }, [price]);
  const month = report.data?.months.at(-1);
  return (
    <Card>
      <Eyebrow>Energy</Eyebrow>
      <div className="row-between">
        <div className="stat"><div className="stat-value">{(month?.energy_kwh ?? 0).toFixed(1)}<span className="stat-unit">kWh this month</span></div>
          <div className="stat-sub">≈ ${((month?.energy_kwh ?? 0) * price).toFixed(2)} at ${price.toFixed(2)}/kWh</div></div>
        <div style={{ width: 110 }}><TextInput size="sm" type="number" value={String(price)} onChange={(e) => setPrice(Number(e.target.value) || 0)} /></div>
      </div>
      <Spark title="Energy per day" axis values={(report.data?.days ?? []).map((d) => d.energy_kwh)} labels={(report.data?.days ?? []).map((d) => d.day)} height={60} format={(v) => `${v.toFixed(1)} kWh`} />
      <span className="muted" style={{ fontSize: 12 }}>Daily kWh over the last month, from the chips' own power readings.</span>
    </Card>
  );
}

interface PairQr { link: string; qr_svg: string | null; expires_in_s: number; host: string; https_port: number; fingerprint: string }
interface Security { lan_http: boolean; https_port: number; tls: { port: number; sha256: string } | null }
interface SshKey { label: string; type: string; added: string | null; raw: string }

/** The certificate fingerprint in groups of four, easier to compare by eye. */
const grouped = (hex: string) => hex.match(/.{1,4}/g)?.join(" ") ?? hex;

/**
 * Pairing an iPhone, securely: the QR holds a one-time code and the box's
 * certificate fingerprint, so the phone talks to the box only over HTTPS
 * pinned to that certificate. The phone makes its own SSH key and sends only
 * the public half; the private key never leaves the phone.
 */
function Phone() {
  const [qr, setQr] = useState<PairQr | null>(null);
  const [left, setLeft] = useState(0);
  const security = usePoll(() => get<Security>("/security"), 30000);
  const ssh = usePoll(() => get<{ keys: SshKey[]; user: string }>("/ssh/info"), 10000);
  useEffect(() => {
    if (!qr) return;
    const t = window.setInterval(() => setLeft((n) => { if (n <= 1) { setQr(null); return 0; } return n - 1; }), 1000);
    return () => window.clearInterval(t);
  }, [qr]);
  const show = async () => {
    try { const r = await post<PairQr>("/pair/qr"); setQr(r); setLeft(r.expires_in_s); } catch (e) { toastError(e); }
  };
  const revoke = async (k: SshKey) => {
    if (!(await ask(`Remove ${k.label}'s SSH key?`, { body: "That device can no longer sign in to the box over SSH. It can be added again by pairing.", confirm: "Remove", danger: true }))) return;
    try { await delWith("/ssh/authorize", { label: k.raw }); toastOk("Removed."); ssh.refresh(); } catch (e) { toastError(e); }
  };
  const setLanHttp = async (on: boolean) => {
    if (on && !(await ask("Allow unencrypted connections from other computers?", {
      body: "Apps on other computers could then use the box's API (/v1) over plain HTTP, readable by anyone on your network. Phones always use encrypted HTTPS either way.",
      confirm: "Allow", danger: true,
    }))) return;
    try { await post("/security", { lan_http: on }); security.refresh(); } catch (e) { toastError(e); }
  };
  const fp = security.data?.tls?.sha256;
  const mm = `${Math.floor(left / 60)}:${String(left % 60).padStart(2, "0")}`;
  return (
    <>
      <Card>
        <Eyebrow>Pair your iPhone</Eyebrow>
        <Typography variant="copy-sm" className="muted">
          Scan with the iPhone's Camera. TT Station on the phone connects to this box only over encrypted HTTPS, pinned to this box's certificate, and creates its own SSH key: just the public half comes here.
        </Typography>
        {qr ? (
          <div className="row" style={{ gap: 16, alignItems: "flex-start" }}>
            {qr.qr_svg && <img src={`data:image/svg+xml;utf8,${encodeURIComponent(qr.qr_svg)}`} alt="Pairing QR code" style={{ width: 200, height: 200, borderRadius: 10 }} />}
            <div className="stack" style={{ gap: 6 }}>
              <span>Works once, for {mm}.</span>
              <span className="muted" style={{ fontSize: 12 }}>The phone must be on the same network ({qr.host}).</span>
              <span className="muted" style={{ fontSize: 12 }}>It checks the box's certificate: <code>{grouped(qr.fingerprint)}…</code></span>
              <div><Button size="sm" variant="ghost" onClick={() => setQr(null)}>Hide</Button></div>
            </div>
          </div>
        ) : <div><Button onClick={show}>Show the pairing code</Button></div>}
      </Card>

      <PairComputer />

      <Card>
        <Eyebrow>SSH keys this box trusts</Eyebrow>
        <div className="list">
          {(ssh.data?.keys ?? []).map((k) => (
            <div key={k.raw} className="list-row">
              <div className="grow"><b>{k.label}</b><div className="muted mono" style={{ fontSize: 12 }}>{k.type}{k.added ? ` · added ${k.added}` : ""}</div></div>
              <Button size="xs" variant="ghost" onClick={() => revoke(k)}>Remove</Button>
            </div>
          ))}
          {ssh.data && ssh.data.keys.length === 0 && <span className="muted" style={{ fontSize: 13 }}>None added through TT Station yet. Pairing a phone adds its key.</span>}
        </div>
      </Card>

      <Card>
        <Eyebrow>Encryption</Eyebrow>
        <div className="list">
          <div className="list-row"><span className="muted grow">Phones</span><span>Encrypted HTTPS only, on port {security.data?.https_port ?? "…"}</span></div>
          {fp && <div className="list-row"><span className="muted grow">This box's certificate</span><span className="mono" style={{ fontSize: 11 }}>{grouped(fp.slice(0, 32))}…</span></div>}
          <div className="list-row"><span className="muted grow">Away from home</span><span>Off: the phone connects on your home network only</span></div>
        </div>
        <div className="row-between">
          <div className="stack" style={{ gap: 2 }}>
            <b style={{ fontSize: 14 }}>Unencrypted API for other computers</b>
            <span className="muted" style={{ fontSize: 12 }}>Only for apps on other computers that can't use the box's certificate. Off is safer.</span>
          </div>
          <Switch checked={!!security.data?.lan_http} onChange={(e) => setLanHttp(e.target.checked)} />
        </div>
      </Card>
    </>
  );
}

function Software() {
  const sw = usePoll(() => get<{ tt?: { current?: string; latest?: string; newer?: boolean }; tools?: { name: string; installed: string | null; golden: string | null; up_to_date: boolean }[];
    system?: { installed_kmd?: string; advice?: string }; firmware_target?: string }>("/software"), 300000);
  const d = sw.data;
  interface CliPlan { mode: string; blocked: boolean; others: string[]; command: string; would_install: string[]; updated?: boolean; version_now?: string | null; message?: string }
  const [updating, setUpdating] = useState(false);
  const [plan, setPlan] = useState<CliPlan | null>(null);
  /** Review what the update changes, ask, update, and say what happened. tt runs fresh each time, so nothing restarts. */
  const updateTt = async () => {
    setUpdating(true); setPlan(null);
    try {
      const p = await post<CliPlan>("/software/cli-update", { dry_run: true });
      if (p.mode === "none") { toastOk(p.message ?? "tt is up to date."); return; }
      if (p.blocked) { setPlan(p); return; }
      const ok = await ask(`Update tt to ${d?.tt?.latest}?`, {
        body: `This installs ${p.mode === "pip" ? p.would_install.map((x) => x.replace(/-(\d)/, " $1")).join(", ") : "the newest tt"} and changes nothing else in the environment${p.mode === "pip" ? " (pip checked first)" : ""}. It takes under a minute. TT Station keeps running, and the next tt command uses the new version.`,
        confirm: "Update",
      });
      if (!ok) return;
      const r = await post<CliPlan>("/software/cli-update");
      if (r.blocked) { setPlan(r); return; }
      toastOk(`tt is now ${r.version_now ?? d?.tt?.latest}.`);
      sw.refresh();
    } catch (e) { toastError(e); } finally { setUpdating(false); }
  };
  return (
    <Card>
      <Eyebrow>Software</Eyebrow>
      <div className="list">
        <div className="list-row"><span className="grow">tt CLI</span><span className="mono muted">{d?.tt?.current ?? "–"}</span>
          {d?.tt?.newer && <Button size="xs" variant="subtle" disabled={updating} onClick={updateTt}>{updating ? "Updating…" : `Update to ${d.tt.latest}`}</Button>}</div>
        {plan?.blocked && (
          <div className="stack" style={{ gap: 6, padding: "8px 0" }}>
            <span style={{ fontSize: 13 }}>Updating tt here would also change <b>{plan.others.map((o) => o.replace(/-\d[\w.]*$/, "")).join(", ")}</b>, which other tools in this shared environment use, so TT Station won't do it for you. To go ahead yourself, run:</span>
            <div className="row" style={{ gap: 8 }}>
              <code className="paste-value grow">{plan.command}</code>
              <Button size="xs" variant="ghost" onClick={() => void copyText(plan.command, "Command copied.")}>Copy</Button>
            </div>
          </div>
        )}
        {(d?.tools ?? []).map((t) => (
          <div key={t.name} className="list-row"><span className="grow">{t.name}</span>
            <span className="mono muted">{t.installed ?? "not installed"}</span>
            {!t.up_to_date && t.golden && <Badge size="sm" subtle variant="info">{t.golden} available</Badge>}</div>
        ))}
        {d?.system?.installed_kmd && <div className="list-row"><span className="grow">Driver (KMD)</span><span className="mono muted">{d.system.installed_kmd}</span></div>}
      </div>
      {d?.system?.advice && <Typography variant="copy-xs" className="muted">{d.system.advice}</Typography>}
    </Card>
  );
}

interface Release { version: string; name: string; notes: string; published: string; url: string; asset: { name: string; size: number } | null }
interface Change { at: number; from_label: string; to_label: string; status: string; note: string | null }
interface Updates {
  version: string;
  install: { kind: "source"; repo: string; branch: string | null; changed_files: number } | { kind: "package" } | { kind: "managed"; version: string } | { kind: "other"; path: string };
  latest: Release | null;
  update: Release | null;
  history: Change[];
  undo: { to: string | null; label: string } | null;
  working: string | null;
  problem: string | null;
  base: string;
}

/** TT Station's own updates: install the newest release, and undo it. */
function Updates() {
  const u = usePoll(() => get<Updates>("/updates"), 4000);
  const d = u.data;
  const [busy, setBusy] = useState(false);
  const install = async () => {
    if (!d?.update) return;
    if (!(await ask(`Update TT Station to ${d.update.version}?`, { body: "It downloads the release, then restarts the agent on it: the app reconnects in a few seconds, and a model that's serving keeps serving. If the new version doesn't start properly, the current one comes back on its own. You can undo it afterwards.", confirm: "Update" }))) return;
    setBusy(true);
    try { await post("/updates/install", {}); toastOk("Updating…"); void u.refresh(); } catch (e) { toastError(e); } finally { setBusy(false); }
  };
  const undo = async () => {
    if (!d?.undo) return;
    if (!(await ask(`Go back to ${d.undo.label}?`, { body: "The agent restarts on it; the app reconnects in a few seconds.", confirm: "Go back" }))) return;
    try { await post("/updates/undo"); toastOk("Going back…"); } catch (e) { toastError(e); }
  };
  if (!d) return null;
  const src = d.install.kind === "source" ? d.install : null;
  const last = d.history[0];
  return (
    <Card>
      <div className="row-between">
        <Eyebrow>TT Station</Eyebrow>
        <span className="muted mono" style={{ fontSize: 12 }}>{d.version}</span>
      </div>
      {d.working ? (
        <div className="row" style={{ gap: 8 }}><span className="lib-busy">{d.working}…</span></div>
      ) : src ? (
        <span className="muted" style={{ fontSize: 13 }}>
          Built from source ({src.repo.replace(/^\/home\/[^/]+/, "~")}{src.branch ? `, ${src.branch}` : ""}{src.changed_files ? `, ${src.changed_files} uncommitted changes` : ""}). Update it with git and a rebuild.
          {d.latest && <> The newest release is {d.latest.version}.</>}
        </span>
      ) : d.update ? (
        <div className="stack" style={{ gap: 8 }}>
          <span><b>{d.update.version}</b> is out{d.update.published ? ` (${new Date(d.update.published).toLocaleDateString()})` : ""}.</span>
          {d.update.notes && <details className="update-notes"><summary>What's new</summary><pre>{d.update.notes}</pre></details>}
          {d.update.asset
            ? <div><Button disabled={busy} onClick={install}>Update to {d.update.version}</Button></div>
            : <span className="muted" style={{ fontSize: 13 }}>It has no package for this box's Ubuntu yet.</span>}
        </div>
      ) : (
        <span className="muted" style={{ fontSize: 13 }}>Up to date{d.problem ? ` (couldn't check just now: ${d.problem})` : ""}.</span>
      )}
      {last && (
        <span className={`muted ${last.status === "rolled_back" || last.status === "failed" ? "update-warn" : ""}`} style={{ fontSize: 12 }}>
          {last.status === "rolled_back" ? last.note
            : last.status === "failed" ? `Updating to ${last.to_label} didn't work: ${last.note}`
            : last.status === "switching" ? `Switching from ${last.from_label} to ${last.to_label}…`
            : `Updated from ${last.from_label} to ${last.to_label} ${new Date(last.at * 1000).toLocaleDateString()}.`}
        </span>
      )}
      {d.undo && !d.working && <div><Button size="sm" variant="subtle" onClick={undo}>Undo: go back to {d.undo.label}</Button></div>}
    </Card>
  );
}

interface Idle { settings: { enabled: boolean; after_minutes: number; wake_on_request: boolean }; asleep: { label: string; at: number } | null; idle_for_s: number | null }

/** Stop the model when nobody uses it; start it again on the next request. */
function IdleCard() {
  const idle = usePoll(() => get<Idle>("/idle"), 30000);
  const s = idle.data?.settings;
  const save = async (next: Partial<Idle["settings"]>) => {
    if (!s) return;
    try { await put("/idle", { ...s, ...next }); void idle.refresh(); } catch (e) { toastError(e); }
  };
  if (!s) return null;
  const mins = idle.data?.idle_for_s != null ? Math.floor(idle.data.idle_for_s / 60) : null;
  return (
    <Card>
      <Eyebrow>When nobody's using it</Eyebrow>
      <div className="row-between" style={{ gap: 12 }}>
        <div className="stack" style={{ gap: 2 }}>
          <span>Stop the model after it's been idle</span>
          <span className="muted" style={{ fontSize: 12 }}>Four chips serving draw about 200 W even when nobody asks anything. Anything that uses the model counts: the apps, Code, routines, other computers.</span>
        </div>
        <Switch checked={s.enabled} aria-label="Stop the model when idle" onChange={(e) => save({ enabled: e.target.checked })} />
      </div>
      {s.enabled && (
        <>
          <div className="row-between" style={{ gap: 12 }}>
            <span>After</span>
            <Select size="sm" value={String(s.after_minutes)} aria-label="Idle for" onValueChange={(v) => v && save({ after_minutes: Number(v) })}
              options={[15, 30, 60, 120, 240, 480].map((m) => ({ value: String(m), label: m < 60 ? `${m} minutes` : `${m / 60} hour${m > 60 ? "s" : ""}` }))} />
          </div>
          <div className="row-between" style={{ gap: 12 }}>
            <div className="stack" style={{ gap: 2 }}>
              <span>Start it again when something asks</span>
              <span className="muted" style={{ fontSize: 12 }}>The first request after a stop says it's waking up; the model's ready a few minutes later.</span>
            </div>
            <Switch checked={s.wake_on_request} aria-label="Start again on request" onChange={(e) => save({ wake_on_request: e.target.checked })} />
          </div>
        </>
      )}
      {idle.data?.asleep ? (
        <span className="muted" style={{ fontSize: 12 }}>{idle.data.asleep.label} is stopped to save power.</span>
      ) : mins != null && mins >= 1 ? (
        <span className="muted" style={{ fontSize: 12 }}>The model's been idle for {mins} min.</span>
      ) : null}
    </Card>
  );
}

/** A link for the TT Station desktop app on another computer to add this box. */
function PairComputer() {
  const [link, setLink] = useState<{ link: string; left: number } | null>(null);
  useEffect(() => {
    if (!link) return;
    const t = window.setInterval(() => setLink((l) => (l && l.left > 1 ? { ...l, left: l.left - 1 } : null)), 1000);
    return () => window.clearInterval(t);
  }, [link === null]);
  const make = async () => {
    try { const r = await post<PairQr>("/pair/qr"); setLink({ link: r.link, left: r.expires_in_s }); } catch (e) { toastError(e); }
  };
  // What to type in TT Station on the other computer, if it doesn't find this device by itself.
  const connect = usePoll(() => get<{ urls: { label: string; url: string }[] }>("/connect"), 60000);
  const address = connect.data?.urls.map((u) => { try { return new URL(u.url).hostname; } catch { return ""; } }).find((h) => /^\d+\.\d+\.\d+\.\d+$/.test(h));
  const copy = async () => {
    if (!link) return;
    await copyText(link.link, "Copied. Paste it in TT Station on the other computer: the device name at the top left → Add another device.");
  };
  return (
    <Card>
      <Eyebrow>Pair another computer</Eyebrow>
      <Typography variant="copy-sm" className="muted">
        For TT Station's desktop app on another computer (a Mac, say). It finds this device on the network by itself
        {address ? <>, or type its address: <code>{address}</code></> : null}. Then sign in with your account on this device, as you would over SSH.
        A pairing link is the other way in, for someone who doesn't have that login. The link works once, for a few minutes, and holds this device's certificate, so the other computer only ever talks to this device.
      </Typography>
      {link ? (
        <div className="stack" style={{ gap: 6 }}>
          <div className="row" style={{ gap: 8 }}>
            <code className="pair-link" title={link.link}>{link.link.replace(/code=[^&]+/, "code=••••••")}</code>
            <Button size="sm" onClick={copy}>Copy</Button>
          </div>
          <span className="muted" style={{ fontSize: 12 }}>Works once, for {Math.floor(link.left / 60)}:{String(link.left % 60).padStart(2, "0")}.</span>
        </div>
      ) : <div><Button variant="subtle" onClick={make}>Make a pairing link</Button></div>}
    </Card>
  );
}
