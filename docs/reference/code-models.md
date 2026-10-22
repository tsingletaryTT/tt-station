# Model for Code: the picker

Every Code session runs on the model the box is serving. The **Model for Code** picker (the
model name in a Code session's header, or in the new-session form) lists every model on the
box that Code could run on, and switches the box to one. There's no recommended default:
pick any model that works for you. The box keeps what it measured for each one,
so the picker can show it.

Agent: `crates/tt-station-agentd/src/codemodels.rs` (the list), `codespeed.rs` (speeds and
prefix caching). Desktop: `desktop/src/pages/CodeSpeed.tsx`, `desktop/src/codeModels.ts`.

## What's listed

`GET /code/models` (any paired device) lists the `tt` catalog's models (`tt model list`),
one choice per model, each started with `POST /serve` `{ "model", "force": true }`.

A model is listed only when it would work for Code:

- **A chat model.** Its Hugging Face `config.json` names a causal-LM or vision-language
  architecture and it has a chat template. Base checkpoints (no chat template), media models
  and embedding or reranking models are left out. Catalog models must be `llm` or `vlm`.
- **Weights on the box**, every shard.
- **Fits the box.** Catalog models must be supported on the box's mesh.
- **Tool calling on its serving path.** The catalog's `tool_call_parser`: a model without one
  isn't listed.

Each choice says:

| Field | What it is |
|---|---|
| `group`, `name` | The model; the picker groups a model's choices together |
| `source`, `origin` | Where it comes from: "tt catalog" |
| `chips`, `context` | Chips it uses and its context window (Code uses up to 131K of it) |
| `tools` | `verified`, and how it's known ("tool parser openai") or why it isn't |
| `prefix_cache` | `on`, `off` or `unknown`, and how it's known: the live server for the model serving now, else what was seen when it last served here, else its launch flags (a DFlash profile, or `--no-enable-prefix-caching`, is off) |
| `speed` | The median decode speed and time to first token Code measured on the box in that setup, or null |
| `reliability` | The latest check-up's agent reliability (Evals): valid tool calls within one retry, and the window it held up to, or null |
| `choice` | `in_use`, `switch`, `busy` (with why) or `unavailable` (with why) |
| `serve` | The route and body that start it (null for the one in use) |

The model in use comes first, then the rest. A model serving by a path the list doesn't know
is still shown first, as `source: "running"`.

## Speeds and check-ups per setup

Code's speed book (`code-speed.json` in the agent's config folder) keeps each model's
measurements under the setup that served it (the model's name; entries recorded as
`<model> @ <package>#<profile>` by older versions keep their key). Check-ups (`evals/<id>.json`) record the same
`setup`. Entries and check-ups from before the box kept the setup are shown for a setup that
has none of its own, marked **setup not recorded**.

## Switching

**Use for Code** opens a confirm in place that says what stops (the model serving now, for
everyone using the box: Chat, Code, apps and connected clients), how many chips the new choice
uses and how long it takes to load. Confirming calls the choice's `serve` route, the same start Models uses, so that
route's permission checks, the model-control setting, chip leases and refusals (weights
missing, chips held by someone else, a start already under way) apply unchanged. The picker
only ever calls `/serve`.

Keyboard: every choice's **Use for Code** button is in the tab order; the confirm takes focus on open, Escape cancels it and returns focus to
the choice's button.
