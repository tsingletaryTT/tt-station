// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Embeddings and reranking: two small models on the box's CPU, beside the
// model on the chips, served by the box's own API (`/v1/embeddings`,
// `/v1/rerank`). Document search, research apps, agent memory and knowledge
// graphs use them.

import { useState } from "react";
import { Badge } from "@tenstorrent/vesper/badge";
import { Select } from "@tenstorrent/vesper/select";
import { Switch } from "@tenstorrent/vesper/switch";
import { StatusIndicator } from "@tenstorrent/vesper/status-indicator";
import { Typography } from "@tenstorrent/vesper/typography";
import { get, put } from "../api";
import { Card, Eyebrow, toastError, toastOk, usePoll } from "../ui";

interface Svc {
  id: "embed" | "rerank"; name: string; model: string | null; status: "off" | "starting" | "ready" | "failed" | "stopped";
  step: string | null; error: string | null; endpoint: string; models: { id: string; about: string; dims: number }[];
}

const ABOUT: Record<Svc["id"], string> = {
  embed: "Turns text into vectors, so apps can search documents by meaning. Needed by document chat, LightRAG, agent memory and research apps.",
  rerank: "Orders search results by how well they answer a question, so apps read the best passages first.",
};

export function CpuServices() {
  const s = usePoll(() => get<{ services: Svc[] }>("/embeddings"), 4000);
  const [busy, setBusy] = useState<string | null>(null);
  const set = async (svc: Svc, model: string | null) => {
    setBusy(svc.id);
    try {
      await put("/embeddings", { service: svc.id, model });
      toastOk(model ? `${svc.name}: starting ${model.split("/").pop()}. The first time, it downloads the model.` : `${svc.name} is off.`);
      s.refresh();
    } catch (e) { toastError(e); } finally { setBusy(null); }
  };
  return (
    <>
      <div className="section-title"><Eyebrow>Embeddings and reranking</Eyebrow><span className="muted">Small models on the box's CPU, beside the model on the chips, served at the same address with the same app keys.</span></div>
      <div className="grid grid-2">
        {(s.data?.services ?? []).map((svc) => {
          const on = svc.status !== "off";
          return (
            <Card key={svc.id}>
              <div className="row-between">
                <div className="row" style={{ gap: 8 }}>
                  <Typography variant="heading-xs">{svc.name}</Typography>
                  <Badge size="sm" variant="contrast" subtle>CPU</Badge>
                </div>
                <div className="row" style={{ gap: 10 }}>
                  {on && <StatusIndicator label={svc.status === "ready" ? "Ready" : svc.status === "failed" ? "Failed" : svc.status === "stopped" ? "Stopped" : svc.step ?? "Starting"}
                    state={svc.status === "ready" ? "ready" : svc.status === "failed" ? "error" : svc.status === "stopped" ? "queued" : "progress"} animated={svc.status === "starting"} />}
                  <Switch checked={on} disabled={busy === svc.id} aria-label={`${svc.name} on or off`}
                    onChange={(e) => set(svc, e.target.checked ? svc.models[0].id : null)} />
                </div>
              </div>
              <span className="muted" style={{ fontSize: 13 }}>{ABOUT[svc.id]}</span>
              {on && (
                <div className="stack" style={{ gap: 6 }}>
                  <Select size="sm" value={svc.model ?? svc.models[0].id} aria-label={`${svc.name} model`}
                    onValueChange={(v) => v && v !== svc.model && set(svc, v)}
                    options={svc.models.map((m) => ({ value: m.id, label: m.id.split("/").pop()! }))} />
                  <span className="muted" style={{ fontSize: 12 }}>{svc.models.find((m) => m.id === svc.model)?.about}</span>
                  {svc.status === "ready" && <span className="muted" style={{ fontSize: 12 }}>Apps use it at <code>{svc.endpoint}</code> on the box's API{svc.id === "rerank" ? " (Cohere/Jina format)" : " (OpenAI format)"}.</span>}
                  {svc.status === "starting" && <span className="muted" style={{ fontSize: 12 }}>The first start downloads the model (up to about 2 GB), then it's ready in seconds.</span>}
                  {svc.error && <span style={{ fontSize: 12, color: "var(--vesper-text-error)" }}>{svc.error}</span>}
                </div>
              )}
            </Card>
          );
        })}
      </div>
    </>
  );
}
