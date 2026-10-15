// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Hardware: the chips, live and over the last hour; the chips as tt-toplike
// draws them; energy use (the one place for it: Home shows today's and links
// here, `#energy`); the serving log; and, with developer tools on, who holds which
// chips (the chip scheduler, gozer) and the silicon check (gpt-oss straight on
// the chips with tt-lab).

import { useEffect, useRef, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Badge } from "@tenstorrent/vesper/badge";
import { Select } from "@tenstorrent/vesper/select";
import { TextArea } from "@tenstorrent/vesper/text-area";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Switch } from "@tenstorrent/vesper/switch";
import { Typography } from "@tenstorrent/vesper/typography";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { authHeader, boxName, type Chip, get, hardwareLabel, type MetricsNow, short, type Status } from "../api";
import { Card, Eyebrow, Page, Spark, Stat, tempTone, tempWord, toastError, usePoll, useTelemetry } from "../ui";
import { useDevTools } from "../devtools";
import { TopTerm } from "../TopTerm";
import { askToStop } from "../dialog";
import { OWNER_ONLY_HOST, useOwner } from "../role";

interface HistoryPoint { t: number; chips: { temp_c: number | null; power_w: number | null }[]; gen_tps: number | null }
interface Lease { chip: number; board: string; state: string; who: string | null; reason: string | null }

/** "14:05" for a unix-seconds timestamp, for the chart hover tooltips. */
const at = (t: number) => new Date(t * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

export function Hardware() {
  const { chips, live, error } = useTelemetry(true);
  const history = usePoll(() => get<{ points: HistoryPoint[]; step_s: number }>("/metrics/history?range=1h"), 15000);
  const devTools = useDevTools();
  const leases = usePoll(() => (devTools ? get<{ available: boolean; leases: Lease[] }>("/leases") : Promise.resolve(null)), devTools ? 10000 : 600000, [devTools]);
  const status = usePoll(() => get<Status>("/status"), 5 * 60 * 1000);
  const logs = usePoll(() => get<{ lines: string[] }>("/logs?tail=60"), 8000);
  const points = history.data?.points ?? [];
  const times = points.map((p) => at(p.t));
  const watts = chips.reduce((s, c) => s + (c.powerW ?? 0), 0);

  return (
    <Page title="Hardware" subtitle={[boxName, hardwareLabel(status.data)].filter(Boolean).join(" · ")}>
      {error && <Admonition variant="warning" subtle size="sm">Chip readings aren't live: {error}</Admonition>}

      <div className="grid grid-4">
        {chips.map((c) => <ChipPanel key={c.index} chip={c} live={live}
          temps={points.map((p) => p.chips[c.index]?.temp_c ?? NaN)} power={points.map((p) => p.chips[c.index]?.power_w ?? NaN)} times={times} />)}
      </div>

      <Card>
        <div className="row-between"><Eyebrow>tt-toplike</Eyebrow><span className="muted mono" style={{ fontSize: 12 }}>{Math.round(watts)} W now</span></div>
        <TopTerm />
      </Card>

      <div className={devTools ? "grid grid-2" : ""}>
        <Card>
          <Eyebrow>Last hour</Eyebrow>
          <div className="stack" style={{ gap: 18 }}>
            <div className="stack" style={{ gap: 6 }}>
              <span className="muted" style={{ fontSize: 12 }}>Box power</span>
              <Spark title="Box power" axis values={points.map((p) => p.chips.length ? p.chips.reduce((s, c) => s + (c.power_w ?? 0), 0) : NaN)} labels={times} height={70} format={(v) => `${v.toFixed(0)} W`} />
            </div>
            <div className="stack" style={{ gap: 6 }}>
              <span className="muted" style={{ fontSize: 12 }}>Hottest chip</span>
              <Spark title="Hottest chip" axis values={points.map((p) => p.chips.length ? Math.max(...p.chips.map((c) => c.temp_c ?? -Infinity)) : NaN)} labels={times} color="var(--vesper-amber-600)" height={70} format={(v) => `${v.toFixed(1)} °C`} />
            </div>
            <div className="stack" style={{ gap: 6 }}>
              <span className="muted" style={{ fontSize: 12 }}>Generation speed</span>
              <Spark title="Generation speed" axis values={points.map((p) => p.gen_tps ?? 0)} labels={times} color="var(--vesper-mint-600)" height={50} format={(v) => `${v.toFixed(1)} tok/s`} />
            </div>
          </div>
        </Card>
        {devTools && <Card>
          <div className="row-between"><Eyebrow>Who has the chips</Eyebrow>{leases.data?.available === false && <Badge size="sm" subtle variant="contrast">Chip scheduler not installed</Badge>}</div>
          <Typography variant="copy-xs" className="muted">The chip scheduler (gozer) hands chips to one job at a time, so two never collide.</Typography>
          <div className="list">
            {(leases.data?.leases ?? []).map((l) => (
              <div key={l.chip} className="list-row">
                <span className="mono" style={{ width: 54 }}>chip {l.chip}</span>
                <Badge size="sm" subtle variant={l.state === "FREE" ? "mint" : l.state.startsWith("HELD") ? "info" : "warning"}>{l.state.toLowerCase()}</Badge>
                <span className="grow muted" style={{ fontSize: 12 }}>{l.reason ?? l.who ?? ""}</span>
              </div>
            ))}
          </div>
        </Card>}
      </div>

      <Energy watts={watts} />

      <div>
        <Card>
          <Eyebrow>Serving log</Eyebrow>
          <div className="console" style={{ maxHeight: 260 }}>{(logs.data?.lines ?? []).join("\n") || "No output."}</div>
        </Card>
      </div>

      {devTools && <Lab />}
    </Page>
  );
}

interface EnergyReport { months: { month: string; energy_kwh: number; tokens: number }[]; days: { day: string; energy_kwh: number }[] }

const PRICE_KEY = "tt.price";
function loadPrice(): number {
  try { const v = Number(localStorage.getItem(PRICE_KEY) ?? "0.15"); return Number.isFinite(v) && v >= 0 ? v : 0.15; } catch { return 0.15; }
}

/** Energy use: today, this month with what it costs, and each day of the last month. */
function Energy({ watts }: { watts: number }) {
  const report = usePoll(() => get<EnergyReport>("/energy"), 60000);
  const metrics = usePoll(() => get<MetricsNow>("/metrics/now"), 15000);
  const [price, setPrice] = useState(loadPrice);
  useEffect(() => { try { localStorage.setItem(PRICE_KEY, String(price)); } catch { /* private */ } }, [price]);
  const ref = useRef<HTMLDivElement>(null);
  // Opened as /app/hardware#energy (from Home or Settings): bring it into view.
  useEffect(() => { if (location.hash === "#energy") ref.current?.scrollIntoView({ block: "start" }); }, []);
  const t = metrics.data?.totals;
  const month = report.data?.months.at(-1);
  const kwh = month?.energy_kwh ?? 0;
  return (
    <div id="energy" ref={ref} className="anchor">
      <Card>
        <div className="row-between"><Eyebrow>Energy</Eyebrow><span className="muted mono" style={{ fontSize: 12 }}>{Math.round(watts)} W now</span></div>
        <div className="grid grid-3">
          <Stat label="Today" value={((t?.energy_wh_today ?? 0) / 1000).toFixed(2)} unit="kWh" sub={`≈ $${(((t?.energy_wh_today ?? 0) / 1000) * price).toFixed(2)}`} />
          <Stat label="This month" value={kwh.toFixed(1)} unit="kWh" sub={`≈ $${(kwh * price).toFixed(2)}`} />
          <Stat label="All time" value={((t?.energy_wh_all ?? 0) / 1000).toFixed(1)} unit="kWh" />
        </div>
        <Spark title="Energy per day" axis values={(report.data?.days ?? []).map((d) => d.energy_kwh)} labels={(report.data?.days ?? []).map((d) => d.day)} height={60} format={(v) => `${v.toFixed(1)} kWh`} />
        <div className="row-between wrap" style={{ gap: 12 }}>
          <span className="muted" style={{ fontSize: 12 }}>Each day over the last month, from the chips' own power readings.</span>
          <label className="row" style={{ gap: 8, fontSize: 13 }}>
            <span className="muted">Price per kWh ($)</span>
            <div style={{ width: 100 }}><TextInput size="sm" type="number" min={0} step={0.01} value={String(price)} aria-label="Price per kWh, in dollars"
              onChange={(e) => setPrice(Math.max(0, Number(e.target.value) || 0))} /></div>
          </label>
        </div>
      </Card>
    </div>
  );
}

function ChipPanel({ chip: c, live, temps, power, times }: { chip: Chip; live: boolean; temps: number[]; power: number[]; times: string[] }) {
  const throttle = c.throttleC ?? 90;
  return (
    <Card className="" >
      <div className="row-between"><Eyebrow>Chip {c.index}</Eyebrow><span className="muted mono" style={{ fontSize: 11 }}>{(c.board ?? "").toUpperCase()}</span></div>
      <div style={{ opacity: live ? 1 : 0.5 }}>
        <div className="chip-temp" style={{ color: tempTone(c.tempC), fontSize: 34 }}>
          {c.tempC != null ? c.tempC.toFixed(0) : "–"}<span className="stat-unit">°C</span>
        </div>
        <Typography variant="copy-xs" className="muted">{tempWord(c.tempC)}</Typography>
        <div className="bar" style={{ margin: "8px 0" }}>
          <i style={{ width: `${Math.min(100, Math.max(0, (((c.tempC ?? 25) - 25) / (throttle - 25)) * 100))}%`, background: tempTone(c.tempC) }} />
        </div>
        <div className="muted" style={{ fontSize: 12 }}>
          Memory {c.memTempC ?? "–"} °C · slows down at {throttle} °C
        </div>
      </div>
      <div className="grid grid-2" style={{ gap: 10 }}>
        <div><div className="eyebrow">Power</div><div className="mono">{c.powerW?.toFixed(0) ?? "–"} W</div></div>
        <div><div className="eyebrow">Clock</div><div className="mono">{c.aiclk ?? "–"} MHz</div></div>
        <div><div className="eyebrow">Current</div><div className="mono">{c.currentA?.toFixed(0) ?? "–"} A</div></div>
        <div><div className="eyebrow">Memory</div><div className="ddr" title="GDDR channels trained">{c.ddrTrained.map((on, i) => <i key={i} className={on ? "on" : ""} />)}</div></div>
      </div>
      <Spark title={`Chip ${c.index} power`} values={power} labels={times} height={28} format={(v) => `${v.toFixed(0)} W`} />
      <Spark title={`Chip ${c.index} temperature`} values={temps} labels={times} color="var(--vesper-amber-600)" height={20} format={(v) => `${v.toFixed(0)} °C`} />
    </Card>
  );
}

interface LabStatus { installed: boolean; serving: string | null; running: unknown; models: { id: string; title: string; ready: boolean; tiles: number[] }[] }

function Lab() {
  const owner = useOwner();
  const lab = usePoll(() => get<LabStatus>("/lab"), 20000);
  const [model, setModel] = useState("gpt-oss-20b");
  const [prompt, setPrompt] = useState("Explain what a Tensix core is in two sentences.");
  const [check, setCheck] = useState(false);
  const [output, setOutput] = useState("");
  const [result, setResult] = useState<{ ok: boolean; seconds: number } | null>(null);
  const [running, setRunning] = useState(false);
  if (!lab.data?.installed) return null;

  const run = async (stopServing = false) => {
    setRunning(true);
    setOutput("");
    setResult(null);
    try {
      const m = lab.data!.models.find((x) => x.id === model)!;
      const res = await fetch("/lab/run", {
        method: "POST",
        headers: { "content-type": "application/json", ...authHeader() },
        body: JSON.stringify({ model, prompt, tokens: check ? 8 : 128, tiles: m.tiles[0], check, profile: false, stop_serving: stopServing }),
      });
      if (res.status === 409) {
        const msg = (await res.json()).error as string;
        setRunning(false);
        if (/Stop/.test(msg)) { if (await askToStop(msg, "It can be started again afterwards.")) run(true); }
        else toastError(new Error(msg));
        return;
      }
      const reader = res.body!.getReader();
      const dec = new TextDecoder();
      let buf = "";
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        buf += dec.decode(value, { stream: true });
        let i: number;
        while ((i = buf.indexOf("\n")) >= 0) {
          const line = buf.slice(0, i).trim();
          buf = buf.slice(i + 1);
          if (!line.startsWith("data:")) continue;
          const e = JSON.parse(line.slice(5));
          if (e.type === "text") setOutput((o) => o + e.text);
          if (e.type === "done") setResult({ ok: e.ok, seconds: e.seconds });
        }
      }
    } catch (e) {
      toastError(e);
    } finally {
      setRunning(false);
      lab.refresh();
    }
  };

  return (
    <Card>
      <div className="row-between">
        <div><Eyebrow>Silicon check (tt-lab)</Eyebrow><Typography variant="copy-sm" className="muted">Runs gpt-oss straight on the chips with tt-lab (its own firmware, no TT-Metal) and can check every output bit for bit against a host-side copy of the math, to show the hardware computes exactly what it should. It takes every chip, so a serving model is stopped first.</Typography></div>
        {result && <Badge variant={result.ok ? "mint" : "danger"} subtle>{result.ok ? "passed" : "failed"} · {result.seconds.toFixed(1)} s</Badge>}
      </div>
      <div className="row">
        <Select size="sm" value={model} onValueChange={(v) => setModel(v ?? model)}
          options={(lab.data?.models ?? []).map((m) => ({ label: `${m.title}${m.ready ? "" : " (not prepared)"}`, value: m.id }))} />
        <Switch size="sm" label="Bit-exact check" checked={check} onChange={(e) => setCheck(e.target.checked)} />
        <div className="grow" />
        <Button size="sm" disabled={running || !owner || !lab.data?.models.find((m) => m.id === model)?.ready} title={owner ? undefined : OWNER_ONLY_HOST} onClick={() => run()}>{running ? "Checking…" : "Run the check"}</Button>
      </div>
      <TextArea value={prompt} onChange={(e) => setPrompt(e.target.value)} height={64} />
      {(output || running) && <div className="console">{output || "Resetting the chips and loading the firmware…"}</div>}
      {lab.data?.serving && <span className="muted" style={{ fontSize: 12 }}>The silicon check needs every chip: running it stops {short(lab.data.serving)} first.</span>}
    </Card>
  );
}
