// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Check-ups (`evals.rs`): how good the serving model is at what this box uses
// it for -- tool use, code, reasoning, following instructions, long documents
// and speed -- graded automatically, kept per model, and compared: which model
// to use for Chat, Code and Research.

import { useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Badge } from "@tenstorrent/vesper/badge";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { Icon } from "@tenstorrent/vesper/icons";
import { Typography } from "@tenstorrent/vesper/typography";
import { Sheet, useSheet } from "@tenstorrent/vesper/sheet";
import { ago, del, get, post } from "../api";
import { Card, Empty, Eyebrow, toastError, usePoll } from "../ui";
import { ask } from "../dialog";
import { navigate } from "../App";

interface Case { name: string; ok: boolean; detail: string; seconds: number }
interface Category { key: string; label: string; passed: number; total: number; cases: Case[] }
interface Speed { ttft_s: number | null; single_tps: number | null; batch_tps: number | null; wh_per_million_tokens: number | null }
interface Eval { id: string; model: string; started: number; finished: number | null; status: string; context: number | null; speed: Speed; categories: Category[]; overall: number | null; activity: string | null; error: string | null }
interface Pick { use_for: string; model: string; why: string }
interface Data { evals: Eval[]; running: Eval | null; recommendations: Pick[]; serving: string | null }

const COLUMNS: [string, string][] = [["tools", "Tool use"], ["code", "Code"], ["reasoning", "Reasoning"], ["instructions", "Instructions"], ["long", "Long docs"]];
const short = (m: string) => m.split("/").pop() ?? m;
const pct = (x: number) => `${Math.round(x * 100)}%`;

function Score({ c }: { c: Category | undefined }) {
  if (!c) return <span className="muted">–</span>;
  const s = c.total ? c.passed / c.total : 0;
  return <span className={`eval-score ${s >= 0.85 ? "good" : s >= 0.6 ? "fair" : "poor"}`} title={`${c.passed} of ${c.total}`}>{c.passed}/{c.total}</span>;
}

export function Evals() {
  const data = usePoll(() => get<Data>("/evals"), 3000);
  const d = data.data;
  const sheet = useSheet();
  const [open, setOpen] = useState<Eval | null>(null);
  const running = d?.running?.status === "running" ? d.running : null;

  // The latest finished check-up of each model, best first.
  const latest = new Map<string, Eval>();
  for (const e of d?.evals ?? []) if (e.status === "done" && !latest.has(e.model)) latest.set(e.model, e);
  const rows = [...latest.values()].sort((a, b) => (b.overall ?? 0) - (a.overall ?? 0));
  const failed = (d?.evals ?? []).find((e) => e.status !== "done");

  const start = async () => {
    try { await post("/evals"); void data.refresh(); } catch (e) { toastError(e); }
  };
  const stop = async () => { try { await post("/evals/stop"); } catch (e) { toastError(e); } };
  const remove = async (e: Eval) => {
    if (!(await ask(`Delete this check-up of ${short(e.model)}?`, { confirm: "Delete", danger: true }))) return;
    try { await del(`/evals/${e.id}`); sheet.close(); void data.refresh(); } catch (err) { toastError(err); }
  };
  const show = (e: Eval) => { setOpen(e); sheet.open(); };

  return (
    <div className="stack" style={{ gap: 16 }}>
      <Card className="eval-head">
        <div className="stack" style={{ gap: 4, minWidth: 0 }}>
          <Typography variant="heading-xs">Model check-ups</Typography>
          <span className="muted" style={{ fontSize: 13 }}>
            About 50 graded tests of what the box uses a model for: tool use, writing code that passes tests, reasoning, following instructions and finding facts in long documents, plus its speed. Takes a minute or two.
          </span>
        </div>
        {running ? (
          <Button variant="subtle" onClick={stop}>Stop</Button>
        ) : (
          <Button disabled={!d?.serving} onClick={start} title={d?.serving ? undefined : "Start a chat model first"}>
            {d?.serving ? `Check up on ${short(d.serving)}` : "Nothing is serving"}
          </Button>
        )}
      </Card>

      {data.error && <Admonition variant="warning" subtle size="sm">Couldn't load check-ups: {data.error}</Admonition>}

      {running && (
        <Card className="eval-running">
          <div className="row" style={{ gap: 10 }}>
            <Icon kind="spinner" className="lib-spin" />
            <span><strong>{short(running.model)}</strong> · {running.activity ?? "Working"}</span>
          </div>
          <div className="eval-steps">
            {["Speed", ...COLUMNS.map((c) => c[1])].map((label, i) => {
              const done = i === 0 ? running.speed.single_tps != null || running.categories.length > 0 : running.categories.length >= i;
              const c = i > 0 ? running.categories[i - 1] : undefined;
              return (
                <span key={label} className={`eval-step ${done ? "done" : ""}`}>
                  {label}{c ? ` ${c.passed}/${c.total}` : i === 0 && running.speed.single_tps ? ` ${running.speed.single_tps.toFixed(0)} tok/s` : ""}
                </span>
              );
            })}
          </div>
        </Card>
      )}

      {failed && !running && failed === d?.evals[0] && failed.error && (
        <Admonition variant="warning" subtle size="sm">The last check-up of {short(failed.model)} didn't finish: {failed.error}</Admonition>
      )}

      {d && d.recommendations.length > 0 && (
        <div className="stack" style={{ gap: 8 }}>
          <Eyebrow>What to use</Eyebrow>
          <div className="eval-picks">
            {d.recommendations.map((p) => (
              <Card key={p.use_for} className="eval-pick">
                <span className="muted" style={{ fontSize: 12 }}>For {p.use_for}</span>
                <span className="eval-pick-model" title={p.model}>{short(p.model)}</span>
                <span className="muted" style={{ fontSize: 12 }}>{p.why}</span>
              </Card>
            ))}
          </div>
          {rows.length === 1 && (
            <span className="muted" style={{ fontSize: 12 }}>
              Only one model checked so far. Check up on others to compare them, or let a routine do it overnight.{" "}
              <button className="link-button" onClick={() => navigate("routines")}>Routines</button>
            </span>
          )}
        </div>
      )}

      {d && rows.length === 0 && !running && (
        <Empty title="No check-ups yet">{d.serving ? `Run one on ${short(d.serving)} to see how it does.` : "Start a chat model, then run one."}</Empty>
      )}

      {rows.length > 0 && (
        <div className="stack" style={{ gap: 8 }}>
          <Eyebrow>Results</Eyebrow>
          <div className="eval-table-wrap">
            <table className="eval-table">
              <thead>
                <tr>
                  <th>Model</th>
                  {COLUMNS.map(([k, l]) => <th key={k}>{l}</th>)}
                  <th title="One person's answer speed">Speed</th>
                  <th title="Time to first token">First word</th>
                  <th>Overall</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {rows.map((e) => (
                  <tr key={e.id} onClick={() => show(e)}>
                    <td>
                      <span className="eval-model" title={e.model}>{short(e.model)}</span>
                      {e.model === d?.serving && <Badge size="sm" variant="mint" subtle>Serving</Badge>}
                    </td>
                    {COLUMNS.map(([k]) => <td key={k}><Score c={e.categories.find((c) => c.key === k)} /></td>)}
                    <td className="mono">{e.speed.single_tps ? `${e.speed.single_tps.toFixed(0)} tok/s` : "–"}</td>
                    <td className="mono">{e.speed.ttft_s != null ? `${e.speed.ttft_s.toFixed(1)} s` : "–"}</td>
                    <td><strong>{e.overall != null ? pct(e.overall) : "–"}</strong></td>
                    <td className="muted" style={{ fontSize: 12, whiteSpace: "nowrap" }}>{ago(e.finished ?? e.started)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}

      <Sheet ref={sheet.ref} popover title={open ? short(open.model) : ""} description={open ? `Checked ${ago(open.finished ?? open.started)}${open.context ? ` · ${Math.round(open.context / 1024)}K context` : ""}` : ""}>
        {open && (
          <div className="stack" style={{ gap: 16 }}>
            <div className="eval-speed">
              <span><strong>{open.speed.single_tps?.toFixed(0) ?? "–"}</strong> tok/s for one person</span>
              <span><strong>{open.speed.batch_tps?.toFixed(0) ?? "–"}</strong> tok/s for eight at once</span>
              <span><strong>{open.speed.ttft_s?.toFixed(2) ?? "–"}</strong> s to the first word</span>
              {open.speed.wh_per_million_tokens != null && <span><strong>{open.speed.wh_per_million_tokens.toFixed(0)}</strong> Wh per million tokens</span>}
            </div>
            {open.categories.map((c) => (
              <div key={c.key} className="stack" style={{ gap: 4 }}>
                <div className="row" style={{ gap: 8 }}><strong>{c.label}</strong><Score c={c} /></div>
                {c.cases.map((x) => (
                  <div key={x.name} className={`eval-case ${x.ok ? "ok" : "no"}`}>
                    <Icon kind={x.ok ? "success" : "circle-x"} />
                    <span className="eval-case-name">{x.name}</span>
                    <span className="muted eval-case-detail" title={x.detail}>{x.detail}</span>
                  </div>
                ))}
              </div>
            ))}
            <div><Button size="sm" variant="ghost" onClick={() => remove(open)}>Delete this check-up</Button></div>
          </div>
        )}
      </Sheet>
    </div>
  );
}
