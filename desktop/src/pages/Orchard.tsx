// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Orchard, one of the ways to bring up a model (see BringupWays): tt-orchard
// grafts a new model's weights onto an architecture TT-Metal already runs,
// with its own agents on models served on this box (no account). This is its
// part of the Bring-up page: starting a run (its checks, the base model to
// build on, credentials), a test box that runs every hardware test so the coder
// on this box is never parked (`--lab`), and the runs (OrchardRun.tsx).

import { useEffect, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { Badge } from "@tenstorrent/vesper/badge";
import { Select } from "@tenstorrent/vesper/select";
import { Switch } from "@tenstorrent/vesper/switch";
import { Icon } from "@tenstorrent/vesper/icons";
import { ApiError, get, type OrchardLab, type OrchardStatus, post, put, short } from "../api";
import { Card, Eyebrow, toastError, toastOk, usePoll } from "../ui";
import { ask, askToStop } from "../dialog";
import { OrchardRunView } from "./OrchardRun";

export const ORCHARD_STATE: Record<string, [string, string]> = {
  preparing: ["Starting", "Checking the model and downloading it."],
  running: ["Running", "Working through the stages."],
  paused: ["Paused", "Holding the chips until you resume."],
  "ready-for-operator-review": ["Ready for review", "Every stage passed. Review it, then serve it."],
  blocked: ["Blocked", "It stopped and said why."],
  aborted: ["Stopped", "Stopped before finishing. Retry picks it up."],
  "stopped-or-crashed": ["Stopped on an error", "It hit an error. Retry picks it up."],
  refused: ["Didn't start", "tt-orchard's checks refused it."],
};

/** Bring-up hands a model over here when Orchard is the better fit. */
export const HANDOFF_KEY = "tt.orchard.model";

const statusIcon = (s: string) => (s === "ok" ? "success" : s === "warn" || s === "todo" ? "warning" : "error");
const statusColor = (s: string) => (s === "ok" ? "var(--vesper-mint-600)" : s === "warn" || s === "todo" ? "var(--vesper-amber-600)" : "var(--vesper-red-600)");

/** Orchard's part of the Bring-up page. */
export function OrchardBody() {
  const status = usePoll(() => get<OrchardStatus>("/orchard").catch(() => null), 6000);
  const [selected, setSelected] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  const [handoff] = useState<string | null>(() => {
    try { const m = localStorage.getItem(HANDOFF_KEY); localStorage.removeItem(HANDOFF_KEY); return m; } catch { return null; }
  });
  const s = status.data;
  const runs = [...(s?.runs ?? [])].sort((a, b) => Number(b.live) - Number(a.live));
  const pick = runs.find((r) => r.run_name === selected) ?? runs.find((r) => r.live) ?? runs[0] ?? null;
  const lab = s?.config?.lab ?? null;
  const showStart = (starting || runs.length === 0 || !!handoff) && !s?.busy;

  return (
    <>
      {!s && <span className="muted">Looking for tt-orchard…</span>}
      {s && !s.installed && <Admonition variant="info" subtle>tt-orchard isn't on this box yet. Set it up below.</Admonition>}
      {s && !s.ready && <OrchardSetup s={s} onChanged={status.refresh} />}
      {s?.notes?.map((n) => <Admonition key={n.text} variant="info" subtle size="sm">{n.text}{n.command ? <> <code>{n.command}</code></> : null}</Admonition>)}

      {s?.ready && showStart && (
        <StartOrchard initial={handoff ?? ""} packaging={s.packaging} lab={lab} canClose={runs.length > 0}
          onClose={() => setStarting(false)} onStarted={() => { setStarting(false); status.refresh(); }} />
      )}

      {runs.length > 0 && (
        <>
          <div className="row-between wrap">
            <div className="run-chips" role="tablist" aria-label="Orchard runs">
              {runs.map((r) => (
                <button key={r.run_name} role="tab" aria-selected={pick?.run_name === r.run_name} className={`run-chip ${pick?.run_name === r.run_name ? "on" : ""}`} onClick={() => setSelected(r.run_name)}>
                  <span className={`run-dot ${r.live ? "live" : r.state === "ready-for-operator-review" ? "done" : "stopped"}`} />
                  <b>{short(r.model ?? r.run_name)}</b>
                  <span className="muted">{(ORCHARD_STATE[r.state]?.[0] ?? r.state).toLowerCase()}</span>
                </button>
              ))}
            </div>
            {!showStart && !s?.busy && s?.ready && <Button size="sm" iconLeft={<Icon kind="add" />} onClick={() => setStarting(true)}>Graft another model</Button>}
          </div>
          {pick && <OrchardRunView key={pick.run_name} initial={pick} onChanged={status.refresh} onDeleted={() => { setSelected(null); status.refresh(); }} />}
        </>
      )}

      {s?.ready && <LabCard lab={lab} packaging={s.packaging} onChanged={status.refresh} />}
    </>
  );
}

function OrchardSetup({ s, onChanged }: { s: OrchardStatus; onChanged: () => void }) {
  return (
    <Card>
      <div className="row-between"><b>Orchard needs setting up first</b>
        {s.setup_running ? <Badge variant="info" subtle>Setting up…</Badge>
          : <Button size="sm" onClick={async () => { try { await post("/orchard/setup"); onChanged(); } catch (e) { toastError(e); } }}>Set it up</Button>}
      </div>
      <div className="list">
        {s.setup.filter((x) => x.status !== "ok").map((step) => (
          <div key={step.name} className="list-row"><b>{step.name}</b><span className="muted" style={{ fontSize: 12 }}>{step.detail}</span></div>
        ))}
      </div>
      {s.config_error && <Admonition variant="warning" subtle size="sm">{s.config_error}</Admonition>}
    </Card>
  );
}

// ---------------------------------------------------------------- start

interface Check { name: string; status: string; detail: string; reason: string | null; candidates: { name: string; installed?: boolean }[] | null }
interface Preflight { model: string; run_name: string; resuming: boolean; local: boolean; download_gb: number; license: string | null; checks: Check[]; blocked: boolean; lab: OrchardLab | null }

/** The select's value for "no --base": the model card's base_model. */
const CARD_BASE = "card-base";

/** A model, Orchard's own checks, the base to build on, then one Start. */
function StartOrchard({ initial, packaging, lab, canClose, onClose, onStarted }: {
  initial: string; packaging: boolean; lab: OrchardLab | null; canClose: boolean; onClose: () => void; onStarted: () => void;
}) {
  const [model, setModel] = useState(initial);
  const [pre, setPre] = useState<Preflight | null>(null);
  const [checking, setChecking] = useState(false);
  const [base, setBase] = useState("");
  const [refuse, setRefuse] = useState(false);
  const [showChecks, setShowChecks] = useState(false);

  const check = async (opts: { base?: string; refuse?: boolean } = {}) => {
    const m = model.trim();
    if (!m) return;
    setChecking(true);
    try {
      setPre(await post<Preflight>("/orchard/preflight", { model: m, base: (opts.base ?? base) || undefined, refuse_credentials_visible: opts.refuse ?? refuse }));
    } catch (e) { toastError(e); setPre(null); } finally { setChecking(false); }
  };
  useEffect(() => { if (initial) void check(); }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const start = async (stopServing = false) => {
    if (!pre) return;
    try {
      await post("/orchard/runs", { model: pre.model, stop_serving: stopServing, base: base || undefined, refuse_credentials_visible: refuse });
      toastOk(`Orchard is bringing up ${short(pre.model)}${lab ? `, testing on ${lab.host}` : ""}. You'll get a notification at each stage.`);
      onStarted();
    } catch (e) {
      if (e instanceof ApiError && e.status === 409 && (e.body as { needs_stop?: boolean })?.needs_stop) {
        if (await askToStop(e.message, "It starts again when the bring-up ends.")) await start(true);
      } else toastError(e);
    }
  };

  const failing = pre?.checks.filter((c) => c.status !== "ok") ?? [];
  const baseCheck = pre?.checks.find((c) => (c.candidates?.length ?? 0) > 0);
  return (
    <Card className="start-card">
      <div className="row-between">
        <Eyebrow>Bring up a model with Orchard</Eyebrow>
        {canClose && <Button size="xs" variant="ghost" onClick={onClose}>Close</Button>}
      </div>
      <div className="row">
        <div className="grow"><TextInput value={model} onChange={(e) => { setModel(e.target.value); setPre(null); }}
          placeholder="A Hugging Face model, e.g. Qwen/Qwen3-14B" onKeyDown={(e) => { if (e.key === "Enter" && model.trim()) void check(); }} /></div>
        <Button variant={pre ? "subtle" : "primary"} disabled={!model.trim() || checking} onClick={() => check()}>{checking ? "Checking…" : "Check it"}</Button>
      </div>
      {lab && <span className="muted" style={{ fontSize: 13 }}>Hardware tests run on the test box <b>{lab.host}</b>; the coder stays on this box and is never parked.</span>}

      {pre && (
        <div className="start-go">
          <div className="muted" style={{ fontSize: 13 }}>
            {pre.local ? "The weights are already on the box." : `${pre.download_gb.toFixed(1)} GB to download.`}
            {failing.length === 0 ? " Every check passed." : ` ${failing.length} check${failing.length > 1 ? "s" : ""} to look at.`}
            {pre.resuming ? " It picks up an earlier run." : ""}
          </div>
          {(failing.length > 0 || showChecks) && (
            <div className="list">
              {(showChecks ? pre.checks : failing).map((c) => (
                <div key={c.name} className="list-row">
                  <Icon kind={statusIcon(c.status)} width={16} height={16} style={{ color: statusColor(c.status) }} />
                  <div className="grow"><b>{c.name}</b> <span className="muted">{c.detail}</span></div>
                </div>
              ))}
            </div>
          )}
          {baseCheck && (
            <div className="stack" style={{ gap: 6 }}>
              <b style={{ fontSize: 14 }}>Which model should it build on?</b>
              <div style={{ maxWidth: 420 }}>
                <Select size="sm" value={base || CARD_BASE} aria-label="Base model"
                  onValueChange={(v) => { const b = !v || v === CARD_BASE ? "" : v; setBase(b); void check({ base: b }); }}
                  options={[{ value: CARD_BASE, label: "Choose one (the model card's base has no bundle)" }, ...(baseCheck.candidates ?? []).map((c) => ({ value: c.name, label: `${c.name}${c.installed ? " · installed" : " · installs first"}` }))]} />
              </div>
            </div>
          )}
          <details>
            <summary className="muted" style={{ fontSize: 12, cursor: "pointer" }}>Options</summary>
            <div className="stack" style={{ gap: 8, marginTop: 8 }}>
              <div className="row" style={{ gap: 8 }}>
                <div style={{ width: 360 }}><TextInput size="sm" value={base} onChange={(e) => setBase(e.target.value)} placeholder="Base model: a tt-model bundle or org/name (optional)" /></div>
                <Button size="sm" variant="ghost" onClick={() => check()}>Use it</Button>
              </div>
              <Switch size="sm" label="Stop if credential files are visible to the agents (they run as you)" checked={refuse}
                onChange={(e) => { setRefuse(e.target.checked); void check({ refuse: e.target.checked }); }} />
            </div>
          </details>
          <div className="row wrap" style={{ gap: 10 }}>
            {!pre.blocked && <Button onClick={() => start()}>{pre.resuming ? "Resume with Orchard" : "Bring it up with Orchard"}</Button>}
            <Switch size="sm" label={lab ? "Package it (not with a lab box yet)" : "Package it to serve when it's done"} checked={packaging} disabled={!!lab}
              onChange={async (e) => { try { await post("/orchard/packaging", { enabled: e.target.checked }); onStarted(); } catch (err) { toastError(err); } }} />
            <button className="link-button" onClick={() => setShowChecks((v) => !v)}>{showChecks ? "Hide checks" : "All checks"}</button>
          </div>
        </div>
      )}
    </Card>
  );
}

// ---------------------------------------------------------------- the lab box

interface LabState { lab: OrchardLab | null; packaging: boolean; setup: { running: boolean; log: string[]; exit: string | null } }
interface LabCheck { host: string; ready: boolean; steps: { name: string; status: string; detail: string; fixable: boolean }[] }

/**
 * A second box that runs every hardware test (tt-orchard `--lab`): the run,
 * its caches and the test interpreter live under one root with the same
 * absolute path on both boxes, and this box reaches it by ssh key.
 */
function LabCard({ lab, packaging, onChanged }: { lab: OrchardLab | null; packaging: boolean; onChanged: () => void }) {
  const state = usePoll(() => get<LabState>("/orchard/lab"), 5000);
  const [editing, setEditing] = useState(false);
  const [form, setForm] = useState({ host: "", root: "/srv/orchard", path: "~/.local/bin, ~/.tenstorrent-venv/bin", test_python: "", python: "", gozer: "" });
  const [checks, setChecks] = useState<LabCheck | null>(null);
  const [checking, setChecking] = useState(false);
  const setup = state.data?.setup;

  const edit = () => {
    setForm({
      host: lab?.host ?? "", root: lab?.root ?? "/srv/orchard",
      path: (lab?.path ?? ["~/.local/bin", "~/.tenstorrent-venv/bin"]).join(", "),
      test_python: lab?.test_python ?? "", python: lab && lab.python !== "python3" ? lab.python : "", gozer: lab && lab.gozer !== "gozer" ? lab.gozer : "",
    });
    setEditing(true);
  };
  const save = async () => {
    try {
      await put("/orchard/lab", {
        host: form.host.trim(), root: form.root.trim(),
        path: form.path.split(",").map((p) => p.trim()).filter(Boolean),
        test_python: form.test_python.trim() || undefined, python: form.python.trim() || undefined, gozer: form.gozer.trim() || undefined,
      });
      toastOk("Saved. Check the test box before the first run.");
      setEditing(false); setChecks(null); onChanged(); state.refresh();
    } catch (e) { toastError(e); }
  };
  const remove = async () => {
    if (!(await ask("Stop using the test box?", { body: "Runs test on this box's chips again, parking the coder for each test.", confirm: "Stop using it" }))) return;
    try { await put("/orchard/lab", { remove: true }); setChecks(null); onChanged(); state.refresh(); } catch (e) { toastError(e); }
  };
  const check = async () => {
    setChecking(true);
    try { setChecks(await post<LabCheck>("/orchard/lab/check")); } catch (e) { toastError(e); } finally { setChecking(false); }
  };
  const fix = async () => {
    try { await post("/orchard/lab/setup"); toastOk("Fixing what it can on both boxes."); state.refresh(); } catch (e) { toastError(e); }
  };

  return (
    <Card>
      <div className="row-between">
        <div className="stack" style={{ gap: 2 }}>
          <Eyebrow>Test box</Eyebrow>
          <span className="muted" style={{ fontSize: 13 }}>
            {lab ? <>Hardware tests run on <b>{lab.host}</b> (root <code>{lab.root}</code>). The coder stays here and is never parked.</>
              : "Optional: send every hardware test to a second box over ssh, so the coder on this box keeps its chips."}
          </span>
        </div>
        <div className="row" style={{ gap: 6 }}>
          {lab && !editing && <><Button size="sm" variant="subtle" disabled={checking} onClick={check}>{checking ? "Checking…" : "Check the test box"}</Button>
            <Button size="sm" variant="ghost" onClick={edit}>Edit</Button><Button size="sm" variant="ghost" onClick={remove}>Stop using it</Button></>}
          {!lab && !editing && <Button size="sm" variant="subtle" onClick={edit}>Add a test box</Button>}
        </div>
      </div>

      {editing && (
        <div className="stack" style={{ gap: 8 }}>
          {packaging && <Admonition variant="warning" subtle size="sm">Packaging is on, and tt-orchard can't package a run that tests on another box yet. Turn packaging off first.</Admonition>}
          <div className="grid grid-2" style={{ gap: 8 }}>
            <TextInput size="sm" value={form.host} onChange={(e) => setForm({ ...form, host: e.target.value })} placeholder="ssh host: node4 or user@node4" />
            <TextInput size="sm" value={form.root} onChange={(e) => setForm({ ...form, root: e.target.value })} placeholder="Root folder, same path on both boxes: /srv/orchard" />
            <TextInput size="sm" value={form.path} onChange={(e) => setForm({ ...form, path: e.target.value })} placeholder="PATH on the test box (comma-separated)" />
            <TextInput size="sm" value={form.test_python} onChange={(e) => setForm({ ...form, test_python: e.target.value })} placeholder="Test python on the test box (optional)" />
            <TextInput size="sm" value={form.python} onChange={(e) => setForm({ ...form, python: e.target.value })} placeholder="Python for the test box helper (default python3)" />
            <TextInput size="sm" value={form.gozer} onChange={(e) => setForm({ ...form, gozer: e.target.value })} placeholder="gozer on the test box (default gozer)" />
          </div>
          <span className="muted" style={{ fontSize: 12 }}>
            Before the first run: this box must ssh to the test box with a key (no password prompt), and the root must exist on both boxes, owned by you (<code>sudo mkdir -p /srv/orchard && sudo chown "$USER" /srv/orchard</code>). The run's runs, caches and Hugging Face cache must sit under the root.
          </span>
          <div className="row"><Button size="sm" disabled={!form.host.trim() || !form.root.trim()} onClick={save}>Save</Button><Button size="sm" variant="ghost" onClick={() => setEditing(false)}>Cancel</Button></div>
        </div>
      )}

      {checks && (
        <div className="stack" style={{ gap: 6 }}>
          <div className="row-between">
            <b style={{ fontSize: 14 }}>{checks.ready ? `${checks.host} is ready for runs.` : `${checks.host} isn't ready yet.`}</b>
            {!checks.ready && checks.steps.some((x) => x.fixable) && <Button size="sm" disabled={setup?.running} onClick={fix}>{setup?.running ? "Fixing…" : "Fix what it can"}</Button>}
          </div>
          <div className="list">
            {checks.steps.map((x) => (
              <div key={x.name} className="list-row" style={{ alignItems: "flex-start" }}>
                <Icon kind={statusIcon(x.status)} width={16} height={16} style={{ color: statusColor(x.status), marginTop: 2 }} />
                <div className="grow"><b style={{ fontSize: 13 }}>{x.name}</b> <span className="muted" style={{ fontSize: 12 }}>{x.detail}</span></div>
              </div>
            ))}
          </div>
        </div>
      )}
      {setup && (setup.running || setup.exit) && setup.log.length > 0 && (
        <div className="stack" style={{ gap: 4 }}>
          <span className="muted" style={{ fontSize: 12 }}>{setup.running ? "Fixing…" : setup.exit === "0" ? "The last fix finished: the test box is ready." : "The last fix finished with steps left (it never runs sudo; the log says what to do)."}</span>
          <div className="console" style={{ maxHeight: 180, fontSize: 11 }}>{setup.log.join("\n")}</div>
          {!setup.running && <div><Button size="xs" variant="ghost" onClick={check}>Check again</Button></div>}
        </div>
      )}
    </Card>
  );
}
