# Agent reliability: gpt-oss-20b tool calls vs. session length

**Date:** 2026-10-15 · **Model:** `openai/gpt-oss-20b` (vLLM 0.26.0.dev4, `--tool-call-parser openai`,
max_model_len 131072, prefix caching on) on the QuietBox at `localhost:8003` · **Code:** branch
`jberkowitz/reliability-eval`, commit `9ca80c1` · **Raw data:** [`agent-reliability-gpt-oss-20b.json`](agent-reliability-gpt-oss-20b.json)
(report + all 300 graded requests)

## Question

Code sessions (Claude Code → the agent's Anthropic-to-OpenAI translator → gpt-oss-20b) worked for
hundreds of tool calls, then around 85K prompt tokens the model started writing calls as text,
inventing tools and inventing parameters. Sessions were set to compact at a guessed 64K. How does
tool-call validity actually change with prompt length, and where should a session compact?

## Method

- **Conversations.** Each trial is a Claude Code-like request: a coding-agent system prompt (written
  for this eval), the six tools Claude Code offers with their real parameter names (Bash, Read
  `{file_path, offset, limit}`, Edit, Write, Grep, Glob; descriptions in our own words), then a
  synthetic session: a task, then earlier tool calls and results (generated Rust/Python/TypeScript
  files read with numbered lines, test runs, git status/diff, searches, edits), grown until the
  prompt reaches the target size, then a final user turn that needs a tool. 12 final tasks (read a
  function, read lines A–B, run tests, find a definition, list files, git status, write a file,
  rename, count lines, find TODOs, read first 40 lines, search a file), varied across trials.
  Everything is generated from a seed; nothing comes from real session transcripts.
- **Sizes.** 8K, 16K, 32K, 64K, 96K, 120K prompt tokens. Sizes are calibrated against the server's
  own `usage.prompt_tokens` (vLLM's `/tokenize` renders gpt-oss's chat ~10% longer than the
  harmony prompt it actually runs, so it can't be used). Median measured sizes are in the table.
- **Request shape.** Built as an Anthropic request and converted with the translator's own
  `anthropic::to_openai` and `fit_output`, with what Claude Code sent when captured on this box:
  `max_tokens` 16384 (capped to the room left), `thinking: adaptive` (so `enable_thinking: false`),
  streamed, `tool_choice: auto`, and **no temperature** — the server's default sampling.
- **Trials.** Per size: 10 independently generated sessions × 5 different final tasks = 50 requests,
  300 in all, at most 4 in flight. Within a session the 5 requests share the history (and the
  server's prefix cache), so they are not fully independent; see caveats.
- **Grading** (of the reply as the server returned it):
  - *valid*: every call names an offered tool and its arguments fit the schema (required present,
    no unknown keys — the schemas set `additionalProperties: false` — right types, enums);
  - *leaked*: no structured call, but the text holds one (the translator's leak markers:
    `to=functions.`, `assistantcommentary`, …);
  - *invented*: a call to a tool name that wasn't offered;
  - *invalid args*: a known tool with arguments that don't fit;
  - *no call*: a text answer.
  - Also recorded: whether the translator's leak salvage would turn a leaked call into a valid one,
    and whether the tool suits the task.
- **Recommended window rule:** the largest tested size at which it **and every smaller tested
  size** had ≥95% valid calls, with ≥20 graded trials each, counting calls before any salvage.
  (The "every smaller size" clause stops a lucky large size from counting past a failing one.)

## Results

| Prompt size (target) | Measured (median) | Valid | 95% CI (Wilson) | Leaked as text | Invented tool | Invalid args | No call | Median latency |
|---|---|---|---|---|---|---|---|---|
| 8K | 8,170 | **86%** (43/50) | 74–93% | 0% | 6% | 8% | 0% | 6.6 s |
| 16K | 16,107 | **90%** (45/50) | 79–96% | 2% | 2% | 4% | 2% | 7.1 s |
| 32K | 32,599 | **82%** (41/50) | 69–90% | 0% | 6% | 12% | 0% | 7.5 s |
| 64K | 65,406 | **88%** (44/50) | 76–94% | 2% | 0% | 10% | 0% | 7.7 s |
| 96K | 97,820 | **72%** (36/50) | 58–83% | 2% | 6% | 14% | 6% | 10.0 s |
| 120K | 123,202 | **70%** (35/50) | 56–81% | 2% | 4% | 22% | 2% | 9.6 s |

No requests failed. Median latency is the whole streamed request with the session already in the
prefix cache (as for the next step of a real session). Reading a session from an empty cache took a
median 2.7 s (8K), 12.9 s (32K), 21.6 s (64K), 29.8 s (96K) and 37.2 s (120K), measured with
other requests in flight.

**Recommended compaction window: none.** No tested size reached 95% valid calls, so by the rule
there is no measured window and Code sessions on gpt-oss-20b keep the 64K default.

What the data does show:

1. **A plateau, then a drop after 64K.** 8K–64K pooled: 173/200 valid (86.5%); 96K–120K pooled:
   71/100 (71%); two-proportion z ≈ 3.2. Within 8K–64K the differences are within noise.
2. **Even short sessions fail ~1 call in 8.** Every "invented tool" in this run (12 of 300, at
   every size but 64K) is a real tool name with harmony tokens glued on
   (`Read<|channel|>commentary`, `Grepjson`), not a new tool; the rest are invented parameters.
   (The pilot run below also had one genuinely invented tool, `GitCommit`.)
3. **The dominant long-context failure is invented parameters**, above all Read with
   `line_start`/`line_end` instead of `offset`/`limit` (19 of 300 replies, 10 of them at 96K–120K),
   plus Grep with `limit`, `recursive`, `regex`, `depth` or no `pattern`. This matches the real
   session.
4. **Calls leaked as text are rare here** (5 of 300, 1–2% per size from 16K up), and the
   translator's salvage would have recovered **none** of those 5 as valid calls (wrong names like
   `Git status`, or arguments that don't fit).
5. **A pilot run** (same method, before a sizing fix that left one 120K session too large to send)
   gave 86 / 90 / 86 / 82 / 74 / 69% valid, the same picture.

## Caveats: what was and wasn't measured

- **One reply per request.** This measures whether the first reply to a request is a valid call.
  In a real session Claude Code returns a schema error and the model usually retries, so an
  invalid-args reply often costs a turn rather than ending the task. Recovery wasn't measured.
- **Synthetic sessions.** The history is generated: plausible, but more repetitive than real
  work, and every earlier call in it is valid. Real sessions that already contain malformed calls
  may degrade sooner. Real Code sessions on this box start at ~15K tokens (Claude Code's own
  system prompt, tools and project notes); this eval's fixed part is ~2K.
- **Intervals are approximate.** Wilson intervals assume 50 independent trials per size; trials
  share 10 sessions. A bootstrap over sessions gives similar widths here (e.g. 64K: 82–94%), but
  with only 10 clusters treat ±10 points as the real resolution. The 95% bar can't be shown with
  50 trials per size unless ≤2 fail.
- **Sampling.** The server's default sampling (temperature 1.0, as Claude Code requests) — a
  temperature-0 run would score differently.
- **One model, one server build, one box**, shared with other users during the run (latencies
  include that contention). The grading is on the server's parsed tool calls; garbled names are a
  model/parser interaction, not separated from the model here.
- **Not measured:** sizes between the tested points, task success (only call validity), Edit
  correctness, multi-step recovery, other models.

## Reproduce

```sh
cargo run --release -p tt-station-agentd --example agent_reliability -- \
  --url http://localhost:8003/v1 --histories 10 --tasks 5 --concurrency 4 --seed 1 \
  --out agent-reliability.json
```

The same eval runs as the "Agent reliability" part of a model check-up in TT Station (Evals page,
4 sessions × 5 tasks per size), stored with the check-up in `~/.config/tt-station/evals/`
(graded requests in `evals/raw/`). Code sessions then compact at that model's recommended
window (never below 32K), or 64K when none was recommended.
