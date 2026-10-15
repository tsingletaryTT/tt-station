# Does vLLM constrain gpt-oss tool-call arguments? (No.)

**Date:** 2026-10-15
**Server:** vLLM `0.26.0.dev4+g2bbd2a6d9`, `openai/gpt-oss-20b`, on this QuietBox at `:8003`,
started with `--enable-auto-tool-choice --tool-call-parser openai --structured-outputs-config
{"reasoning_parser": "openai_gptoss", "enable_in_reasoning": false}`
**Script:** [`tool-call-constrained-decoding.py`](tool-call-constrained-decoding.py) (standard
library only; raw results were kept out of the repo)

## Question

Claude Code reaches local models through the agent's Anthropic translator
(`crates/tt-station-agentd/src/anthropic.rs`), which maps Anthropic `tool_choice` to OpenAI's:
`auto` → `"auto"`, `any` → `"required"`, `tool` → a named function. If vLLM held tool-call
arguments to the tool's JSON schema in some mode, the translator could lean on it instead of
checking calls itself. Does it, on this server, and at what cost?

## Method

Six Claude Code-shaped tools (Bash, Read `{file_path, offset, limit}`, Edit, Write, Grep, Glob),
every schema `additionalProperties: false` with its `required` list, a short Claude Code-like
system prompt, `temperature: 1.0`, and `chat_template_kwargs.enable_thinking: false` (what the
translator sends). Four modes:

| Mode | `tool_choice` |
|---|---|
| auto | `"auto"` (what the translator sends for ordinary Claude Code turns) |
| auto+strict | `"auto"`, each function marked `"strict": true` |
| required | `"required"` |
| named | `{"type": "function", "function": {"name": <the tool the task needs>}}` |

Two prompt sets, sent twice each in two rounds (36 requests a mode, 144 in all), shuffled across
modes and sent at concurrency 4 so load hit every mode alike:

- **ordinary** (24 a mode): six everyday tasks ("show me lines 470 to 520 of …", "find every use
  of parse_args …", "run the unit tests with a 10 minute timeout" …).
- **tempting** (12 a mode): the user asks for arguments the schema forbids (`line_start`/
  `line_end` on Read; `offset` as the string `"40"`; Grep `output_mode: "lines"`). A decoder held
  to the schema *cannot* emit these; an unconstrained one may.

A call "fits" when its arguments parse as JSON and pass the same checks the translator's
`ToolCheck` makes (tool exists, required present, primitive types, enums, no unknown properties),
before any repair.

## Results

| Mode | Set | n | Returned a call | Fits schema | Named tool honoured | p50 latency | mean latency | p50 output tokens |
|---|---|---|---|---|---|---|---|---|
| auto | ordinary | 24 | 24 | 22 (92%) | — | 7.3 s | 8.9 s | 62 |
| auto | tempting | 12 | 12 | 10 (83%) | — | 17.6 s | 24.9 s | 223 |
| auto+strict | ordinary | 24 | 24 | 21 (88%) | — | 6.3 s | 11.2 s | 66 |
| auto+strict | tempting | 12 | 11 | 3 (27%) | — | 15.1 s | 19.5 s | 134 |
| required | ordinary | 24 | 24 | 23 (96%) | — | 10.1 s | 14.6 s | 78 |
| required | tempting | 12 | **9** | 8 (89%) | — | 16.7 s | 24.0 s | 202 |
| named | ordinary | 24 | 24 | 21 (88%) | **17 of 24** | 6.3 s | 7.9 s | 67 |
| named | tempting | 12 | 12 | 10 (83%) | 12 of 12 | 20.7 s | 33.1 s | 148 |

Over all 144 requests: 140 calls, 22 of them not fitting their schema. What went wrong:
`line_start`/`line_end` on Read (7), `i` for Grep's `-i` (6), an `output_mode` outside the enum
(3), `head_limit: null` (2), a missing required `pattern`, `offset`/`limit` as strings, an
unknown `glob` on Glob, and one call whose arguments weren't JSON.

What the numbers say:

1. **No mode constrains the arguments.** Schema-forbidden arguments came back in every mode:
   `line_start`/`line_end` under `required` and under a Read-named `tool_choice`, string
   `offset`/`limit` and an out-of-enum `output_mode` under `strict`, and an unparseable call
   under `strict`. Each is impossible for a schema-bound decoder.
2. **`tool_choice` isn't enforced either.** With a named tool, 7 of 24 ordinary replies called a
   *different* tool (asked for Edit, got Read; asked for Bash, got Grep). Under `required`, 3 of
   12 tempting replies had no call at all, just text ("I'm sorry, the tool expects `offset` and
   `limit` …") with `finish_reason: "tool_calls"`.
3. **The structured-output engine itself works.** Three requests with `response_format:
   json_schema` (`{offset: integer, limit: integer}`, prompt asking for `line_start`/`line_end`
   strings) all came back as `{"offset": 0, "limit": …}`. So the gap is in vLLM's tool path for
   gpt-oss (harmony format, `openai` tool parser). It isn't that structured outputs are off.
4. **Fit rates are statistically the same** across auto, required and named on ordinary work
   (88–96%, n=24 each; a one-call difference is 4 points). `strict` did no better, and on
   tempting prompts it did worse (27%, n=11). That gap is likely noise or the model reading
   `strict` as "do as the user says". It isn't a constraint effect either way.
5. **Latency follows output length, not mode.** Medians are within a few seconds of each other.
   `required` ran a little slower (10.1 s against 7.3 s for ordinary work), at concurrency 4 on a
   shared server, which is about the noise between our two rounds.

Of the 22 calls that didn't fit, the translator's safe repairs (case-only name fix, unquoting
clean numbers or booleans, decoding JSON-in-a-string) would have fixed 1: the `"40"`/`"20"`
offset and limit. The rest need guesses (`i` → `-i`, `line_start` → `offset`, dropping `null`)
that could change what the model meant, and the translator rightly leaves those to Claude Code's
own error message.

## Recommendation

- **Keep the translator's `tool_choice` mapping as it is.** `"auto"` for ordinary turns.
  Forcing `"required"` would buy nothing here (it constrains nothing) and would break Claude
  Code, which must be able to answer in plain text. Named and `required` stay only where Claude
  Code asks for them.
- **Don't send `strict: true`.** It isn't enforced, and it didn't help.
- **Rely on the translator's own check** (`ToolCheck` in `anthropic.rs`) for safe repairs and
  for counting tool-call health per model. On this model, invented parameters (`line_start`,
  `i`) are the main failure, and no safe repair fixes them. Claude Code's error reply already
  does that job.
- **Translator fix made from this data:** a reply with `finish_reason: "tool_calls"` but no call
  in it (finding 2) used to be reported to Claude Code as `stop_reason: "tool_use"` with nothing
  to run. It is now `end_turn`.
- **Worth raising upstream (vLLM):** with gpt-oss + `--tool-call-parser openai`, `tool_choice:
  "required"` and named tool choice are not enforced, though `response_format` is. If that's
  fixed, re-run this script: a named or required call that's truly schema-bound would be worth
  using where Claude Code forces a tool.

## Limits

One model (gpt-oss-20b) and one vLLM build. Small samples: n=24 or 12 a cell, enough to show
constraint is absent (one forbidden argument settles that) but not to rank the modes'
unconstrained fit rates. Non-streaming requests. Claude Code streams, but the decoding is the
same. Prompts are Claude Code-*like*, not recorded sessions, and the system prompt is much shorter
than Claude Code's. Re-run with `python3 docs/notes/tool-call-constrained-decoding.py --reps 2`
(add `--seed` for a different order).
