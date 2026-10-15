// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Hardware: the four Blackhole chips, live and over the last hour; the chips
// as tt-toplike draws them; who holds which board (gozer); the serving log;
// and the silicon check: gpt-oss straight on the chips with tt-lab.

import { useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Badge } from "@tenstorrent/vesper/badge";
import { Select } from "@tenstorrent/vesper/select";
import { TextArea } from "@tenstorrent/vesper/text-area";
import { Switch } from "@tenstorrent/vesper/switch";
import { Typography } from "@tenstorrent/vesper/typography";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { authHeader, type Chip, get, short } from "../api";
import { Card, Eyebrow, Page, Spark, tempTone, toastError, usePoll, useTelemetry } from "../ui";
import { TopTerm } from "../TopTerm";
import { askToStop } from "../dialog";

interface HistoryPoint { t: number; chips: { temp_c: number | null; power_w: number | null }[]; gen_tps: number | null }
interface Lease { chip: number; board: string; state: string; who: string | null; reason: string | null }

/** "14:05" for a unix-seconds timestamp, for the chart hover tooltips. */
const at = (t: number) => new Date(t * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

export function Hardware() {
  const { chips, live, error } = useTelemetry(true);
  const history = usePoll(() => get<{ points: HistoryPoint[]; step_s: number }>("/metrics/history?range=1h"), 15000);
  const leases = usePoll(() => get<{ available: boolean; leases: Lease[] }>("/leases"), 10000);
  const logs = usePoll(() => get<{ lines: string[] }>("/logs?tail=60"), 8000);
  const points = history.data?.points ?? [];
  const times = points.map((p) => at(p.t));
  const watts = chips.reduce((s, c) => s + (c.powerW ?? 0), 0);

  return (
    <Page title="Hardware" subtitle="QuietBox 2 · two p300c boards · four Blackhole chips">
      {error && <Admonition variant="warning" subtle size="sm">Chip readings aren't live: {error}</Admonition>}

      <div className="grid grid-4">
        {chips.map((c) => <ChipPanel key={c.index} chip={c} live={live}
          temps={points.map((p) => p.chips[c.index]?.temp_c ?? NaN)} power={points.map((p) => p.chips[c.index]?.power_w ?? NaN)} times={times} />)}
      </div>

      <Card>
        <div className="row-between"><Eyebrow>tt-toplike</Eyebrow><span className="muted mono" style={{ fontSize: 12 }}>{Math.round(watts)} W now</span></div>
        <TopTerm />
      </Card>

      <div className="grid grid-2">
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
        <Card>
          <div className="row-between"><Eyebrow>Who has the chips</Eyebrow>{leases.data?.available === false && <Badge size="sm" subtle variant="contrast">gozer not installed</Badge>}</div>
          <div className="list">
            {(leases.data?.leases ?? []).map((l) => (
              <div key={l.chip} className="list-row">
                <span className="mono" style={{ width: 54 }}>chip {l.chip}</span>
                <Badge size="sm" subtle variant={l.state === "FREE" ? "mint" : l.state.startsWith("HELD") ? "info" : "warning"}>{l.state.toLowerCase()}</Badge>
                <span className="grow muted" style={{ fontSize: 12 }}>{l.reason ?? l.who ?? ""}</span>
              </div>
            ))}
          </div>
        </Card>
      </div>

      <div>
        <Card>
          <Eyebrow>Serving log</Eyebrow>
          <div className="console" style={{ maxHeight: 260 }}>{(logs.data?.lines ?? []).join("\n") || "No output."}</div>
        </Card>
      </div>

      <Lab />
    </Page>
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
        <Button size="sm" disabled={running || !lab.data?.models.find((m) => m.id === model)?.ready} onClick={() => run()}>{running ? "Checking…" : "Run the check"}</Button>
      </div>
      <TextArea value={prompt} onChange={(e) => setPrompt(e.target.value)} height={64} />
      {(output || running) && <div className="console">{output || "Resetting the chips and loading the firmware…"}</div>}
      {lab.data?.serving && <span className="muted" style={{ fontSize: 12 }}>The silicon check needs every chip: running it stops {short(lab.data.serving)} first.</span>}
    </Card>
  );
}
