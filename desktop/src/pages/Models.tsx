// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Models: everything the official tt CLI knows runs on this box. Download,
// start, remove, and bring up anything that isn't listed yet.

import { useMemo, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Badge } from "@tenstorrent/vesper/badge";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Icon } from "@tenstorrent/vesper/icons";
import type { IconKind } from "@tenstorrent/vesper/icons";
import { Typography } from "@tenstorrent/vesper/typography";
import { useModal, Modal } from "@tenstorrent/vesper/modal";
import { ApiError, bytes, del, get, post, type Pull, sameModel, short, type TtModel } from "../api";
import { ask, askToStop, whoIsAffected } from "../dialog";
import { useDevTools } from "../devtools";
import { navigate } from "../App";
import { Card, DownloadProgress, Empty, Eyebrow, matches, Page, toastError, toastOk, usePoll, useServe } from "../ui";
import { CatalogFavorites, CommunityCatalog, CommunitySection, takeOpenRequest, useCommunity } from "./CommunityModels";
import { FavoriteStar, useFavorites } from "../favorites";
import { CpuServices } from "./CpuServices";
import { Evals } from "./Evals";

type Filter = "all" | "favorites" | "box" | "chat" | "image" | "video" | "audio" | "community" | "checkups";

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

interface Bundle {
  id: string; source: "tt-kernel" | "bring-up"; dir: string; arch: string | null; description: string | null;
  model: string | null; weights: string | null; weights_on_box: boolean; code_present: boolean; ready: boolean; why_not: string | null;
}
interface Native {
  ready: boolean; vllm: boolean; plugin: boolean; python: string; tt_metal: string; can_set_up: boolean; env_busy: boolean;
  setting_up: boolean; setup_failed: boolean; setup_log: string[]; bundles: Bundle[]; serving_native: string | null; tt_kernel_hint: string;
}

/**
 * Models that run straight from their code instead of a pre-built package:
 * your own bring-ups (once their vLLM stage passes) and community models
 * shared with tt-kernel. They look and start like any other model; the only
 * difference shown is where they came from.
 */
function CodeModels({ query, filter, onServe }: { query: string; filter: Filter; onServe: () => void }) {
  const native = usePoll(() => get<Native>("/vllm/native"), 5000);
  const fav = useFavorites();
  const n = native.data;
  if (!n || !(filter === "all" || filter === "chat" || filter === "box" || filter === "community" || filter === "favorites")) return null;
  const q = query.trim().toLowerCase();
  const list = n.bundles.filter((b) => matches(`${b.id} ${b.model ?? ""}`, q)).filter((b) => filter !== "box" || b.ready)
    .filter((b) => filter !== "favorites" || fav.has("code", b.id))
    .sort((a, b) => Number(fav.has("code", b.id)) - Number(fav.has("code", a.id)));
  const start = async (b: Bundle, stopServing = false) => {
    if (!n.ready) {
      // One-time: these run from tt-metal's own Python, which needs the serving pieces added once.
      if (n.setting_up) { toastOk("Still getting ready to run models from code. Try again in a few minutes."); return; }
      if (!(await ask("Get ready to run models from their code?", {
        body: "A one-time setup of a few minutes adds the serving pieces to tt-metal. After that, this model and any you bring up start like the others.",
        confirm: "Set it up",
      }))) return;
      try { await post("/vllm/native/setup"); toastOk("Setting up. Start the model again when it's done."); native.refresh(); } catch (e) { toastError(e); }
      return;
    }
    try {
      await post("/vllm/native/serve", { dir: b.dir, stop_serving: stopServing });
      toastOk(`Starting ${short(b)}. The first start takes a while; you'll get a notification.`);
      native.refresh(); onServe();
    } catch (e) {
      if (e instanceof ApiError && e.status === 409 && (e.body as { needs_stop?: boolean })?.needs_stop) { if (await askToStop(e.message, "")) await start(b, true); }
      else toastError(e);
    }
  };
  const short = (b: Bundle) => (b.model ?? b.id).split("/").pop() ?? b.id;
  return (
    <>
      {list.map((b) => {
        const serving = n.serving_native === b.id;
        return (
          <Card key={b.dir} tone={serving ? "accent" : undefined}>
            <div className="row-between">
              <Icon kind={family(b.model ?? b.id)} style={{ width: 26, height: 26 }} />
              <div className="row" style={{ gap: 6 }}>
                {serving && <Badge size="sm" variant="mint" subtle>Serving</Badge>}
                <Badge size="sm" variant={b.source === "bring-up" ? "info" : "contrast"} subtle>{b.source === "bring-up" ? "your bring-up" : "community"}</Badge>
                <FavoriteStar kind="code" id={b.id} name={short(b)} />
              </div>
            </div>
            <div>
              <Typography variant="heading-xs">{short(b)}</Typography>
              <span className="muted mono" style={{ fontSize: 12 }}>{b.id}</span>
            </div>
            <span className="muted" style={{ fontSize: 13 }}>
              {!b.ready ? (b.why_not ?? "Can't run on this box yet.")
                : n.setting_up ? "Getting ready to run (one-time setup)…"
                : n.setup_failed && !n.ready ? "The one-time setup didn't finish. Start tries it again."
                : b.weights_on_box ? "On this box" : "Downloads its weights on first start"}
            </span>
            <div className="grow" />
            <div className="row">
              {b.ready && !serving && <Button size="sm" disabled={n.setting_up || (!n.ready && n.env_busy)}
                title={!n.ready && n.env_busy ? "Available once the current bring-up ends." : undefined} onClick={() => start(b)}>Start</Button>}
              {serving && <Button size="sm" variant="subtle" onClick={() => navigate("chat")}>Use it</Button>}
            </div>
          </Card>
        );
      })}
    </>
  );
}

export function Models() {
  const catalog = usePoll(() => get<{ models: TtModel[]; pulls: Pull[]; disk_free_bytes: number }>("/tt/models"), 30000);
  const pulls = usePoll(() => get<{ pulls: Pull[]; disk_free_bytes: number }>("/tt/pulls"), 3000);
  const serve = useServe(4000);
  const devTools = useDevTools();
  const community = useCommunity();
  const [opened] = useState(takeOpenRequest);
  const [query, setQuery] = useState(opened?.query ?? "");
  const [filter, setFilter] = useState<Filter>((opened?.filter as Filter) ?? "all");
  const [pending, setPending] = useState<TtModel | null>(null);
  const confirm = useModal();
  const fav = useFavorites();

  const running = new Map((pulls.data?.pulls ?? []).filter((p) => p.state === "running").map((p) => [p.model, p]));

  const models = useMemo(() => {
    const q = query.trim().toLowerCase();
    return (catalog.data?.models ?? [])
      .filter((m) => m.supported)
      .filter((m) => matches(`${m.name} ${m.model_type}`, q))
      .filter(() => filter !== "community")
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
    <Page title="Models" subtitle={`${(catalog.data?.models ?? []).filter((m) => m.supported).length} models run on this box · ${bytes(pulls.data?.disk_free_bytes ?? catalog.data?.disk_free_bytes)} free`}
      actions={devTools ? <Button variant="tertiary" iconLeft={<Icon kind="ai-task" aria-hidden />} onClick={() => navigate("bringup")}>Not listed? Bring one up</Button> : undefined}>
      <div className="row-between wrap">
        <div style={{ width: 360, visibility: filter === "checkups" ? "hidden" : undefined }}>
          <TextInput value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Search models" iconLeft={<Icon kind="search" />} />
        </div>
        <div className="pill-tabs">
          {(["all", "favorites", "box", "chat", "image", "video", "audio", "community", "checkups"] as Filter[]).map((f) => (
            <button key={f} className={filter === f ? "on" : ""} onClick={() => setFilter(f)}>
              {f === "all" ? "All" : f === "favorites" ? <>★ Favorites{fav.list.length ? <span className="muted"> {fav.list.length}</span> : null}</>
                : f === "box" ? "On this box" : f === "checkups" ? "Check-ups" : f[0].toUpperCase() + f.slice(1)}
            </button>
          ))}
        </div>
      </div>

      {filter === "checkups" && <Evals />}
      {catalog.error && filter !== "checkups" && <Empty title="Couldn't load models">{catalog.error}</Empty>}
      {filter !== "community" && models.length > 0 && (
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
      {filter === "favorites" && fav.loaded && fav.list.length === 0 && (
        <Empty title="No favorites yet">Star any model (☆ in a card's corner) to keep it here, at the top of its section.</Empty>
      )}
      {["all", "favorites", "box", "chat", "community"].includes(filter) && (filter !== "favorites" || fav.list.some((f) => f.kind === "community" || f.kind === "code")) && (
        <CommunitySection c={community.data} query={query} filter={filter}
          onServe={() => { serve.refresh(); community.refresh(); }}
          onBrowse={filter === "community" ? undefined : () => setFilter("community")}>
          <CodeModels query={query} filter={filter} onServe={() => serve.refresh()} />
        </CommunitySection>
      )}
      {filter === "all" && <CpuServices />}
      {filter === "community" && <CommunityCatalog c={community.data} query={query} onGot={() => community.refresh()} />}
      {filter === "favorites" && <CatalogFavorites c={community.data} query={query} onGot={() => community.refresh()} />}
      {filter !== "community" && filter !== "favorites" && filter !== "checkups" && models.length === 0 && !catalog.error && catalog.data && (
        <Card>
          <Eyebrow>Not in the list?</Eyebrow>
          {devTools ? <>
            <Typography variant="copy-md">Bring up a Hugging Face model yourself: a Quick port grafts it onto one that runs here, an Agent port writes a new architecture's code, or start a community package someone already made.</Typography>
            <div><Button variant="subtle" onClick={() => navigate("bringup")}>Open Bring-up</Button></div>
          </> : <>
            <Typography variant="copy-md">Try the Community tab: people package models for Tenstorrent chips there. To port a model yourself, turn on Developer tools in Settings → General.</Typography>
            <div><Button variant="subtle" onClick={() => setFilter("community")}>Browse community models</Button></div>
          </>}
        </Card>
      )}

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
