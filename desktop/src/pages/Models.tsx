// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Models: everything the official tt CLI knows runs on this box. Download,
// start and remove.

import { useMemo, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Badge } from "@tenstorrent/vesper/badge";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Icon } from "@tenstorrent/vesper/icons";
import type { IconKind } from "@tenstorrent/vesper/icons";
import { Typography } from "@tenstorrent/vesper/typography";
import { useModal, Modal } from "@tenstorrent/vesper/modal";
import { bytes, del, get, post, type Pull, sameModel, short, type TtModel } from "../api";
import { ask, whoIsAffected } from "../dialog";
import { navigate } from "../App";
import { Card, DownloadProgress, Empty, Eyebrow, matches, Page, toastError, toastOk, usePoll, useServe } from "../ui";
import { takeOpenRequest } from "../modelsLink";
import { FavoriteStar, useFavorites } from "../favorites";
import { CpuServices } from "./CpuServices";
import { Evals } from "./Evals";

type Filter = "all" | "favorites" | "box" | "chat" | "image" | "video" | "audio" | "checkups";

function family(name: string): IconKind {
  const n = name.toLowerCase();
  if (n.includes("llama")) return "model-llama-color";
  if (n.includes("qwen")) return "model-qwen-color";
  if (n.includes("deepseek")) return "model-deepseek-color";
  if (n.includes("gemma") || n.includes("gemini")) return "model-google-color";
  if (n.includes("falcon")) return "model-falcon-color";
  if (n.includes("gpt-oss") || n.includes("whisper")) return "model-openai";
  return "ai-model";
}

const kindOf = (m: TtModel): Filter =>
  m.model_type === "image" || m.engines.includes("media") && /flux|diffusion|sd/i.test(m.name) ? "image"
  : /video|wan|mochi/i.test(m.model_type + m.name) ? "video"
  : m.model_type === "audio" || /whisper/i.test(m.name) ? "audio" : "chat";

export function Models() {
  const catalog = usePoll(() => get<{ models: TtModel[]; pulls: Pull[]; disk_free_bytes: number }>("/tt/models"), 30000);
  const pulls = usePoll(() => get<{ pulls: Pull[]; disk_free_bytes: number }>("/tt/pulls"), 3000);
  const serve = useServe(4000);
  const [opened] = useState(takeOpenRequest);
  const [query, setQuery] = useState(opened?.query ?? "");
  const [filter, setFilter] = useState<Filter>((opened?.filter as Filter) ?? "all");
  const [pending, setPending] = useState<TtModel | null>(null);
  const confirm = useModal();
  const fav = useFavorites();
  // Only the official catalog has stars now; older boxes may still list other kinds.
  const favCount = fav.list.filter((f) => f.kind === "official").length;

  const running = new Map((pulls.data?.pulls ?? []).filter((p) => p.state === "running").map((p) => [p.model, p]));

  const models = useMemo(() => {
    const q = query.trim().toLowerCase();
    return (catalog.data?.models ?? [])
      .filter((m) => m.supported)
      .filter((m) => matches(`${m.name} ${m.model_type}`, q))
      .filter((m) => filter === "all" || (filter === "favorites" ? fav.has("official", m.name)
        : filter === "box" ? m.weights === "complete" || m.weights === "container" : kindOf(m) === filter))
      .sort((a, b) => Number(fav.has("official", b.name)) - Number(fav.has("official", a.name))
        || Number(b.weights === "complete") - Number(a.weights === "complete") || a.name.localeCompare(b.name));
  }, [catalog.data, query, filter, fav.list]); // eslint-disable-line react-hooks/exhaustive-deps

  const start = async (m: TtModel) => {
    const s = serve.data;
    const current = s && (s.phase === "serving" || s.phase === "starting") ? short(s.served_id ?? s.model) : null;
    if (current && !(await ask(`Start ${m.name} instead of ${current}?`, { body: whoIsAffected(current), confirm: `Start ${m.name}` }))) return;
    try {
      await post("/serve", { model: m.name, force: true });
      toastOk(`Starting ${m.name}. It takes a few minutes; you'll get a notification.`);
      serve.refresh();
    } catch (e) { toastError(e); }
  };

  return (
    <Page title="Models" subtitle={`${(catalog.data?.models ?? []).filter((m) => m.supported).length} models run on this box · ${bytes(pulls.data?.disk_free_bytes ?? catalog.data?.disk_free_bytes)} free`}>
      <div className="row-between wrap">
        <div style={{ width: 360, visibility: filter === "checkups" ? "hidden" : undefined }}>
          <TextInput value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Search models" iconLeft={<Icon kind="search" />} />
        </div>
        <div className="pill-tabs">
          {(["all", "favorites", "box", "chat", "image", "video", "audio", "checkups"] as Filter[]).map((f) => (
            <button key={f} className={filter === f ? "on" : ""} onClick={() => setFilter(f)}>
              {f === "all" ? "All" : f === "favorites" ? <>★ Favorites{favCount ? <span className="muted"> {favCount}</span> : null}</>
                : f === "box" ? "On this box" : f === "checkups" ? "Check-ups" : f[0].toUpperCase() + f.slice(1)}
            </button>
          ))}
        </div>
      </div>

      {filter === "checkups" && <Evals />}
      {catalog.error && filter !== "checkups" && <Empty title="Couldn't load models">{catalog.error}</Empty>}
      {models.length > 0 && (
        <div className="section-title"><Eyebrow>Official</Eyebrow><span className="muted">Tenstorrent's tested models for this box.</span></div>
      )}
      <div className="grid grid-3">
        {models.map((m) => {
          const pull = running.get(m.name);
          const onBox = m.weights === "complete" || m.weights === "container";
          const isServing = serve.data?.phase === "serving" && (sameModel(serve.data.served_id, m.name) || sameModel(serve.data.model, m.name));
          return (
            <Card key={m.name} tone={isServing ? "accent" : undefined}>
              <div className="row-between">
                <Icon kind={family(m.name)} style={{ width: 26, height: 26 }} />
                <div className="row" style={{ gap: 6 }}>
                  {isServing && <Badge size="sm" variant="mint" subtle>Serving</Badge>}
                  <Badge size="sm" variant="contrast" subtle>{kindOf(m)}</Badge>
                  <FavoriteStar kind="official" id={m.name} name={m.name} />
                </div>
              </div>
              <div>
                <Typography variant="heading-xs">{m.name}</Typography>
                <span className="muted mono" style={{ fontSize: 12 }}>{m.hf_repo ?? m.model_type}</span>
              </div>
              {pull ? (
                <DownloadProgress progress={pull.progress ?? { done_bytes: pull.bytes_on_disk, total_bytes: null, percent: null, bytes_per_s: null, eta_s: null }} step="Downloading the weights" />
              ) : (
                <span className="muted" style={{ fontSize: 13 }}>
                  {onBox ? "On this box" : m.weights === "partial" ? `Partly downloaded · ${bytes(m.downloaded_bytes)}` : m.min_disk_gb ? `Needs about ${m.min_disk_gb} GB` : "Not downloaded"}
                </span>
              )}
              <div className="grow" />
              <div className="row">
                {onBox && !isServing && <Button size="sm" onClick={() => start(m)}>Start</Button>}
                {isServing && <Button size="sm" variant="subtle" onClick={() => navigate(kindOf(m) === "chat" ? "chat" : "studio")}>Use it</Button>}
                {!onBox && !pull && <Button size="sm" variant="subtle" iconLeft={<Icon kind="download" />}
                  onClick={async () => { try { await post("/tt/pull", { model: m.name }); pulls.refresh(); } catch (e) { toastError(e); } }}>Download</Button>}
                {pull && <Button size="sm" variant="ghost" onClick={async () => { try { await post("/tt/pull/cancel", { model: m.name }); pulls.refresh(); } catch (e) { toastError(e); } }}>Cancel</Button>}
                {onBox && !isServing && m.weights !== "container" && <Button size="sm" variant="ghost" onClick={() => { setPending(m); confirm.open(); }}>Remove</Button>}
              </div>
            </Card>
          );
        })}
      </div>
      {filter === "favorites" && fav.loaded && favCount === 0 && (
        <Empty title="No favorites yet">Star any model (☆ in a card's corner) to keep it here, at the top of its section.</Empty>
      )}
      {filter === "all" && <CpuServices />}

      <Modal ref={confirm.ref} title={`Remove ${pending?.name ?? ""}?`} description="Its weights are deleted from the box. You can download it again later."
        buttons={[{ children: "Cancel", onClick: confirm.close },
          { children: "Remove", variant: "danger", onClick: async () => {
            confirm.close();
            if (!pending) return;
            try { await del(`/tt/models/${encodeURIComponent(pending.name)}?weights=true`); toastOk(`${pending.name} removed.`); catalog.refresh(); } catch (e) { toastError(e); }
          } }]} />
    </Page>
  );
}
