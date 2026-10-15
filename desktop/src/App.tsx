// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The app's frame: a sidebar of places, the box's live state at the bottom of
// it, and the current place on the right. Routes live under /app/.

import { type ComponentType, lazy, Suspense, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { Icon } from "@tenstorrent/vesper/icons";
import type { IconKind } from "@tenstorrent/vesper/icons";
import { Toasts, addToast } from "@tenstorrent/vesper/toast";
import { StatusIndicator } from "@tenstorrent/vesper/status-indicator";
import { Button } from "@tenstorrent/vesper/button";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Typography } from "@tenstorrent/vesper/typography";
import { boxName, get, onAnotherComputer, openBoxShell, setToken, short, signIn } from "./api";
import { pressable, toastError, useServe, useTelemetry, tempTone, tempWord, useToriShortcut } from "./ui";
import { Home } from "./pages/Home";
import { adoptFromBox, placeVisible, storedChoice, useDevTools } from "./devtools";
import { OfflineBanner } from "./OfflineBanner";
import { RecordingBanner } from "./RecordingBanner";
import { useConnectivity } from "./connectivity";
import { useInbox } from "./useInbox";
import { Badge } from "@tenstorrent/vesper/badge";

// Every place but Home loads when it's first opened, so the app starts on Home
// without waiting for screens it may not need.
const named = <K extends string>(load: () => Promise<Record<K, ComponentType>>, name: K) =>
  lazy(() => load().then((m) => ({ default: m[name] })));
const Chat = named(() => import("./pages/Chat"), "Chat");
const Studio = named(() => import("./pages/Studio"), "Studio");
const Models = named(() => import("./pages/Models"), "Models");
const BringupPage = named(() => import("./pages/BringupWays"), "BringupPage");
const Research = named(() => import("./pages/Research"), "Research");
const Code = named(() => import("./pages/Code"), "Code");
const Terminals = named(() => import("./pages/Terminals"), "Terminals");
const Hardware = named(() => import("./pages/Hardware"), "Hardware");
const Connect = named(() => import("./pages/Connect"), "Connect");
const Discover = named(() => import("./pages/Discover"), "Discover");
const Settings = named(() => import("./pages/Settings"), "Settings");
const Routines = named(() => import("./pages/Routines"), "Routines");
const Library = named(() => import("./pages/Library"), "Library");
const Inbox = named(() => import("./pages/Inbox"), "Inbox");
const ToriWindow = named(() => import("./Tori"), "ToriWindow");

export type Route = "home" | "inbox" | "chat" | "studio" | "research" | "code" | "terminals" | "models" | "bringup" | "orchard" | "hardware" | "discover" | "connect" | "routines" | "library" | "settings";

const PLACES: { route: Route; label: string; icon: IconKind; group: number }[] = [
  { route: "home", label: "Home", icon: "quietbox", group: 0 },
  { route: "inbox", label: "Needs you", icon: "user", group: 0 },
  { route: "chat", label: "Chat", icon: "chat", group: 0 },
  { route: "studio", label: "Studio", icon: "ai-video-gen", group: 0 },
  { route: "research", label: "Research", icon: "search", group: 0 },
  { route: "code", label: "Code", icon: "org-developer", group: 0 },
  { route: "library", label: "Library", icon: "folder", group: 0 },
  { route: "routines", label: "Routines", icon: "clock", group: 0 },
  { route: "models", label: "Models", icon: "ai-model", group: 1 },
  { route: "bringup", label: "Bring-up", icon: "ai-task", group: 1 },
  { route: "terminals", label: "Terminals", icon: "ai-agent", group: 1 },
  { route: "hardware", label: "Hardware", icon: "blackhole", group: 1 },
  { route: "discover", label: "What's new", icon: "globe", group: 2 },
  { route: "connect", label: "Connect", icon: "link", group: 2 },
  { route: "settings", label: "Settings", icon: "gear", group: 2 },
];

function routeFromPath(): Route {
  const r = location.pathname.replace(/^\/app\/?/, "").split("/")[0] as Route;
  // Orchard (Quick port) is one of Bring-up's ways; its old address still opens it there.
  return r === "orchard" ? "bringup" : PLACES.some((p) => p.route === r) ? r : "home";
}

/** Opens a place; `hash` picks something in it (a Code session, a run, a Settings section). */
export function navigate(route: Route, hash?: string) {
  const path = route === "orchard" ? "bringup#orchard" : route === "home" ? "" : route;
  history.pushState(null, "", `/app/${path}${hash ? `#${hash}` : ""}`);
  window.dispatchEvent(new PopStateEvent("popstate"));
  if (hash) window.dispatchEvent(new HashChangeEvent("hashchange"));
}

export function App() {
  const [ready, setReady] = useState<"loading" | "ok" | "token">("loading");
  useEffect(() => {
    signIn().then((ok) => setReady(ok ? "ok" : "token")).catch(() => setReady("token"));
  }, []);
  if (ready === "loading") return <div className="boot" />;
  if (ready === "token") return <TokenGate onDone={() => setReady("ok")} />;
  if (location.pathname.startsWith("/app/tori")) return <Suspense fallback={<div className="boot" />}><ToriWindow /></Suspense>;
  return <Shell />;
}

/**
 * Pick up new builds: the agent serves the app from disk, but an open window
 * keeps the screens it loaded. When the served build changes, reload -- right
 * away if the window is in the background, else the next time it's hidden or
 * after the person has been away from it for a minute.
 */
function useNewBuilds() {
  useEffect(() => {
    const mine = [...document.scripts].map((s) => s.src).find((src) => /\/assets\/index-/.test(src)) ?? "";
    let pending = false;
    const check = async () => {
      try {
        const html = await (await fetch("/app/", { cache: "no-store" })).text();
        const served = html.match(/\/app\/assets\/index-[^"']+\.js/)?.[0];
        if (served && mine && !mine.endsWith(served)) pending = true;
      } catch { /* box unreachable: try later */ }
      if (pending && (document.hidden || !document.hasFocus())) location.reload();
    };
    const t = window.setInterval(check, 20000);
    const onHide = () => { if (pending && document.hidden) location.reload(); };
    document.addEventListener("visibilitychange", onHide);
    return () => { window.clearInterval(t); document.removeEventListener("visibilitychange", onHide); };
  }, []);
}

function Shell() {
  useNewBuilds();
  const [route, setRoute] = useState<Route>(routeFromPath());
  useEffect(() => {
    const onPop = () => setRoute(routeFromPath());
    window.addEventListener("popstate", onPop);
    window.ttDesktop?.onNavigate((r) => {
      const [where, hash] = r.replace(/^\//, "").split("#");
      const [path, query] = where.split("?");
      navigate(path as Route);
      if (hash) history.replaceState(null, "", `/app/${path}#${hash}`);
      // Voice mode: Chat opens it when it mounts, or on this event if it's already open.
      if (query === "voice") {
        history.replaceState(null, "", `/app/${path}?voice`);
        window.dispatchEvent(new Event("tt:voice"));
      }
    });
    window.ttDesktop?.onBoxEvent((e) => addToast({ content: <><b>{e.title}</b><br />{e.body}</>, timeout: 6000 }));
    return () => window.removeEventListener("popstate", onPop);
  }, []);
  const devTools = useDevTools();
  const places = useMemo(() => PLACES.filter((p) => placeVisible(p.route, devTools)), [devTools]);
  // Keyboard: Ctrl+1…9 jumps to the first nine places shown.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey) || e.altKey) return;
      const n = Number(e.key);
      if (n >= 1 && n <= Math.min(9, places.length)) {
        e.preventDefault();
        navigate(places[n - 1].route);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [places]);
  // No choice made about developer tools yet: someone with bring-up runs on the box gets them.
  useEffect(() => {
    if (storedChoice()) return;
    void Promise.all([
      get<{ runs: unknown[] }>("/orchard").catch(() => null),
      get<{ runs: unknown[] }>("/bringup/codex/runs").catch(() => null),
    ]).then(([o, a]) => adoptFromBox((o?.runs?.length ?? 0) + (a?.runs?.length ?? 0) > 0));
  }, []);

  let page: ReactNode;
  switch (route) {
    case "chat": page = <Chat />; break;
    case "studio": page = <Studio />; break;
    case "research": page = <Research />; break;
    case "code": page = <Code />; break;
    case "terminals": page = <Terminals />; break;
    case "models": page = <Models />; break;
    case "bringup":
    case "orchard": page = <BringupPage />; break;
    case "routines": page = <Routines />; break;
    case "library": page = <Library />; break;
    case "inbox": page = <Inbox />; break;
    case "hardware": page = <Hardware />; break;
    case "discover": page = <Discover />; break;
    case "connect": page = <Connect />; break;
    case "settings": page = <Settings />; break;
    default: page = <Home />;
  }

  return (
    <div className="shell">
      <Sidebar route={route} places={places} />
      <main className="content" key={route}>
        <OfflineBanner />
        <RecordingBanner />
        <Suspense fallback={<div className="page-loading" role="status" aria-label="Loading" />}>{page}</Suspense>
      </main>
      <Toasts />
    </div>
  );
}

function Sidebar({ route, places }: { route: Route; places: typeof PLACES }) {
  const shortcut = useToriShortcut();
  const waiting = useInbox().items.length;
  return (
    <nav className="sidebar" aria-label="Places">
      <div className="brand">
        <Icon kind="tenstorrent" className="brand-mark" />
        <div className="grow" style={{ minWidth: 0 }}>
          <div className="brand-name">TT Station</div>
          {window.ttDesktop?.boxes ? <BoxSwitcher /> : <div className="brand-box">{boxName}</div>}
        </div>
      </div>
      {window.ttDesktop && (
        <button className="ask-tori" onClick={() => window.ttDesktop!.showTori("talk")} title={`The assistant, Tori: talk or type, from anywhere (${shortcut})`}
          aria-label={`Ask Tori, the assistant (${shortcut})`}>
          <span className="ask-tori-orb" aria-hidden />
          <span className="grow">Ask Tori</span>
          <span className="kbd" aria-hidden>{shortcut}</span>
        </button>
      )}
      <BoxTerminalButton />
      <div className="nav">
        {places.map((p, i) => (
          <div key={p.route}>
            {i > 0 && places[i - 1].group !== p.group && <div className="nav-gap" />}
            <button className={`nav-item ${route === p.route ? "active" : ""}`} onClick={() => navigate(p.route)}
              aria-current={route === p.route ? "page" : undefined} title={i < 9 ? `${p.label} (Ctrl+${i + 1})` : p.label}>
              <Icon kind={p.icon} className="nav-icon" aria-hidden />
              <span className="grow">{p.label}</span>
              {p.route === "inbox" && waiting > 0 && (
                <Badge as="span" size="sm" variant="warning" className="nav-count" aria-label={`${waiting} waiting`}>{waiting > 99 ? "99+" : waiting}</Badge>
              )}
            </button>
          </div>
        ))}
      </div>
      <BoxFooter />
    </nav>
  );
}

/** The bottom of the sidebar: what's serving and the chips, always in view. */
function BoxFooter() {
  const { data: serve } = useServe();
  const { chips, live } = useTelemetry(false);
  const { online } = useConnectivity();
  const stuck = serve?.phase === "starting" && !!serve.stalled_s;
  const state = !online ? "error" : !serve ? "queued" : serve.phase === "serving" ? "ready" : stuck ? "error" : serve.phase === "starting" ? "progress"
    : serve.phase === "failed" ? "error" : "queued";
  const label = !online ? "Can't reach the box" : !serve ? "Connecting" : serve.phase === "serving" ? short(serve.served_id ?? serve.model)
    : stuck ? `${short(serve.model)} looks stuck` : serve.phase === "starting" ? `Loading ${short(serve.model)}` : serve.phase === "failed" ? "Didn't start" : "Idle";
  const watts = chips.reduce((s, c) => s + (c.powerW ?? 0), 0);
  return (
    <div className="box-footer" {...pressable(() => navigate("hardware"))}
      aria-label={`${label}. ${chips.length ? `Chips: ${chips.map((c) => c.tempC != null ? `${Math.round(c.tempC)} °C, ${tempWord(c.tempC)}` : "no reading").join("; ")}. ` : ""}Open Hardware`}>
      <StatusIndicator label={label} state={state} animated={state === "progress"} />
      <div className="chip-dots" title={live ? "Chip temperatures" : "Chip readings aren't live"} aria-hidden>
        {chips.map((c) => (
          <div key={c.index} className="chip-dot" style={{ opacity: live ? 1 : 0.4 }} title={`Chip ${c.index}: ${tempWord(c.tempC)}`}>
            <span style={{ color: tempTone(c.tempC) }}>{c.tempC != null ? Math.round(c.tempC) : "–"}°</span>
            {c.tempC != null && c.tempC >= 78 && <span className="chip-hot" aria-hidden>{c.tempC >= 88 ? "!!" : "!"}</span>}
          </div>
        ))}
        {chips.length > 0 && <span className="chip-watts">{Math.round(watts)} W</span>}
      </div>
    </div>
  );
}

/** Only outside the desktop app (a browser on the box, for development). */
function TokenGate({ onDone }: { onDone: () => void }) {
  const [value, setValue] = useState("");
  return (
    <div className="gate">
      <Icon kind="tenstorrent" className="gate-mark" />
      <Typography variant="heading-md">Open TT Station from its app</Typography>
      <Typography variant="copy-sm" className="muted">
        The desktop app signs in by itself. To use these screens in a browser, paste a pairing token.
      </Typography>
      <TextInput value={value} onChange={(e) => setValue(e.target.value)} placeholder="Token" type="password" />
      <Button disabled={!value} onClick={() => { setToken(value.trim()); onDone(); }}>Continue</Button>
    </div>
  );
}

type Boxes = { current: string; boxes: { id: string; name: string; host: string; local: boolean }[] };

/**
 * Which box this window shows (desktop app only). Pairing lives in one place,
 * Settings → Devices and pairing; this menu only points there, and "Add
 * another box" opens the same connect screen a new computer starts on.
 */
function BoxSwitcher() {
  const [open, setOpen] = useState(false);
  const [list, setList] = useState<Boxes | null>(null);
  const load = () => { void window.ttDesktop?.boxes?.().then(setList); };
  useEffect(load, []);
  // The menu is drawn on top of the whole window (the sidebar's blur makes it
  // a layer of its own, which the page would otherwise cover), under the button.
  const button = useRef<HTMLButtonElement>(null);
  const [at, setAt] = useState<{ top: number; left: number } | null>(null);
  useLayoutEffect(() => {
    if (!open) return;
    const place = () => {
      const r = button.current?.getBoundingClientRect();
      if (r) setAt({ top: r.bottom + 8, left: Math.max(8, Math.min(r.left - 36, window.innerWidth - 316)) });
    };
    place();
    window.addEventListener("resize", place);
    return () => window.removeEventListener("resize", place);
  }, [open]);
  useEffect(() => {
    if (!open) return;
    const shut = () => setOpen(false);
    const close = (e: MouseEvent) => { if (!(e.target as HTMLElement).closest(".box-switcher, .box-switcher-menu")) shut(); };
    const esc = (e: KeyboardEvent) => { if (e.key === "Escape") { shut(); button.current?.focus(); } };
    window.addEventListener("mousedown", close);
    window.addEventListener("keydown", esc);
    return () => { window.removeEventListener("mousedown", close); window.removeEventListener("keydown", esc); };
  }, [open]);
  const toPairing = () => { setOpen(false); navigate("settings", "devices"); };
  const several = (list?.boxes.length ?? 0) > 1;
  return (
    <div className="box-switcher">
      <button ref={button} className="brand-box box-switcher-button" onClick={() => { setOpen((o) => !o); load(); }} aria-expanded={open} title={several ? "Switch box" : "Add another box"}>
        <span className="grow" style={{ overflow: "hidden", textOverflow: "ellipsis" }}>{boxName}</span>
        <Icon kind="caret-down" style={{ width: 12, height: 12, flex: "none" }} />
      </button>
      {open && at && createPortal(
        <div className="box-switcher-menu" role="menu" style={{ top: at.top, left: at.left }}>
          {list?.boxes.map((b) => (
            <button key={b.id} role="menuitemradio" aria-checked={b.id === list.current} className={`box-switcher-item ${b.id === list.current ? "on" : ""}`}
              onClick={() => { setOpen(false); if (b.id !== list.current) void window.ttDesktop!.switchBox!(b.id); }}>
              <span className="grow"><b>{b.name}</b><span className="muted"> · {b.host}</span></span>
              {b.id === list.current && <Icon kind="checkmark" style={{ width: 14, height: 14 }} />}
            </button>
          ))}
          <div className="box-switcher-gap" role="separator" />
          <button role="menuitem" className="box-switcher-item" onClick={() => { if (window.ttDesktop?.showConnect) { setOpen(false); void window.ttDesktop.showConnect(); } else toPairing(); }}>
            <Icon kind="add" style={{ width: 14, height: 14 }} aria-hidden /> Add another box…
          </button>
          <button role="menuitem" className="box-switcher-item" onClick={toPairing}>
            <Icon kind="phone" style={{ width: 14, height: 14 }} aria-hidden /> Pair a phone or computer with {boxName}…
          </button>
        </div>,
        document.body,
      )}
    </div>
  );
}

/** On another computer (a Mac): the box's command line in this computer's Terminal, over SSH. */
function BoxTerminalButton() {
  const [show, setShow] = useState(false);
  const [busy, setBusy] = useState(false);
  useEffect(() => { void onAnotherComputer().then(setShow); }, []);
  if (!show) return null;
  const open = async () => {
    setBusy(true);
    const why = await openBoxShell("terminal");
    setBusy(false);
    if (why) toastError(new Error(why));
  };
  return (
    <button className="box-terminal" onClick={open} disabled={busy} title={`${boxName}'s command line, in Terminal on this computer (SSH)${window.ttDesktop?.platform === "darwin" ? " · ⇧⌘T" : ""}`}>
      <span className="box-terminal-glyph" aria-hidden>›_</span>
      <span className="grow">{busy ? "Opening…" : "Open Terminal"}</span>
    </button>
  );
}
