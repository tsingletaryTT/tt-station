# Model for Code: the picker

Every Code session runs on the model the box is serving. The **Model for Code** picker (the
model name in a Code session's header, or in the new-session form) lists every model on the
box that Code could run on, and switches the box to one. There's no recommended default:
pick any model and profile that works for you. The box keeps what it measured for each one,
so the picker can show it.

Agent: `crates/tt-station-agentd/src/codemodels.rs` (the list), `codespeed.rs` (speeds and
prefix caching). Desktop: `desktop/src/pages/CodeSpeed.tsx`, `desktop/src/codeModels.ts`.

## What's listed

`GET /code/models` (any paired device) gathers models from every way the box serves one:

| Source | One choice per | Started with |
|---|---|---|
| `tt` catalog (`tt model list`) | model | `POST /serve` `{ "model", "force": true }` |
| tt-model community packages (`~/.cache/tt-model/installed.json`) | serve profile that fits the box | `POST /community/serve` `{ "id", "profile", "stop_serving": true }` |
| Native vLLM bundles (tt-kernel's and bring-up ones, as `GET /vllm/native` lists them) | bundle | `POST /vllm/native/serve` `{ "dir", "stop_serving": true }` |

A model is listed only when it would work for Code:

- **A chat model.** Its Hugging Face `config.json` names a causal-LM or vision-language
  architecture and it has a chat template. Base checkpoints (no chat template), media models
  and embedding or reranking models are left out. Catalog models must be `llm` or `vlm`.
- **Weights on the box**, every shard.
- **Fits the box.** A package profile's chips come from its hardware (`p150` 1, `p150x2` 2,
  `p300` 2, `p150x4` and `p300x2` 4) or its visible-devices list; a profile for more chips
  than the box has, or for other hardware (Galaxy, Wormhole), is left out. Catalog models must
  be supported on the box's mesh; bundles must have a launch command for Blackhole and their
  model code.
- **Tool calling on its serving path.** A package's `tool_parser` (per profile or for the
  package), a bundle's `--tool-call-parser`, or the catalog's `tool_call_parser`. The catalog
  knows: a model without one isn't listed. A package or bundle that doesn't say is listed with
  a **Tool calling unverified** badge rather than hidden.

Each choice says:

| Field | What it is |
|---|---|
| `group`, `name` | The model; the picker groups a model's choices together |
| `source`, `origin`, `profile`, `profile_label` | Where it comes from: "tt catalog", the package and profile ("4 chips · low latency": a `-throughput` profile is "throughput", its sibling "low latency"), or the bundle |
| `chips`, `context` | Chips it uses and its context window (Code uses up to 131K of it) |
| `tools` | `verified`, and how it's known ("tool parser openai") or why it isn't |
| `prefix_cache` | `on`, `off` or `unknown`, and how it's known: the live server for the model serving now, else what was seen when it last served here, else its launch flags (a DFlash profile, or `--no-enable-prefix-caching`, is off) |
| `speed` | The median decode speed and time to first token Code measured on the box in that setup, or null |
| `reliability` | The latest check-up's agent reliability (Evals): valid tool calls within one retry, and the window it held up to, or null |
| `choice` | `in_use`, `switch`, `busy` (with why) or `unavailable` (with why) |
| `serve` | The route and body that start it (null for the one in use) |

The model in use comes first, then models with verified tool calling, then unverified ones.
The one in use is the exact setup serving: for a community package, the package and the
profile its `tt-model serve` was given (its default if none). A model serving by a path the
list doesn't know is still shown first, as `source: "running"`.

## Speeds and check-ups per setup

Code's speed book (`code-speed.json` in the agent's config folder) keeps each model's
measurements under the setup that served it: `<model> @ <package>#<profile>` for a community
package, the model's name for anything else. Check-ups (`evals/<id>.json`) record the same
`setup`. Entries and check-ups from before the box kept the setup are shown for a setup that
has none of its own, marked **setup not recorded**.

## Switching

**Use for Code** opens a confirm in place that says what stops (the model serving now, for
everyone using the box: Chat, Code, apps and connected clients), how many chips the new choice
uses, how long it takes to load, and, for unverified tool calling, that Code may not be able to
work with it. Confirming calls the choice's `serve` route, the same start Models uses, so that
route's permission checks, the model-control setting, chip leases and refusals (weights
missing, chips held by a bring-up, a start already under way) apply unchanged. The picker
only ever calls `/serve`, `/community/serve` or `/vllm/native/serve`.

`POST /community/serve` takes an optional `profile`: one of the package's own profiles that
fits the box (`tt-model serve … --profile <it>`). An unknown profile or one too big for the box
is refused with 409 before anything runs. Without one, the box picks as before (the package
default, or its QuietBox 2 profile when the default is for other hardware).

Keyboard: every choice's **Use for Code** button and the unverified badge (which carries why)
are in the tab order; the confirm takes focus on open, Escape cancels it and returns focus to
the choice's button.
