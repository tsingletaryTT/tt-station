# Agent reliability: gpt-oss-20b tool calls vs. session length

**Date:** 2026-10-15 · **Model:** `openai/gpt-oss-20b` (vLLM 0.26.0.dev4, `--tool-call-parser openai`,
max_model_len 131072, prefix caching on) on the QuietBox at `localhost:8003` · **Code:** branch
`jberkowitz/reliability-eval`, commit `9ca80c1` · **Raw data:** [`agent-reliability-gpt-oss-20b.json`](agent-reliability-gpt-oss-20b.json)
(report + all 300 graded requests)

> **Update (same day): recovery-aware scoring.** The first-reply metric below undercounts what a
> Code session gets, because Claude Code sends a schema error back and the model retries. See
> [Recovery: what a Code session actually gets](#recovery-what-a-code-session-actually-gets).
> Per the new rule, the pooled result recommends **64K**: ≥98% of trials get a valid call within
> one retry at every size up to 64K. 96K was borderline (96% on one set of sessions, 92% on
> another) and 120K fails (78%).

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

**Recommended compaction window (first-reply rule): none.** No tested size reached 95% valid
calls, so by that rule there is no measured window and Code sessions on gpt-oss-20b keep the 64K
default. (The recovery-aware rule below replaces this one for the recommendation.)

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
  invalid-args reply often costs a turn rather than ending the task. Recovery wasn't measured in
  this run. It is measured in the section below.
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

## Recovery: what a Code session actually gets

**Date:** 2026-10-15 (afternoon) · same model, server, box and eval sessions as above · **Code:**
branch `jberkowitz/eval-recovery`, commit `366434d` · **Raw data:**
[`agent-reliability-gpt-oss-20b-recovery.json`](agent-reliability-gpt-oss-20b-recovery.json)
(seed 1, all six sizes: 300 trials with every retry) and
[`agent-reliability-gpt-oss-20b-recovery-seed2.json`](agent-reliability-gpt-oss-20b-recovery-seed2.json)
(seed 2, 64K and 96K only: 100 trials).

### What changed in the method

Every trial still sends the same first request and grades the reply exactly as before. That gives
the **raw** column, comparable with the first run. The trial then continues the way a Code session
would:

1. **Translator repair.** The server's stream is replayed through the translator's own streaming
   path (`anthropic::Stream`, the code `/v1/messages` runs for Claude Code). Thinking is hidden,
   because thinking isn't asked for. The output is rebuilt into the Anthropic message Claude Code
   would have received. That path does three things:
   - it salvages calls leaked as text;
   - it strips harmony residue from tool names (`Read<|channel|>commentary` → `Read`) and fixes their case;
   - it turns numeric and JSON strings into the types the schema wants, and repairs broken JSON where it safely can.

   The checks run uncounted (`ToolCheck::uncounted`), so eval calls don't appear in the box's
   tool-call health.
2. **Claude Code's error, then ask again.** If a call is still invalid, the translated assistant
   turn goes into the session. It is followed by a `tool_result` (`is_error: true`) worded the way
   Claude Code 2.1.x words it on this box:
   - unknown tool: `Error: No such tool available: <name>`;
   - bad parameters: `InputValidationError: <Tool> failed due to the following issue(s):`
     followed by `The required parameter `x` is missing` and/or
     `An unexpected parameter `x` was provided`;
   - both wrapped in `<tool_use_error>…</tool_use_error>`.

   Wording was learned from the Code session transcripts on the box; none of their content is in
   the eval. The model is then asked again, up to **2 retries**. A valid sibling call in a mixed
   reply would get a neutral `(no output)` result; none of the 78 retries needed one.
3. **A text answer ends the turn.** If the reply has no call at all, even after salvage, Claude
   Code shows the text and stops. There is nothing to retry, so the trial fails.

Per size the eval reports first reply valid (raw), valid after translator repair, valid within
1 retry, valid within 2 retries, and still failing.

**New recommendation rule:** the largest tested size at which it, and every smaller size, had
**≥95% of trials with a valid call after the translator's repair and at most one retry** (≥20
trials each). Why this metric:

- **After repair:** this is what Claude Code actually receives. Nothing upstream of it is visible to the user.
- **At most one retry:** one bounced call costs a round trip, ~12 s here (median per retry). A
  session that needs two retries for a simple read is visibly struggling. Two retries would also
  hide real degradation; at 120K, two retries lift 78% to 88%.
- **95%:** kept from the first rule. A session makes hundreds of calls, so even 5% of turns
  needing a second retry or ending in text is noticeable.

The raw first-reply window is still computed and reported (`raw_window`) for comparison.

### Results

Seed 1: the same 10 sessions × 5 last turns per size as the first run (same seed; the task
lists are identical and 210 of 300 prompts are the same size to the token). 50 trials per size, 300 in all.

| Prompt size | Measured (median) | First reply valid (raw) | After translator repair | **Within 1 retry** | 95% CI (1 retry) | Within 2 retries | Still failing | of which: text, no call |
|---|---|---|---|---|---|---|---|---|
| 8K | 8,289 | 84% (42/50) | 88% (44/50) | **98%** (49/50) | 90–100% | 98% | 1 | 0 |
| 16K | 16,976 | 86% (43/50) | 92% (46/50) | **100%** (50/50) | 93–100% | 100% | 0 | 0 |
| 32K | 32,592 | 72% (36/50) | 74% (37/50) | **98%** (49/50) | 90–100% | 98% | 1 | 1 |
| 64K | 65,756 | 88% (44/50) | 88% (44/50) | **100%** (50/50) | 93–100% | 100% | 0 | 0 |
| 96K | 97,357 | 88% (44/50) | 88% (44/50) | **96%** (48/50) | 87–99% | 96% | 2 | 2 |
| 120K | 122,777 | 58% (29/50) | 60% (30/50) | **78%** (39/50) | 65–87% | 88% (44/50) | 6 | 6 |

By the rule alone, this run gives **96K**. 96K sits right at the line: two failures pass, a third
would not. So 64K and 96K were re-run on 10 new sessions each (seed 2, 50 trials per size):

| Prompt size | Measured (median) | First reply valid (raw) | After repair | **Within 1 retry** | 95% CI | Within 2 retries | Still failing (all text, no call) |
|---|---|---|---|---|---|---|---|
| 64K | 65,449 | 72% (36/50) | 72% (36/50) | **98%** (49/50) | 90–100% | 98% | 1 |
| 96K | 98,249 | 66% (33/50) | 70% (35/50) | **92%** (46/50) | 81–97% | 94% | 3 |

Pooled over both sets of sessions (100 trials each), within one retry:

| Size | Within 1 retry |
|---|---|
| 64K | **99/100** |
| 96K | **94/100**, below the bar |

No requests failed in either run.

**Recommended compaction window: 64K.** By the stated rule, 64K holds with margin on both sets of
sessions, and 96K does not hold once pooled. The seed-1 run's 96K verdict was luck of the
sessions. This is the first time 64K is a measured window rather than a default. The number
itself doesn't change.

**What Code will now do:** the same as before, compact at 64K. `CLAUDE_CODE_AUTO_COMPACT_WINDOW`
comes from the latest agent-reliability check-up saved for the model, else 64K. These runs were
not saved as a check-up, so nothing on the box changed. A check-up run from the Evals page uses
the new rule (and the Evals page shows the new columns). It has only 20 trials per size, and at
96K the true rate is about 94–96%, so a check-up could still land on 96K by chance. Read a 96K
check-up result for gpt-oss-20b with that in mind, or re-run it with more sessions
(`--histories 10 --save-eval ~/.config/tt-station/evals`).

What the data shows:

1. **Retries fix almost all invalid calls.** Over both runs (400 trials):
   - 64 first replies had invalid arguments: 1 was mended by the translator, 57 were valid after
     one retry, 5 after two, and 1 never;
   - 13 called a tool by a wrong name: 6 were mended by the translator, 6 were valid after one
     retry, and 1 after two.

   The dominant error, Read with `line_start`/`line_end` (33 of 400 first replies), was corrected
   on the first retry 30 times and on the second 3 times. The model only needed to see
   `An unexpected parameter `line_end` was provided`.
2. **The translator's repair matters most for short sessions.** It made 7 of 300 seed-1 trials
   valid with no retry:
   - five harmony-suffixed names (`Bash<|channel|>commentary`) at 8–32K;
   - two salvaged leaks, at 16K and 120K.

   At 64K and 96K it changed nothing. When a suffixed name also has bad arguments
   (`Read<|channel|>commentary` with `line_end`), the name is mended and the arguments recover on
   retry.
3. **The failures that remain are text answers, not bad calls.** 14 trials never got a valid call.
   13 of them ended with the model answering in prose instead of calling a tool, and 11 of those
   were at 96K–120K. Typically it **made up the tool's output**: a git status, a file list, a
   function body, test failures. Claude Code can't catch this with a schema error, so retries
   don't help. It is the real long-context failure, and it grows fast past 96K (6 of 50 trials at
   120K).
4. **First-reply rates are noisy run to run.** The same seed-1 sessions scored 86/90/82/88/72/70%
   raw this morning and 84/86/72/88/88/58% this afternoon (temperature 1.0). 96K moved +16 points
   and 120K −12. Seed 2's 64K came in at 72% raw. Single 50-trial cells move ±10–15 points, so
   compare pooled numbers, not single cells.

### Caveats specific to recovery

- **Validity only, not task success.** A recovered call is counted if it fits the schema. Whether
  the retried call suits the task wasn't graded; `on_task` is graded for first replies only.
- **Error wording.** The unknown-tool, missing-parameter and unexpected-parameter messages match
  Claude Code's own as seen on this box. The wrong-type and enum messages
  (``The parameter `x` type is expected as `number` but provided as `string` ``, ``The parameter
  `x` must be one of …``) weren't seen in a transcript and follow the same pattern. They were
  sent 2 times in 78 retries.
- **Translator repair is replayed, not live.** The eval feeds the recorded stream through the same
  `Stream` code after the reply ends, rather than through `/v1/messages`. The output is the same,
  but streaming timing isn't exercised.
- **The rest of the first run's caveats apply:** synthetic sessions, shared sessions within a size
  (10 clusters per run), default sampling, one model and box, and a ~2K fixed prompt where real
  Code sessions start near 15K.

## Reproduce

```sh
cargo run --release -p tt-station-agentd --example agent_reliability -- \
  --url http://localhost:8003/v1 --histories 10 --tasks 5 --concurrency 4 --seed 1 \
  --out agent-reliability.json
# recovery run (branch jberkowitz/eval-recovery): retries default to 2; the second set of sessions
#   ... --seed 2 --lengths 65536,98304
```

The same eval runs as the "Agent reliability" part of a model check-up in TT Station (Evals page,
4 sessions × 5 tasks per size), stored with the check-up in `~/.config/tt-station/evals/`
(graded requests in `evals/raw/`). Code sessions then compact at that model's recommended
window (never below 32K), or 64K when none was recommended.
