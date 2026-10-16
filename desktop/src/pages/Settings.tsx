// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Settings: the desktop app, power, the household, devices and pairing (the
// one place for it: the box switcher and the connect screen point here),
// security, your data and the box's software. `/app/settings#<section>` opens
// a section.
//
// Schedules are routines now (Routines → Switch the box's mode), and energy
// use is on Hardware; the Power section points to both.

import { useEffect, useRef, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Switch } from "@tenstorrent/vesper/switch";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Select } from "@tenstorrent/vesper/select";
import { Badge } from "@tenstorrent/vesper/badge";
import { Typography } from "@tenstorrent/vesper/typography";
import { useModal, Modal } from "@tenstorrent/vesper/modal";
import { ago, ApiError, type AuditEntry, authHeader, boxName, bytes, del, delWith, get, patch, type PairedDevice, type Person, post, put, upload } from "../api";
import { changesSomething, type DataSettings, describePreview, exportFileName, type ImportPreview, MODEL_CONTROL_CHOICES, type ModelControl, modelControlSaved, RETENTION_CHOICES, retentionDays, retentionValue } from "../yourdata";
import { OWNER_ONLY, useOwner } from "../role";
import { ThemeSwitcher } from "@tenstorrent/vesper/theme-switcher";
import { Card, copyText, Empty, Eyebrow, Page, readable, toastError, toastOk, usePoll } from "../ui";
import { navigate } from "../App";
import { type Section, SECTIONS, sectionFrom } from "../settingsSections";
import { Speaker, type VoiceStatus } from "../voice";
import { ask, askText } from "../dialog";
import { DevToolsSetting } from "../DevToolsSetting";
import { AllowedActionsSetting } from "../AllowedActionsSetting";

export function Settings() {
  const [section, setSection] = useState<Section>(() => {
    const linked = sectionFrom(location.hash);
    if (linked) return linked;
    try { return sectionFrom(localStorage.getItem("tt.settings.section")) ?? "general"; } catch { return "general"; }
  });
  useEffect(() => { try { localStorage.setItem("tt.settings.section", section); } catch { /* private */ } }, [section]);
  // A link to a section while Settings is open (Notifications, the box switcher).
  useEffect(() => {
    const onHash = () => { const s = sectionFrom(location.hash); if (s) setSection(s); };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);
  const current = SECTIONS.find((x) => x.id === section) ?? SECTIONS[0];
  return (
    <Page title="Settings">
      <div className="settings">
        <nav className="settings-nav" aria-label="Settings sections">
          {SECTIONS.map((x) => (
            <button key={x.id} className={x.id === section ? "on" : ""} aria-current={x.id === section ? "page" : undefined}
              onClick={() => { setSection(x.id); history.replaceState(null, "", `/app/settings#${x.id}`); }}>
              <span>{x.label}</span>
              <span className="muted">{x.hint}</span>
            </button>
          ))}
        </nav>
        <div className="settings-body" key={section}>
          <Typography variant="heading-sm">{current.label}</Typography>
          {section === "general" && <Desktop />}
          {section === "tori" && <><ToriCard /><Voice /></>}
          {section === "models" && <><ModelControlCard /><ModelPrefs /></>}
          {section === "power" && <><Power /><IdleCard /><AfterPowerCut /><MovedFromPower /></>}
          {section === "people" && <Household />}
          {section === "data" && <YourData />}
          {section === "devices" && <Pairing />}
          {section === "security" && <SecurityAndActivity />}
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
      <DevToolsSetting />
      <TokenStorage />
      <AllowedActionsSetting />
      <div className="list">
        {[["Ctrl 1 – 9", "Jump between places (in sidebar order)"], ["Ctrl Enter", "Make (in Studio)"]].map(([k, v]) => (
          <div key={k} className="list-row"><span className="kbd">{k}</span><span className="muted">{v}</span></div>
        ))}
      </div>
      <Typography variant="copy-xs" className="muted">Closing the window keeps TT Station in the tray, so notifications still arrive. Quit from the tray icon.</Typography>
    </Card>
  );
}

/** A key press → a GNOME binding, or null for a lone modifier. */
type Storage = { protected: boolean; where: string | null; backend: string };

/** Where this computer keeps its sign-in for each box (the desktop app only). */
function useTokenStorage() {
  const [storage, setStorage] = useState<Storage | null>(null);
  useEffect(() => { window.ttDesktop?.tokenStorage?.().then(setStorage).catch(() => setStorage(null)); }, []);
  return storage;
}

function TokenStorage() {
  const storage = useTokenStorage();
  if (!storage) return null;
  return (
    <div className="stack" style={{ gap: 4 }}>
      <div className="row-between wrap" style={{ gap: 12 }}>
        <span>Sign-in to your boxes</span>
        {storage.protected
          ? <span className="muted">Encrypted in {storage.where}</span>
          : <Badge size="sm" subtle variant="warning">Not in a keychain</Badge>}
      </div>
      {!storage.protected && (
        <Typography variant="copy-xs" className="muted">
          This computer has no keychain TT Station can use{storage.backend === "basic_text" ? " (no login keyring is running)" : ""}, so its sign-in tokens are kept in files only your account can read. Anyone who can sign in as you here could copy them.
        </Typography>
      )}
    </div>
  );
}

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
      try {
        const next = await window.ttDesktop!.toriShortcut(binding);
        setInfo(next);
        window.dispatchEvent(new CustomEvent("tt:shortcut", { detail: next.installed ? binding : "" }));
        toastOk(`Tori now opens with ${readable(binding).join(" + ")}.`);
      }
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
            {info.installed ? "Opens Tori's window from anywhere." : "Off. Turn it on to open Tori's window from anywhere."}
            {window.ttDesktop?.platform === "darwin" && info.binding === "<Control><Alt>space" && " On a Mac, ⌃⌥Space also switches input sources: change it if you use that."}
          </div>
        </div>
        <div className="row" style={{ gap: 8 }}>
          {capturing
            ? <span className="kbd shortcut-capture">Press the new shortcut… (Esc to cancel)</span>
            : <span className="row" style={{ gap: 4 }}>{readable(info.binding).map((k) => <span key={k} className="kbd">{k}</span>)}</span>}
          <Button size="sm" variant="subtle" onClick={() => setCapturing((c) => !c)}>{capturing ? "Cancel" : "Change"}</Button>
        </div>
      </div>
      <div>
        <Button size="sm" variant="ghost" onClick={async () => {
          try {
            await window.ttDesktop!.shortcuts(!info.installed);
            const next = await window.ttDesktop!.toriShortcut();
            setInfo(next);
            window.dispatchEvent(new CustomEvent("tt:shortcut", { detail: next.installed ? next.binding : "" }));
          } catch (e) { toastError(e); }
        }}>
          {info.installed ? "Turn the shortcut off" : "Turn the shortcut on"}
        </Button>
      </div>
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
    preview.finish("Hi, I'm Tori. Ask me anything, and I'll answer right here, on your box.");
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
  const owner = useOwner();
  const modal = useModal();
  const [action, setAction] = useState<{ id: string; label: string; detail: string } | null>(null);
  const ask = (id: string, label: string, detail: string) => { setAction({ id, label, detail }); modal.open(); };
  return (
    <Card>
      <Eyebrow>Power</Eyebrow>
      <div className="row wrap">
        <Button size="sm" variant="subtle" disabled={!owner || info.data?.can_suspend !== "yes"} onClick={() => ask("suspend", "Sleep", "The box sleeps; wake it from the phone app or its power button.")}>Sleep</Button>
        <Button size="sm" variant="subtle" disabled={!owner || info.data?.can_reboot !== "yes"} onClick={() => ask("reboot", "Restart", "The box restarts. TT Station reconnects in a minute or two.")}>Restart</Button>
        <Button size="sm" variant="ghost" disabled={!owner || info.data?.can_power_off !== "yes"} onClick={() => ask("shutdown", "Shut down", "The box turns off.")}>Shut down</Button>
        <Button size="sm" variant="ghost" disabled={!owner} onClick={() => ask("reset_chips", "Reset the chips", "Stops serving and resets all the box's chips.")}>Reset chips</Button>
      </div>
      {!owner && <Typography variant="copy-xs" className="muted">{OWNER_ONLY}</Typography>}
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
  const owner = useOwner();
  const [name, setName] = useState("");
  const run = async (fn: () => Promise<{ people: Person[]; me: string | null }>) => {
    try { people.setData(await fn()); } catch (e) { toastError(e); }
  };
  const list = people.data;
  return (
    <Card>
      <Eyebrow>You and your household</Eyebrow>
      <Typography variant="copy-sm" className="muted">Each person gets their own memories with the assistant. A person is just a name on the box. You can pick someone no other device belongs to yet; to share a person between devices, an owner sets it under Devices and pairing.</Typography>
      {list && (
        <Select value={list.me ?? "anyone"} onValueChange={(v) => run(() => put("/people/me", { person: v === "anyone" ? null : v }))}
          options={[{ label: "This computer is: anyone (shared)", value: "anyone" }, ...list.people.map((p) => ({ label: `This computer is: ${p.name}`, value: p.id }))]} />
      )}
      <div className="list">
        {(list?.people ?? []).map((p) => (
          <div key={p.id} className="list-row">
            <span className="grow">{p.name}{list?.me === p.id && <Badge size="sm" subtle variant="mint" style={{ marginLeft: 8 }}>you</Badge>}</span>
            <Button size="xs" variant="ghost" disabled={!owner} title={owner ? undefined : OWNER_ONLY} onClick={async () => { const n = await askText(`Rename ${p.name}`, p.name); if (n) run(() => patch(`/people/${p.id}`, { name: n })); }}>Rename</Button>
            <Button size="xs" variant="ghost" disabled={!owner} title={owner ? undefined : OWNER_ONLY} onClick={async () => { if (await ask(`Remove ${p.name}?`, { body: "Tori forgets what she remembered about them. Their phone or computer stays paired: remove it under Devices and pairing.", confirm: "Remove", danger: true })) run(() => del(`/people/${p.id}`)); }}>Remove</Button>
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

/** Where Schedules and Energy went: one place each, elsewhere. */
function MovedFromPower() {
  return (
    <Card>
      <Eyebrow>Looking for schedules or energy?</Eyebrow>
      <div className="list">
        <div className="list-row">
          <div className="grow">
            <b>Switching modes at set times</b>
            <div className="muted" style={{ fontSize: 12 }}>It's a routine now, and your schedules have already moved over. In Routines, choose New routine, then Switch the box's mode.</div>
          </div>
          <Button size="sm" variant="subtle" onClick={() => navigate("routines")}>Open Routines</Button>
        </div>
        <div className="list-row">
          <div className="grow">
            <b>Energy use</b>
            <div className="muted" style={{ fontSize: 12 }}>It's on Hardware now, next to the chips' power: today, this month and each day, with what it costs.</div>
          </div>
          <Button size="sm" variant="subtle" onClick={() => navigate("hardware", "energy")}>Open Hardware</Button>
        </div>
      </div>
    </Card>
  );
}

interface PairQr { link: string; qr_svg: string | null; expires_in_s: number; host: string; https_port: number; fingerprint: string }
interface Security { lan_http: boolean; https_port: number; tls: { port: number; sha256: string } | null }
interface SshKey { label: string; type: string; added: string | null; raw: string }

/** The certificate fingerprint in groups of four, easier to compare by eye. */
const grouped = (hex: string) => hex.match(/.{1,4}/g)?.join(" ") ?? hex;

/**
 * Devices and pairing, the one place for it: the devices that can use this
 * box, pairing a phone or another computer, adding another box to this app,
 * and the SSH keys pairing adds. The box switcher and the connect screen on
 * other computers point here.
 */
function Pairing() {
  const owner = useOwner();
  const ssh = usePoll(() => get<{ keys: SshKey[]; user: string }>("/ssh/info"), 10000);
  const revoke = async (k: SshKey) => {
    if (!(await ask(`Remove ${k.label}'s SSH key?`, { body: "That device can no longer sign in to the box over SSH. It can be added again by pairing.", confirm: "Remove", danger: true }))) return;
    try { await delWith("/ssh/authorize", { label: k.raw }); toastOk("Removed."); ssh.refresh(); } catch (e) { toastError(e); }
  };
  return (
    <>
      <Devices />
      <PairDevice />
      {window.ttDesktop?.showConnect && (
        <Card>
          <Eyebrow>Another box</Eyebrow>
          <div className="row-between wrap" style={{ gap: 12 }}>
            <Typography variant="copy-sm" className="muted">Use another box from this app too. Switch between them from the box name at the top of the sidebar.</Typography>
            <Button size="sm" variant="subtle" onClick={() => void window.ttDesktop!.showConnect!()}>Add another box…</Button>
          </div>
        </Card>
      )}
      <Card>
        <Eyebrow>SSH keys this box trusts</Eyebrow>
        <div className="list">
          {(ssh.data?.keys ?? []).map((k) => (
            <div key={k.raw} className="list-row">
              <div className="grow"><b>{k.label}</b><div className="muted mono" style={{ fontSize: 12 }}>{k.type}{k.added ? ` · added ${k.added}` : ""}</div></div>
              <Button size="xs" variant="ghost" disabled={!owner} title={owner ? undefined : OWNER_ONLY} onClick={() => revoke(k)}>Remove</Button>
            </div>
          ))}
          {ssh.data && ssh.data.keys.length === 0 && <span className="muted" style={{ fontSize: 13 }}>None added through TT Station yet. Pairing a phone adds its key (an owner's phone: SSH is a way into the whole box).</span>}
        </div>
        {!owner && <Typography variant="copy-xs" className="muted">{OWNER_ONLY}</Typography>}
      </Card>
    </>
  );
}

/**
 * Pairing a phone or another computer. An iPhone scans a QR that holds a
 * one-time code and the box's certificate fingerprint, so it talks to the box
 * only over HTTPS pinned to that certificate (it makes its own SSH key and
 * sends only the public half). Another computer finds the box by itself and
 * signs in with the box's account, or pastes a one-time link.
 */
function PairDevice() {
  const [what, setWhat] = useState<"phone" | "computer">("phone");
  return (
    <Card>
      <div className="row-between wrap" style={{ gap: 12 }}>
        <Eyebrow>Pair a device</Eyebrow>
        <div className="pill-tabs" role="tablist" aria-label="What to pair">
          <button role="tab" aria-selected={what === "phone"} className={what === "phone" ? "on" : ""} onClick={() => setWhat("phone")}>An iPhone</button>
          <button role="tab" aria-selected={what === "computer"} className={what === "computer" ? "on" : ""} onClick={() => setWhat("computer")}>Another computer</button>
        </div>
      </div>
      {what === "phone" ? <PairPhone /> : <PairComputer />}
    </Card>
  );
}

function PairPhone() {
  const [qr, setQr] = useState<PairQr | null>(null);
  const [left, setLeft] = useState(0);
  useEffect(() => {
    if (!qr) return;
    const t = window.setInterval(() => setLeft((n) => { if (n <= 1) { setQr(null); return 0; } return n - 1; }), 1000);
    return () => window.clearInterval(t);
  }, [qr]);
  const show = async () => {
    try { const r = await post<PairQr>("/pair/qr"); setQr(r); setLeft(r.expires_in_s); } catch (e) { toastError(e); }
  };
  const mm = `${Math.floor(left / 60)}:${String(left % 60).padStart(2, "0")}`;
  return (
    <>
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
    </>
  );
}

/** Encryption and the activity log (owners). */
function SecurityAndActivity() {
  const owner = useOwner();
  const security = usePoll(() => get<Security>("/security"), 30000);
  const setLanHttp = async (on: boolean) => {
    if (on && !(await ask("Allow unencrypted connections from other computers?", {
      body: "Apps on other computers could then use the box's API (/v1) over plain HTTP, readable by anyone on your network. Phones always use encrypted HTTPS either way.",
      confirm: "Allow", danger: true,
    }))) return;
    try { await post("/security", { lan_http: on }); security.refresh(); } catch (e) { toastError(e); }
  };
  const fp = security.data?.tls?.sha256;
  return (
    <>
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
          <Switch checked={!!security.data?.lan_http} disabled={!owner} onChange={(e) => setLanHttp(e.target.checked)} />
        </div>
        {!owner && <Typography variant="copy-xs" className="muted">{OWNER_ONLY}</Typography>}
      </Card>
      {owner ? <ActivityLog /> : (
        <Card><Eyebrow>Activity log</Eyebrow><Typography variant="copy-sm" className="muted">Only owners can read the box's activity log.</Typography></Card>
      )}
    </>
  );
}

function Software() {
  const owner = useOwner();
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
          {d?.tt?.newer && <Button size="xs" variant="subtle" disabled={updating || !owner} title={owner ? undefined : OWNER_ONLY} onClick={updateTt}>{updating ? "Updating…" : `Update to ${d.tt.latest}`}</Button>}</div>
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
  /** False when this build has no release signing key, so the box refuses installs (absent on older boxes: ready). */
  signing_ready?: boolean;
}

/** TT Station's own updates: install the newest release, and undo it. */
function Updates() {
  const u = usePoll(() => get<Updates>("/updates"), 4000);
  const owner = useOwner();
  const d = u.data;
  const [busy, setBusy] = useState(false);
  const install = async () => {
    if (!d?.update || d.signing_ready === false) return;
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
      ) : d.signing_ready === false ? (
        <span className="muted" style={{ fontSize: 13 }}>Updates aren't available for this build yet.</span>
      ) : d.update ? (
        <div className="stack" style={{ gap: 8 }}>
          <span><b>{d.update.version}</b> is out{d.update.published ? ` (${new Date(d.update.published).toLocaleDateString()})` : ""}.</span>
          {d.update.notes && <details className="update-notes"><summary>What's new</summary><pre>{d.update.notes}</pre></details>}
          {d.update.asset
            ? <div className="stack" style={{ gap: 4 }}><div><Button disabled={busy || !owner} onClick={install}>Update to {d.update.version}</Button></div>{!owner && <span className="muted" style={{ fontSize: 12 }}>{OWNER_ONLY}</span>}</div>
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
      {d.undo && !d.working && <div><Button size="sm" variant="subtle" disabled={!owner} title={owner ? undefined : OWNER_ONLY} onClick={undo}>Undo: go back to {d.undo.label}</Button></div>}
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
          <span className="muted" style={{ fontSize: 12 }}>A serving model keeps the chips drawing power even when nobody asks anything. Anything that uses the model counts: the apps, Code, routines, other computers.</span>
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

type DataSettingsReply = { settings: DataSettings; owner: boolean; may_control_models?: boolean };

/** The household's data settings, shared by the cards that change them. */
function useDataSettings() {
  const data = usePoll(() => get<DataSettingsReply>("/data/settings"), 60000);
  const save = async (next: Partial<DataSettings>, said = "Saved.") => {
    try { await put("/data/settings", next); void data.refresh(); toastOk(said); } catch (e) { toastError(e); }
  };
  return { settings: data.data?.settings, owner: data.data?.owner !== false, save };
}

/**
 * Your data (owners change it): what happens to recordings and transcripts,
 * how long the box keeps them, and taking everything to another box.
 */
function YourData() {
  const { settings: s, owner, save } = useDataSettings();
  const [busy, setBusy] = useState<"export" | "import" | null>(null);
  const picker = useRef<HTMLInputElement>(null);

  const exportAll = async () => {
    setBusy("export");
    try {
      const res = await fetch("/export", { method: "POST", headers: authHeader() });
      if (!res.ok) {
        const body = await res.json().catch(() => null) as { error?: string } | null;
        throw new ApiError(res.status, body?.error ?? res.statusText);
      }
      const blob = await res.blob();
      const name = exportFileName(res.headers.get("content-disposition"));
      const a = document.createElement("a");
      a.href = URL.createObjectURL(blob);
      a.download = name;
      a.click();
      setTimeout(() => URL.revokeObjectURL(a.href), 10000);
      toastOk(`Saved ${name} (${bytes(blob.size)}). Keep it somewhere private: it has your transcripts, reports and memories.`);
    } catch (e) { toastError(e); } finally { setBusy(null); }
  };

  const importFile = async (file: File) => {
    setBusy("import");
    try {
      const { preview } = await upload<{ preview: ImportPreview }>("/import?dry_run=true", file);
      if (!changesSomething(preview)) { toastOk(describePreview(preview)); return; }
      const sure = await ask(`Import ${file.name}?`, {
        body: `${describePreview(preview)} Nothing on this box is deleted.`,
        confirm: "Import",
      });
      if (!sure) return;
      const done = await upload<{ preview: ImportPreview }>("/import", file);
      toastOk(`Imported. ${describePreview(done.preview)}`);
    } catch (e) { toastError(e); } finally {
      setBusy(null);
      if (picker.current) picker.current.value = "";
    }
  };

  if (!s) return null;
  return (
    <>
      <Card>
        <Eyebrow>Recordings and transcripts</Eyebrow>
        <div className="row-between" style={{ gap: 12 }}>
          <div className="stack" style={{ gap: 2 }}>
            <span>Keep recordings after they're transcribed</span>
            <span className="muted" style={{ fontSize: 12 }}>Off: the audio is deleted once the transcript is ready. Someone can still keep one when they upload it.</span>
          </div>
          <Switch checked={s.keep_audio} disabled={!owner} aria-label="Keep recordings" onChange={(e) => save({ keep_audio: e.target.checked })} />
        </div>
        <div className="row-between" style={{ gap: 12 }}>
          <div className="stack" style={{ gap: 2 }}>
            <span>Add transcripts to the Library</span>
            <span className="muted" style={{ fontSize: 12 }}>The Library is shared: everyone on the box, and the assistant, can search it. Off: a transcript goes there only when whoever made it asks.</span>
          </div>
          <Switch checked={s.transcripts_to_library} disabled={!owner} aria-label="Add transcripts to the Library" onChange={(e) => save({ transcripts_to_library: e.target.checked })} />
        </div>
        <div className="row-between" style={{ gap: 12 }}>
          <div className="stack" style={{ gap: 2 }}>
            <span>Delete transcripts and research reports after</span>
            <span className="muted" style={{ fontSize: 12 }}>Counted from when each one finished. Anything still running is never deleted.</span>
          </div>
          <Select size="sm" value={retentionValue(s.retention_days)} disabled={!owner} aria-label="Delete after" options={RETENTION_CHOICES}
            onValueChange={(v) => v && save({ retention_days: retentionDays(v) }, v === "keep" ? "Kept until you delete them." : "Saved. Older ones go within the hour.")} />
        </div>
        <Typography variant="copy-xs" className="muted">People see their own transcripts and the ones shared with them; owners see all of them.</Typography>
        {!owner && <Typography variant="copy-xs" className="muted">{OWNER_ONLY}</Typography>}
      </Card>
      <Card>
        <Eyebrow>Take your data with you</Eyebrow>
        <Typography variant="copy-sm" className="muted">
          One file with your research reports, transcripts (and recordings you kept), memories, people, routines (mode switches included), settings, model check-ups, Code sessions' details and image prompts. It never includes passwords, keys or the box's pairings, so devices pair again on a new box.
        </Typography>
        <div className="row" style={{ gap: 8 }}>
          <Button size="sm" disabled={!owner || busy !== null} onClick={exportAll}>{busy === "export" ? "Exporting…" : "Export"}</Button>
          <Button size="sm" variant="subtle" disabled={!owner || busy !== null} onClick={() => picker.current?.click()}>{busy === "import" ? "Importing…" : "Import…"}</Button>
          <input ref={picker} type="file" accept=".tar.gz,.tgz,application/gzip" hidden aria-label="Export file to import"
            onChange={(e) => { const f = e.target.files?.[0]; if (f) void importFile(f); }} />
        </div>
        <Typography variant="copy-xs" className="muted">Importing shows what it would add or replace first. It adds to what's here and replaces only things with the same name.</Typography>
        {!owner && <Typography variant="copy-xs" className="muted">{OWNER_ONLY}</Typography>}
      </Card>
    </>
  );
}

/** Who may start, stop and switch the model everyone uses (owners set it). */
function ModelControlCard() {
  const { settings: s, owner, save } = useDataSettings();
  if (!s) return null;
  const value: ModelControl = s.model_control ?? "anyone";
  return (
    <Card>
      <Eyebrow>Who can start and stop models</Eyebrow>
      <div className="row-between" style={{ gap: 12 }}>
        <div className="stack" style={{ gap: 2 }}>
          <span>Start, stop or switch the model the box serves</span>
          <span className="muted" style={{ fontSize: 12 }}>
            The model is shared: switching it changes it for everyone. This covers Models, modes, Code's model picker, check-ups, Tori, routines run for someone, and the Doctor's restart.
          </span>
        </div>
        <Select size="sm" value={value} disabled={!owner} aria-label="Who can start and stop models" options={MODEL_CONTROL_CHOICES}
          onValueChange={(v) => v && v !== value && save({ model_control: v as ModelControl }, modelControlSaved(v as ModelControl))} />
      </div>
      {!owner && <Typography variant="copy-xs" className="muted">{OWNER_ONLY}</Typography>}
    </Card>
  );
}

/** After a power cut: start the model that was serving again (owners). */
function AfterPowerCut() {
  const { settings: s, owner, save } = useDataSettings();
  if (!s) return null;
  return (
    <Card>
      <Eyebrow>After a power cut</Eyebrow>
      <div className="row-between" style={{ gap: 12 }}>
        <div className="stack" style={{ gap: 2 }}>
          <span>Start the last model when the box boots</span>
          <span className="muted" style={{ fontSize: 12 }}>Only if it was serving when the box went down. A model someone stopped stays stopped.</span>
        </div>
        <Switch checked={s.restart_last_model} disabled={!owner} aria-label="Start the last model when the box boots" onChange={(e) => save({ restart_last_model: e.target.checked })} />
      </div>
      {!owner && <Typography variant="copy-xs" className="muted">{OWNER_ONLY}</Typography>}
    </Card>
  );
}

const pairedOn = (unix: number) => new Date(unix * 1000).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });

/**
 * Every computer and phone that can control this box, each signed out on its
 * own. The box keeps a device for 90 days after it was last used; removing
 * one stops its next request and closes any terminal it has open.
 *
 * Owners see every device, and make others owners or members, and say whose
 * each device is. A member sees only itself.
 */
function Devices() {
  const devices = usePoll(() => get<{ devices: PairedDevice[]; idle_expiry_days: number; owner?: boolean }>("/devices"), 30000);
  const owner = devices.data?.owner !== false;
  const roles = devices.data?.owner !== undefined;
  const people = usePoll(() => (owner && roles ? get<{ people: Person[] }>("/people") : Promise.resolve({ people: [] as Person[] })), 60000, [owner, roles]);
  const storage = useTokenStorage();
  const [onBox, setOnBox] = useState(false);
  useEffect(() => { window.ttDesktop?.boxes?.().then((b) => setOnBox(b.current === "local")).catch(() => {}); }, []);
  const here = window.ttDesktop ? "This computer" : "This browser";
  const remove = async (d: PairedDevice) => {
    const body = !d.this_device
      ? `${d.name} stops working with ${boxName} right away, and any terminal it has open closes. To use it with this box again, pair it again.`
      : onBox
        ? "TT Station on this box signs out and shows the connect screen. Because it runs on the box itself, it can sign in again without a code: pick this device there."
        : `This computer stops working with ${boxName} right away and goes back to the connect screen. To use the box again, pair it again.`;
    if (!(await ask(d.this_device ? `Sign ${here.toLowerCase()} out?` : `Remove ${d.name}?`, { body, confirm: d.this_device ? "Sign out" : "Remove", danger: true }))) return;
    try {
      await del(`/devices/${encodeURIComponent(d.id)}`);
    } catch (e) { toastError(e); return; }
    if (!d.this_device) { toastOk(`${d.name} was removed.`); devices.refresh(); return; }
    if (window.ttDesktop?.signedOut) { await window.ttDesktop.signedOut(); return; }
    // A browser on the box (development): forget the pasted token.
    try { localStorage.removeItem("tt.token"); } catch { /* private */ }
    window.location.reload();
  };
  const setOwner = async (d: PairedDevice, makeOwner: boolean) => {
    const body = makeOwner
      ? `${d.name} can then see and remove devices, make other devices owners, add SSH keys and app keys, and use power, updates and reset.`
      : `${d.name} keeps chat, research, Code, the studio, the library and its own memories, but can't manage devices, keys, power or updates.`;
    if (!(await ask(makeOwner ? `Make ${d.name} an owner?` : `Make ${d.name} a member?`, { body, confirm: makeOwner ? "Make owner" : "Make member", danger: makeOwner }))) return;
    try { await post(`/devices/${encodeURIComponent(d.id)}/owner`, { owner: makeOwner }); toastOk(`${d.name} is ${makeOwner ? "an owner" : "a member"} now.`); devices.refresh(); } catch (e) { toastError(e); }
  };
  const setPerson = async (d: PairedDevice, person: string | null) => {
    try { await put(`/devices/${encodeURIComponent(d.id)}/person`, { person }); devices.refresh(); } catch (e) { toastError(e); }
  };
  const days = devices.data?.idle_expiry_days ?? 90;
  const now = Date.now() / 1000;
  const list = devices.data?.devices ?? [];
  const folks = people.data?.people ?? [];
  return (
    <Card>
      <Eyebrow>Devices</Eyebrow>
      <Typography variant="copy-sm" className="muted">
        Computers and phones that can use and control this box. Remove any you don't recognize or no longer use; a device unused for {days} days is signed out automatically.
      </Typography>
      {devices.error && !devices.data ? (
        <div className="row-between wrap" style={{ gap: 12 }}>
          <span className="muted">Couldn't load the devices: {devices.error}</span>
          <Button size="sm" variant="ghost" onClick={devices.refresh}>Try again</Button>
        </div>
      ) : !devices.data ? (
        <span className="muted" style={{ fontSize: 13 }}>Loading…</span>
      ) : list.length === 0 ? (
        <Empty title="No devices are paired">Pair a phone or another computer below.</Empty>
      ) : (
        <div className="list">
          {list.map((d) => {
            const left = Math.ceil((d.expires - now) / 86400);
            return (
              <div key={d.id} className="list-row">
                <div className="grow">
                  <b>{d.name}</b>
                  {d.this_device && <Badge size="sm" subtle variant="mint" style={{ marginLeft: 8 }}>{here}</Badge>}
                  {roles && <Badge size="sm" subtle variant={d.owner ? "info" : "contrast"} style={{ marginLeft: 8 }}>{d.owner ? "Owner" : "Member"}</Badge>}
                  <div className="muted" style={{ fontSize: 12 }}>
                    {[d.platform,
                      d.paired != null ? `paired ${pairedOn(d.paired)}` : "pairing date not recorded",
                      d.this_device ? "in use now" : `last used ${ago(d.last_used)}`,
                      !d.this_device && left <= 14 ? `signs out in ${left} day${left === 1 ? "" : "s"} unless used` : null,
                    ].filter(Boolean).join(" · ")}
                  </div>
                </div>
                {roles && owner && folks.length > 0 && (
                  <Select size="sm" aria-label={`Whose is ${d.name}`} value={d.person ?? "anyone"} onValueChange={(v) => setPerson(d, v === "anyone" ? null : v)}
                    options={[{ label: "Anyone's", value: "anyone" }, ...folks.map((p) => ({ label: `${p.name}'s`, value: p.id }))]} />
                )}
                {roles && owner && <Button size="xs" variant="subtle" onClick={() => setOwner(d, !d.owner)}>{d.owner ? "Make member" : "Make owner"}</Button>}
                <Button size="xs" variant="ghost" onClick={() => remove(d)}>{d.this_device ? "Sign out" : "Remove"}</Button>
              </div>
            );
          })}
        </div>
      )}
      {roles && !owner && (
        <Typography variant="copy-xs" className="muted">
          {here} is a member of this box: it can use the box, but only an owner can see and remove other devices, add SSH or app keys, or use power, updates and reset. An owner can make it one here.
        </Typography>
      )}
      {storage && !storage.protected && (
        <Typography variant="copy-xs" className="muted">
          {here}'s own sign-in isn't in a keychain: it's kept in a file only your account can read (Settings → General → This app).
        </Typography>
      )}
    </Card>
  );
}

const ACTIONS: Record<string, string> = {
  pair: "Paired", revoke: "Removed a device", promote: "Made an owner", demote: "Made a member", reset: "Reset the box",
  ssh_key_add: "Added an SSH key", ssh_key_remove: "Removed an SSH key", app_key_create: "Made an app key", app_key_change: "Changed an app key",
  app_key_revoke: "Revoked an app key", power: "Power", security: "Security settings", update_install: "Updated TT Station",
  update_undo: "Undid an update", update_tt_cli: "Updated tt", approval: "Answered an approval", bringup_approval: "Answered a bring-up approval",
  assistant_approval: "Answered Tori", model_start: "Started a model", model_stop: "Stopped the model", model_switch: "Switched models",
  mode_switch: "Switched mode", device_person: "Set whose a device is", person_remove: "Removed a person", denied: "Refused (owners only)",
  host_run: "Ran something on the box itself",
};

/** The box's activity log (owners): who did what, newest first, kept on the box. */
function ActivityLog() {
  const [offset, setOffset] = useState(0);
  const PAGE = 25;
  const log = usePoll(() => get<{ entries: AuditEntry[]; more: boolean }>(`/audit?offset=${offset}&limit=${PAGE}`), 30000, [offset]);
  const entries = log.data?.entries ?? [];
  return (
    <Card>
      <Eyebrow>Activity log</Eyebrow>
      <Typography variant="copy-sm" className="muted">
        Who paired, removed devices, changed keys, power, updates or security, answered approvals, and started or stopped models, and what members were refused. Kept on the box, readable only by its owners.
      </Typography>
      {log.error && !log.data ? (
        <span className="muted">Couldn't load the activity log: {log.error}</span>
      ) : entries.length === 0 ? (
        <span className="muted" style={{ fontSize: 13 }}>{log.data ? "Nothing yet." : "Loading…"}</span>
      ) : (
        <div className="list">
          {entries.map((e, i) => (
            <div key={`${e.ts}-${i}`} className="list-row">
              <div className="grow">
                <b>{ACTIONS[e.action] ?? e.action}</b>{e.target ? <span className="muted">: {e.target}</span> : null}
                <div className="muted" style={{ fontSize: 12 }}>{e.device_name} · {ago(e.ts)}{e.result !== "ok" ? ` · ${e.result.replace("_", " ")}` : ""}</div>
              </div>
            </div>
          ))}
        </div>
      )}
      {(offset > 0 || log.data?.more) && (
        <div className="row">
          <Button size="xs" variant="ghost" disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - PAGE))}>Newer</Button>
          <Button size="xs" variant="ghost" disabled={!log.data?.more} onClick={() => setOffset(offset + PAGE)}>Older</Button>
        </div>
      )}
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
    await copyText(link.link, "Copied. Paste it in TT Station on the other computer, on its connect screen (Have a pairing link?).");
  };
  return (
    <>
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
    </>
  );
}
