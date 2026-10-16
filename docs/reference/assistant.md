# `/assistant`, `/assistant/confirm`, and `tt-station ask` (reference)

*Documents the shipped behavior of `crates/tt-station-agentd/src/assistant.rs`, the
`POST /assistant` / `POST /assistant/confirm` handlers in
`crates/tt-station-agentd/src/routes.rs`, and the `tt-station ask` subcommand in
`crates/tt-station/src/main.rs`. The first client is the iPhone prototype in `ios/`.*

## Why this exists

Every other route asks the caller to know what it wants in agent terms: a model id for
`/run`, a power action for `/power`. A phone is a bad place for that. The assistant lets
a person ask in plain language ("what's running and how hot is it?", "switch to Llama")
and has **the model this box is currently serving** answer, using OpenAI-style tool
calls to read and control the box.

The served model is the brain; the agent is the hands. The agent, not the model,
decides which hands may move without a human saying yes.

## Which model to serve

The assistant is only as good as the served model's tool calling. Measured on a P300x2
QuietBox with a 10-command test (status questions, start/stop/reset requests, a
misheard "Lama 3", and two small-talk prompts that must *not* call a tool):

| Model | Score | Notes |
|---|---|---|
| Qwen3-32B (`enable_thinking: false`) | 10/10, three runs | ~1.3 s per command |
| Llama-3.1-8B-Instruct | 5–9/10, varied between identical runs | Forces tools onto small talk; claimed actions it never took |

The agent's `runpy` backend already starts vLLM with a tool-call parser for known model
families (`tool_call_parser_for` in `serving/runpy.rs`; `hermes` for Qwen3).

## The confirmation model

Tools come in two kinds:

| Tool | Kind | What it does |
|---|---|---|
| `box_status` | read-only | Box name, chips, device mesh, the served model, and per-device temperature/power. |
| `list_models` | read-only | The models this box's mesh can serve (names, capped at 40). |
| `recent_logs` | read-only | Tail of the serving container log (`lines` 1–50, default 10); same source as `GET /logs`, compacted (vLLM's `(EngineCore pid=…)` prefix dropped, lines capped at 200 chars, 4,000 chars total, newest kept). |
| `run_model` | **disruptive** | Serve a different model. Validated against the box's model list first; a name that matches nothing ("Lama 3 8B instruct") gets up to five closest real names back, and the model retries with the exact one in the same turn. |
| `stop_model` | **disruptive** | Stop serving. |
| `reset_chips` | **disruptive** | Stop serving, then `tt-smi -r` (the `POST /power reset-chips` body, **not** `POST /reset`, which would also unpair every client). |

- **Read-only** tools run immediately. Their JSON result goes back to the model as a
  `role: tool` message and the model gets another turn, up to 4 tool rounds; a final
  round without tools forces a plain answer. Each run is reported as an action with
  status `done` (or `error`).
- **Disruptive** tools are **never** run from the chat. The call is validated, parked
  as a pending action under a random id, reported as `needs_confirmation`, and the turn
  ends. Only `POST /assistant/confirm` with that id runs it, through the same
  `start_serving` / `stop_serving` / `run_power_command` code that `/run`, `/stop` and
  `/power` use. Pending actions expire after **5 minutes** (at most 16 are held) and are
  consumed by the first confirm call, approved or not.
- A tool name the model invents gets an error listing the real tools, and the loop
  continues.

### The assistant runs on the thing it controls

Every disruptive action takes the served model down: `run_model` replaces it (minutes to
load), `stop_model` removes it, `reset_chips` resets the silicon under it. Each pending
action's summary says so, so the human confirms knowing the assistant will go quiet.

A confirmed `run_model` runs **in the background** (`/run` blocks until the new model is
healthy) and the confirm call returns `started` right away. Clients poll `GET /status`
and treat `serving:<model>` as "the assistant is back".

### Telemetry

`box_status` reuses a `tt-smi -s` snapshot younger than 30 s if a `/telemetry` viewer
already paid for one. Otherwise it takes one fresh, deduplicated snapshot through the
same cache `/telemetry` uses (~0.2 s on a P300x2 box with a model serving), bounded by a
5 s timeout. If that fails, the result carries a `telemetry_note` instead of `devices`,
and the model says temperatures are unavailable rather than inventing them.

### Garbled output

Qwen3-32B on a P300x2 box has been seen to emit garbage intermittently: runs of
`<|im_start|>` or `! ! ! !` instead of an answer. A plain prompt broke at ~3,600 tokens
while the same request at ~2,200 was fine, and a ~3,000-token tool request worked on a
retry. It's a model-side fault, so the agent defends against it:

- chat-template control tokens (`<|…|>`) are stripped from every reply;
- a reply that is empty or dominated by one repeated token is **retried once**;
- if the retry is garbage too, the client gets "Sorry, the model returned garbled
  output. Please try asking again." and the agent logs it;
- tool results are kept small (see `recent_logs`), because prompt size is the trigger.

## `POST /assistant` (authed)

Request; the client keeps the history and the server forwards only the last 20 turns:

```json
{ "messages": [ { "role": "user", "content": "what's running, and how hot are the chips?" } ] }
```

Only `user` and `assistant` roles are accepted. The agent adds its own system prompt
with live box context (name, chips, mesh, served model), asks for 1–3 sentence replies
with no markdown (they may be spoken aloud), and tells the model never to claim an
action happened unless a tool result says so. Requests go to the served endpoint's
`/chat/completions` with `temperature: 0`, `max_tokens: 400`, and
`chat_template_kwargs: {"enable_thinking": false}`; any `<think>…</think>` block in the
reply is stripped anyway.

Response `200`:

```json
{
  "reply": "Qwen3-32B is serving. The four chips are between 41 and 48 °C.",
  "actions": [
    { "id": "3f9c…", "tool": "box_status", "args": {}, "status": "done",
      "summary": "Checked the box status" }
  ],
  "model": "Qwen/Qwen3-32B"
}
```

A disruptive request instead returns an action with `"status": "needs_confirmation"`, a
`summary` spelling out the consequence, and that action's `id` to confirm with.

## `POST /assistant/confirm` (authed)

```json
{ "action_id": "3f9c…", "approve": true }
```

Response `200`:

```json
{ "action_id": "3f9c…", "tool": "run_model", "status": "started",
  "summary": "Starting meta-llama/Llama-3.1-8B-Instruct — this takes a few minutes; I'll be offline until it's ready." }
```

`status` is `cancelled` (`approve: false`), `started` (`run_model`, backgrounded) or
`done` (`stop_model`, `reset_chips`).

`reset_chips` is an owner's (`403`). `run_model`, `stop_model` and `switch_mode` follow who
may start and stop models (`model_control`, `docs/reference/your-data.md`): with `owners`, a
member's turn is told "Only owners can start or stop models on this box." instead of getting
a card, and approving a card offered before the change is `403` with that message.

## More tools: the web, memory, pictures

All run immediately (no confirmation) and show as a chip in the client.

| Tool | |
|---|---|
| `web_search {query}` | DuckDuckGo's keyless HTML endpoint, top 6 `{title, url, snippet}`. |
| `read_webpage {url}` | A public page's readable text (the `<article>`/`<main>` when present; no scripts or navigation), ≤ 6000 characters. Addresses on the box's own networks -- loopback, private, link-local, Tailscale -- are refused, including after redirects, so neither a prompt nor a page can steer it at the agent's API or the LAN. |
| `remember {fact}` / `forget {about}` | Durable facts about the owner in `$TT_CONFIG_DIR/memories.json` (0600, newest 60, near-duplicates merged), added to every system prompt. `GET /memories`, `DELETE /memories`, `DELETE /memories/{id}` (authed) let the owner see and prune them. |
| `create_image {prompt, count?}` | See [gateway.md](gateway.md#images-in-chat-create_image). |

`TT_STATION_NO_WEB=1` turns the web tools off. Web text can carry instructions
aimed at the model; the confirmation model still holds, since every disruptive
action waits for the human's Confirm.

## Documents (`/docs`)

Private Q&A over the user's own files. Clients upload extracted text; the agent
splits it into ~900-character passages (150 overlap) and stores one JSON file per
document in `$TT_CONFIG_DIR/documents` (default `~/.config/tt-station/documents`).
The assistant's read-only `search_documents` tool ranks passages with BM25 and
returns the top five; the system prompt lists the documents' names, so questions
about "my lease" go to the tool rather than a guess. Retrieval is lexical on
purpose: the box serves one model at a time, so no embedding model runs beside the
chat model.

| Route (all authed) | |
|---|---|
| `GET /docs` | `{documents: [{id, name, added, chars, passages}]}`, newest first |
| `POST /docs` `{name, text}` | `201` + summary; replaces a document of the same name. `400` empty name/text, `413` over 2M characters (body limit 8 MB) |
| `DELETE /docs/{id}` | `204`, or `404` |
| `POST /docs/search` `{query, document?, top_k?}` | `{hits: [{document, passage, score, text}]}` — what the tool sees |

## QR pairing (`tt-station-agentd pair`, `POST /pair/offer`)

At startup the agent writes `local-secret` (mode 0600: secret, port, box name) to
its config directory. `tt-station-agentd pair`, run on the box as the same user,
presents it to `POST /pair/offer` (header `x-tt-local-secret`), which mints a
pending pair like `/pair/init` but returns the code too. The command prints a QR of
`ttstation://pair?name=&host=<LAN IPv4>&port=&pair=&code=[&remote=<Tailscale IPv4>]`;
the phone completes `/pair/complete` with it. Wrong or missing secret: `403`.

## Errors

All errors are `{ "error": "<human-readable message>" }`.

| Status | When |
|---|---|
| `401` | Missing or unknown bearer token (pair first). |
| `400` | Empty history or an unsupported role. |
| `404` | Confirming an id that was never issued, already answered, or expired. |
| `409` | `/assistant` while nothing is serving ("No model is serving — start one first"); or a confirmed action refused because another tenant holds the chips (same as `/run`). |
| `502` | The served model was unreachable, answered non-2xx, or returned something unparseable. |
| `500` | A confirmed action failed in the backend. |

## `tt-station ask`

```bash
tt-station ask --host qb2-lab.local:8765 "what's running?"
tt-station ask --host qb2-lab.local:8765 --confirm <action-id>          # approve
tt-station ask --host qb2-lab.local:8765 --confirm <action-id> --deny   # refuse
```

Single-turn: each `ask` sends just that one message. Human output prints the reply and
each action; a pending action is printed with the exact commands that approve or refuse
it. `--json` prints the response as JSON (same fields as the route). Needs
`tt-station pair` first.

## Testing

- `crates/tt-station-agentd/tests/assistant.rs`: the routes against a scripted fake
  `/v1/chat/completions` server and a fake backend: idle `409`, `401`, read-only loop,
  park-don't-run, confirm/cancel/`404`, unknown tools, `<think>` stripping. `tt-smi` is
  pointed at a nonexistent path so no test touches a real board.
- `crates/tt-station/tests/e2e_mock.rs::tt_ask_and_confirm_round_trip_against_mock_box`:
  the CLI against `mock-box`'s canned assistant (`--ignored`, like the other e2e tests).
