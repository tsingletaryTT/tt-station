// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CodeModel, CodeModels } from "../codeModels";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const poll = vi.hoisted(() => ({ data: null as CodeModels | null, error: null as string | null }));
const post = vi.hoisted(() => vi.fn((_route: string, _body: unknown) => Promise.resolve({})));
const toastError = vi.hoisted(() => vi.fn());
vi.mock("../api", () => ({ get: () => Promise.resolve(null), post }));
vi.mock("../App", () => ({ navigate: () => {} }));
vi.mock("../ui", () => ({
  usePoll: () => ({ data: poll.data, error: poll.error, refresh: () => Promise.resolve() }),
  toastError, toastOk: () => {},
}));

const { ModelPicker } = await import("./CodeSpeed");

const opt = (o: Partial<CodeModel> & { key: string }): CodeModel => ({
  group: "gpt-oss-20b", name: "gpt-oss-20b", model: "openai/gpt-oss-20b", source: "community", origin: "tt-hous/gpt-oss-20b-p150",
  profile: null, profile_label: null, profile_note: null, chips: 2, context: 131072,
  tools: { verified: true, how: "tool parser openai" }, prefix_cache: { state: "unknown", how: "not set in its launch flags" },
  params_b: null, served_id: null, speed: null, reliability: null, choice: { action: "switch" }, eta_s: null, serve: null, ...o,
});
const list = (models: CodeModel[]): CodeModels => ({
  code_model: "openai/gpt-oss-20b",
  serve: { phase: "serving", model: "openai/gpt-oss-20b", since: 0, error: null, eta_s: null },
  catalog_error: null, models,
});
const BOX = [
  opt({ key: "community:pkg#p150x2-throughput", profile: "p150x2-throughput", profile_label: "2 chips · throughput", choice: { action: "in_use" },
    served_id: "openai/gpt-oss-20b", speed: { decode_tps: 69, ttft_s: 0.1, steps: 40 },
    reliability: { window: 65536, valid: 99, trials: 100, at: 0, setup_unrecorded: true } }),
  opt({ key: "community:pkg#p150x4", profile: "p150x4", profile_label: "4 chips · low latency", chips: 4,
    serve: { route: "/community/serve", body: { id: "tt-hous/gpt-oss-20b-p150", profile: "p150x4", stop_serving: true } } }),
  opt({ key: "community:flash#qb2-256k", group: "qwen3.8-flash-next", name: "Qwen3.8-Flash-Next", profile: "qb2-256k", profile_label: "4 chips · qb2-256k",
    chips: 4, tools: { verified: false, how: "its package doesn't name a tool parser" },
    serve: { route: "/community/serve", body: { id: "x/flash", profile: "qb2-256k", stop_serving: true } } }),
];

let root: Root;
let host: HTMLDivElement;
const render = () => act(() => { root.render(<ModelPicker />); });
const buttons = (text: string) => [...host.querySelectorAll<HTMLButtonElement>("button")].filter((b) => b.textContent === text);
const click = async (el: HTMLElement) => { await act(async () => { el.click(); }); };

beforeEach(() => {
  poll.data = null;
  poll.error = null;
  post.mockClear();
  toastError.mockClear();
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});
afterEach(() => { act(() => root.unmount()); document.body.innerHTML = ""; });

describe("Model for Code", () => {
  it("groups each model's ways to run it, the one in use marked", () => {
    poll.data = list(BOX);
    render();
    const groups = [...host.querySelectorAll(".code-models-list > li")];
    expect(groups.map((g) => g.querySelector(".code-model-name")!.textContent)).toEqual(["gpt-oss-20b", "Qwen3.8-Flash-Next"]);
    const options = [...groups[0].querySelectorAll(".code-model-option")];
    expect(options.map((o) => o.querySelector(".code-model-option-title")!.textContent)).toEqual(["2 chips · throughput", "4 chips · low latency"]);
    expect(options[0].classList.contains("on")).toBe(true);
    expect(options[0].textContent).toContain("In use");
    expect(options[0].textContent).toContain("69 tok/s");
    expect(options[0].textContent).toContain("99% valid tool calls");
    expect(options[0].textContent).toContain("setup not recorded");
    // Nothing to start on what's in use.
    expect(options[0].querySelector("button")).toBeNull();
  });

  it("marks tool calling nobody has verified, and says so before switching", async () => {
    poll.data = list(BOX);
    render();
    const flash = host.querySelectorAll(".code-models-list > li")[1];
    const badge = flash.querySelector<HTMLElement>(".code-model-unverified")!;
    expect(badge.textContent).toBe("Tool calling unverified");
    expect(badge.tabIndex).toBe(0);
    expect(badge.getAttribute("aria-label")).toContain("doesn't name a tool parser");
    await click(flash.querySelector<HTMLButtonElement>("button")!);
    expect(flash.querySelector(".code-model-confirm")!.textContent).toContain("Its tool calling isn't verified");
  });

  it("confirms what a switch stops, then starts the chosen profile through its own route", async () => {
    poll.data = list(BOX);
    render();
    const use = buttons("Use for Code");
    expect(use[0].getAttribute("aria-label")).toBe("Use gpt-oss-20b, 4 chips · low latency, for Code");
    await click(use[0]);
    const confirm = host.querySelector(".code-model-confirm")!;
    expect(confirm.textContent).toContain("Switch the box to gpt-oss-20b (4 chips · low latency)?");
    expect(confirm.textContent).toContain("gpt-oss-20b stops for everyone using the box");
    expect(confirm.textContent).toContain("It uses all 4 of the box's chips.");
    // Focus lands on the confirm; Escape backs out without starting anything.
    expect(document.activeElement?.textContent).toBe("Stop gpt-oss-20b and switch");
    await act(async () => { confirm.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })); });
    expect(host.querySelector(".code-model-confirm")).toBeNull();
    expect(post).not.toHaveBeenCalled();

    await click(buttons("Use for Code")[0]);
    await click(buttons("Stop gpt-oss-20b and switch")[0]);
    expect(post).toHaveBeenCalledWith("/community/serve", { id: "tt-hous/gpt-oss-20b-p150", profile: "p150x4", stop_serving: true });
  });

  it("says when nothing on the box can run Code, and when the list won't load", () => {
    poll.data = list([]);
    render();
    expect(host.textContent).toContain("No models on the box can run Code yet.");
    act(() => root.unmount());
    root = createRoot(host);
    poll.data = null;
    poll.error = "connection refused";
    render();
    expect(host.textContent).toContain("Couldn't load the models: connection refused");
  });
});
