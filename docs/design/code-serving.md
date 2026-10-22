# Design: serving Code (agent) workloads on QuietBox 2

*Status: decided 2026-10-15: no default serving setup for Code (see **Decision** below); the
benchmark in §5 was cancelled. The analysis is kept as background. Written 2026-10-15 against `3d50ad6`. The live numbers below
were measured on the QuietBox (`tt-quietbox`) on 2026-10-15 unless another date is given.
Audience: TT Station engineering, and whoever decides how company QuietBoxes are configured.*

*Update (v0.12.0): community packages, native vLLM bundles, Lab (tt-lab) and co-serving were
removed from TT Station before production. The Code picker now lists the `tt` catalog only, and
the mentions of those features below are background, not current behavior.*

## Summary

Code (Claude Code and friends, driven by TT Station) runs on **gpt-oss-20b served by vLLM in
the `p150x2-throughput` profile**. Two facts reframe the question:

1. **That profile uses only 2 of the box's 4 chips.** It is board 0, chips 0 and 1. Board 1
   (chips 2 and 3) sits **idle**: `gozer status` shows chips 2 and 3 FREE. So the
   "throughput vs. 4-chip" choice isn't a trade between users today. Half the box does
   nothing.
2. **The 152–158 tok/s figure doesn't come from a serving stack.** It's tt-lab, bare metal,
   with **prefill ~198 tok/s**. A Code session's first ~15K-token read would take ~75 s on it,
   and a 64K re-read over 5 minutes. tt-lab has no server, no batching and no prefix cache. It
   shows what the chips can do, not something Code can use today.

**Decision (2026-10-15): people pick any working model and profile in Code.** Rather than
benchmarking configs and choosing a default serving setup for Code, the "Model for Code" picker
offers every model on the box that would functionally work, from every source the box serves
from, and the box records speed and check-up results per model and profile so the picker shows
them:

- **Sources:** the `tt` catalog, installed tt-model community packages (each serve profile that
  fits the box is its own choice, e.g. "gpt-oss-20b · 4 chips · low latency"), and native vLLM
  bundles (`codemodels.rs`, `GET /code/models`).
- **Listed when it would work:** a chat (instruct) LLM, its weights on the box, fitting the box's
  chips, with tool calling on its serving path. When a source doesn't say whether it makes tool
  calls, it's listed and marked "tool calling unverified" rather than hidden.
- **Recorded per model and profile:** Code's measured speed (`code-speed.json`, keyed by the
  package and profile that served) and the latest check-up's agent reliability (`evals/`, which
  now record the setup).
- **Switching** goes through each source's own serve route (`POST /serve`,
  `POST /community/serve` with the chosen `profile`, `POST /vllm/native/serve`), so roles,
  permission settings, leases and the confirm dialog apply as they do from Models.

**The benchmark protocol in §5 was cancelled.** The per-profile speed and check-up records
replace it: each profile people actually use for Code gets measured on the box as it runs.

The rest of this document is the analysis that led here. Its earlier recommendation (a 4-chip
default per box role, decided by the §5 benchmark) is withdrawn; its measurements and the
step-time model in §2 still hold.

## 1. What is measured today

**Server configuration** (from `docker inspect` and the image's `/opt/gpt-oss-20b/entrypoint.py`):

| | Value |
|---|---|
| Container | `tt-model-gpt-oss-20b-p150-p150x2-throughput`. A **tt-model community package** (`tt-hous/gpt-oss-20b-p150`, image `2026-10-09-e0046b6a0e80-hub1`), started through `community.rs` → `tt-model serve` |
| Chips | `/dev/tenstorrent/0,1` only (`TT_METAL_VISIBLE_DEVICES=0,1`, `MESH_DEVICE=P150x2`, `FABRIC_1D`). The gozer lease covers chips 0 and 1; chips 2 and 3 are FREE |
| vLLM | `--max-model-len 131072 --max-num-seqs 32 --block-size 64 --enable-prefix-caching --enable-chunked-prefill --async-scheduling`, tool parser `openai` |
| Profiles in the package | `p150`, `p150x2`, `p150x4` (latency) and `*-throughput`. All are 32 seqs and 131072 length. Label: `validation = full-task-gates-passed-three-meshes` |
| Latency vs throughput | The profiles differ **only** in `--max-num-batched-tokens` (8192 vs 32768) and `chunked_prefill_decode_steps` (8 vs 0). The latency profile keeps decode going while long prompts are read; that needs confirming with the package authors. Weights are shared between the two profiles on the same mesh |
| KV pool (full-attention layers) | `GPT_OSS_20B_KV_POOL_BLOCKS`: 8256 blocks × 64 = **~528K tokens** on 2 chips; 16512 → **~1.06M** on 4 chips |
| Topology restriction | 2-chip profiles must use devices `0,1` ("other device sets require topology validation"), so a second 2-chip copy on chips 2 and 3 isn't validated |

**Speed:**

| Metric | Value | Source |
|---|---|---|
| Single-user decode | **69 tok/s** (67 at 29K context); 71 in earlier runs | Measured today: 8 streamed requests, `ignore_eos`, temperature 0 |
| TTFT, short prompt (warm server) | **0.12 s** (0.19 s reported before) | Measured |
| Cold prefill | 4.9K tokens in 1.7–2.4 s (~2.5K tok/s); **28.8K in 5.9 s (~4.9K tok/s)**. Earlier: ~4.5K tok/s on 6K prompts | Measured |
| Fully cached prompt (prefix-cache hit) | **0.71 s at 4.8K, 1.0 s at 28.8K.** The *first* reuse after a cold read was slower: 1.1 s and 2.3 s | Measured. This matches "cached prompts still cost 0.6–1 s". An 8-token prompt costs 0.12 s, so a hit carries **~0.6–0.9 s of fixed overhead**. Not yet explained |
| Session read from an empty cache | 2.7 s (8K), 12.9 s (32K), **21.6 s (64K)**, 37.2 s (120K), with other requests in flight | [agent-reliability note](../notes/agent-reliability-gpt-oss-20b.md) |
| Tool-call reliability | Valid within one retry: **99/100 at 64K, 94/100 at 96K** (pooled). Code compacts at **64K** (`coding.rs` `COMPACT_WINDOW`; window capped at `MAX_WINDOW` 131072) | Reliability note |
| tt-lab bare metal, 4 chips | **152–158 tok/s decode, prefill ~198 tok/s**, 5.8 ms/token device time. One chip: 16.5 ms/token | tt-lab run, 2026-10-07 |
| Prompt : output tokens in Code | **~160:1** for Qwen3.8-27B (2026-10-12). **197:1 pooled** over 26 Code turns on this box (35.4M prompt, 180K output; `~/.config/tt-station/code/*/events.jsonl`) | Measured |

## 2. Where a Code step's time goes

Each step re-sends the whole conversation, and prefix caching (`codespeed.rs`) turns most of
that into a cache hit. A step costs roughly:

```
step ≈ cache-hit overhead (0.7–1.0 s)  +  new tail / prefill rate  +  output tokens / decode rate
```

**Inputs from the largest recent session** on this box (346 steps; the model isn't recorded
in its event log):

- **54K** average prompt per step;
- **227** output tokens per step, reasoning included;
- new tail per step typically 1–3K tokens: tool results.

**Estimate** for gpt-oss-20b on `p150x2-throughput`:

| Part | Seconds per step | Share of model time |
|---|---|---|
| Decode: 227 tokens at 69 tok/s | ~3.3 | ~65–70% |
| Cache-hit overhead | ~0.8–1.2 (extrapolated to 54K) | ~20% |
| New-tail prefill: 1–3K tokens at ~3–5K tok/s | ~0.3–0.7 | ~10% |
| **Model time** | **~4.6–5.2** | |
| After compaction: cold re-read at 64K | +13–22 once per compaction | |

The high prompt-to-output ratio doesn't make prefill dominate here, because the prefix cache
works (`readiness-review` row: the cache-safe launch fix is done). **Decode is about two-thirds
of model time.** So:

- **100 tok/s** would cut model time per step by an estimated **~20–25%**.
- **155 tok/s** (the tt-lab level) would cut it by **~35–40%**.

**Caveat: model time isn't the whole step.** That session's turns took 7,292 s of wall time
for 346 steps (~21 s a step), against an estimated ~1,700 s of model time (~23%). The rest is:

- tool runs (`cargo build` and tests);
- 30 approval waits;
- cold re-reads after compaction.

So a faster config makes Code feel faster mainly on tool-light, chatty steps. Measure this
(§5) before paying for it. Step-level speed data (`codespeed::Step`) is collected today but
**not persisted** (the session `step` events carry no numbers, and `code-speed.json` holds only
medians). Persisting it is the first change in §6.

## 3. Options

### (a) Keep the throughput config (shared boxes)

- **Config:**
  - **a0 (today):** `p150x2-throughput` on chips 0 and 1.
  - **a1:** `p150x4-throughput` on all four chips, the same package and flag set.
- **Latency and throughput:**
  - **a0 (measured):** 69 tok/s single user; TTFT 0.12 s on short prompts; cached step
    overhead 0.7–1.0 s; cold 64K read 13–22 s.
  - **a1 (estimated):** decode similar to or above a0. **2× KV pool** (~1.06M tokens), so
    about **16 concurrent 64K Code sessions** stay in the prefix cache before eviction,
    against about **8** on a0. Eviction costs a cold re-read (13–22 s at 64K), which is the
    worst failure for agents on a shared box.
  - Aggregate tokens/s under 8–16 users: not measured for gpt-oss; `POST /bench` gives an
    8-way batch number.
- **Chips:** a0 uses 2, leaving board 1 idle. a1 uses 4.
- **Impact on others:**
  - With up to 32 sequences, everyone shares decode bandwidth.
  - With `chunked_prefill_decode_steps = 0` and 32K chunks, one user's cold 64K read **may
    stall other users' streams** for ~6–7 s per chunk. That's an inference from the flags:
    measure it (§5, test S3). If it's true, the **latency** profile (8K chunks, decode steps
    interleaved) is the better *shared* setting even though it's called "latency".
- **Operational complexity:**
  - a0: none.
  - a1: one profile flag. The package already ships it, and it uses ring fabric
    (`FABRIC_1D_RING`).
  - a1 gives up the free board for other jobs: image models, tt-lab, bring-ups.
- **TT Station changes:**
  - Let the serve path pick a **profile**, not just a package. `community.rs` today passes
    `--profile` only when the package default doesn't fit the box (`profile_for_this_box`).
  - Show the profile in Models and in the Code picker.

### (b) A low-latency 4-chip config (developer boxes)

There are two different things under this name:

**b1: vLLM `p150x4` latency profile** from the same package.

- **Latency:** **Not measured.** Estimated **85–120 tok/s** single user (1.2–1.75×). The
  estimate rests on two points:
  - Batch-1 decode of a 20B MoE across 4 chips is limited by inter-chip communication and
    dispatch, not FLOPs.
  - tt-lab's own 1→4-chip scaling (16.5 → 5.8 ms/token, 2.8×) is a ceiling with a very
    different runtime, not a forecast.
- **Prefill:** should improve; also unmeasured.
- **Chips:** all 4.
- **Impact on others:** none on a personal box. On a shared box, the same as a1 minus
  throughput tuning.
- **Operational complexity:** low. Same image, same weights cache per mesh (`/cache/p150x4`,
  converted on first start), and a first load to measure.
- **TT Station changes:** profile selection (as in a), plus a default profile per box role
  (§6).

**b2: tt-lab bare metal.**

- **Latency:** **152–158 tok/s decode measured, prefill ~198 tok/s measured.**
- **Not usable for Code today:**
  - At ~198 tok/s prefill, a 15K-token session start takes ~75 s and every compaction re-read
    over 5 minutes.
  - No OpenAI or Anthropic server, no batching, no prefix cache, no tool parser.
  - Exclusive device access (`O_EXCL`; the agent pauses telemetry and refuses other starts).
- **Value:** the right reference for "what decode could be". Track it as a research path:
  tt-lab needs fast prefill, a server and a KV cache before it's an option.

### (c) A separate Code-dedicated model or config, with speculative decoding later

**Variants:**

- **c1: a Code-only serve on board 1** while board 0 serves chat or images (two chat models
  co-served, `coserve.rs`).
  - gpt-oss-20b's 2-chip profiles are validated only on chips 0 and 1, so c1 needs
    **topology validation on chips 2 and 3** from the package authors.
  - Today co-serving is also **disabled whenever gozer is present**
    (`main.rs`: the companion only exists without gozer). The fix is to have the companion
    acquire a gozer lease on board 1. gozer's lease grain is the board, which fits.
- **c2: a different model.**
  - Qwen3.8-27B (DFlash package) passed this box's check-up 11/11 on tools and 10/10 on code
    (`evals/1791808719-e32c9f.json`). It measured 41–44 tok/s decode on creative text, 64 on
    the bench prompt, and ~6.8K tok/s prefill.
  - But it has **no prefix cache** (DFlash can't use one), so every step re-reads the whole
    conversation: about **8–9 s TTFT per step at 54K**. That is worse for Code than gpt-oss
    today.
  - A Code model qualifies only when it has prefix caching **and** passes agent reliability
    at 64K.
- **c3: speculative decoding on gpt-oss-20b.** Prompt-lookup / n-gram needs no drafter and
  suits code edits, which copy from context. EAGLE-style heads need a trained head.
  - **Unknown:** whether the TT vLLM plugin supports either with prefix caching on. The DFlash
    precedent, a speculative method that turns the prefix cache off, is a warning.
  - **Expected:** an estimated 1.3–2× decode on edit-heavy steps, much less on reasoning. Not
    measured on TT.

**Summary of (c):**

- **Chips:** c1 uses 2 per model. c2 and c3 depend on the model.
- **Impact on others:** c1 protects chat users from Code load and the other way round, at the
  cost of half the KV each.
- **Operational complexity:** medium to high. Two serving lifecycles, routing by model name
  (`gateway.rs` already routes Code to the second chat model on a split box), and per-model
  reliability checks.
- **TT Station changes:** co-serve under gozer (lease board 1), a Code-model policy, and a
  reliability check-up gate before a model can be Code's.

### (d) Co-serving a small drafter (blocked)

- **Topology:** vLLM speculative decoding runs the draft model **in the same engine, on the
  same devices** as the target. A drafter in its own container on the other board isn't a
  vLLM configuration. It would need TT plugin work: a 4-chip mesh with the drafter placed on
  board 1 and draft tokens exchanged over the fabric, or a new remote-drafter protocol.
- **Blocked twice more:**
  - by gozer: no companion slot while gozer is present;
  - by the lack of a validated 2-chip placement on chips 2 and 3.
- **Expected gain:** an estimated ≤1.5–2× decode with a well-matched drafter. Unmeasured.
- **Chips:** 4.
- **Operational complexity:** high.
- **Recommendation:** don't pursue it as a TT Station project. If speculative decoding comes,
  it should come in-engine (c3).

### Comparison

| | a0 today | a1 x4-throughput | b1 x4 latency | b2 tt-lab | c1 board-split | c3 spec decode | d drafter co-serve |
|---|---|---|---|---|---|---|---|
| Decode, 1 user | **69 (M)** | ~70–110 (E) | 85–120 (E) | **152–158 (M)** | 69 per model (E) | 1.3–2× on edits (E) | ≤1.5–2× (E) |
| Prefill | **~4.9K/s (M)** | ≥ a0 (E) | ≥ a0 (E) | **~198/s (M)** | ≈ a0 | ≈ a0 | ≈ a0 |
| 64K sessions held in cache | ~8 (E from pool) | ~16 (E) | ~16 (E) | 1, no cache | ~8 per model | as base | as base |
| Chips | 2 (2 idle) | 4 | 4 | 4, exclusive | 2 + 2 | 2 or 4 | 4 |
| Ready to use | yes | flag | flag | no | needs validation + gozer change | plugin unknown | no |

M = measured on this box, E = estimate.

## 4. Impact on other workloads

Anything that takes all 4 chips (a1, b1) gives up the idle board that today could hold:

- FLUX or another image model in split mode;
- tt-lab runs;
- bring-ups;
- a second user's lease through gozer.

On a **personal** box the owner trades that consciously. Model switching already exists
(`POST /serve`, and the Code picker's `Switch` choice), and it records load times
(`load_times`). On a **shared** box the trade belongs to the owner, which is why switching must
become owner-only (§6).

## 5. Benchmark protocol (cancelled)

*Cancelled 2026-10-15: Code's picker offers every working model and profile, and the box
records speed and check-up results per model and profile instead (see the Decision above). The
protocol is kept for reference.*

**Configs:**

| Name | Profile |
|---|---|
| A | `p150x2-throughput` (baseline) |
| A′ | `p150x2` (latency) |
| B | `p150x4` (latency) |
| B′ | `p150x4-throughput` |

**Setup:**

- Each config gets an exclusive gozer lease on the whole box, a fresh server start, one
  warm-up pass, then **3 repetitions**.
- Report the median and IQR.
- Run on the same day and same build; record the image tag.

**Tests:**

| # | Test | Tool | Metric |
|---|---|---|---|
| S1 | Raw speed | `POST /bench` (`bench.rs`: single 256-token stream, 8×128 batch, power), and the check-up speed section in the Evals page (`evals.rs`) | TTFT, single tok/s, batch tok/s, Wh per million tokens |
| S2 | Prefill and cache curve | A small script, as used for this doc: cold, first-hit and second-hit TTFT at 5K, 16K, 32K, 64K, 96K | Cold tok/s; hit overhead by length. This also checks the **0.6–1 s hit overhead** across profiles |
| S3 | Interference | A chat stream at 1 tok/s polling, while another client cold-reads 64K | The chat stream's longest gap between tokens and its p95 TTFT. Decides throughput vs latency on shared boxes |
| S4 | Code trace replay (**new, ~2 eng-days**) | Extend `examples/agent_reliability.rs`: replay Code-shaped sessions (15K fixed prefix, +1–3K tail per step, ~230 output tokens, one compaction at 64K) at **1, 2, 4, 8, 16** concurrent sessions | Model seconds per step (p50/p95), cache hit rate, evictions, aggregate steps/min |
| S5 | Reliability unchanged | `agent_reliability --histories 10 --tasks 5 --lengths 65536,98304`, seeds 1 and 2 (pooled 100 trials per size) | Within-one-retry validity at 64K (bar ≥ 95%). Different chunking shouldn't change outputs, but has to be checked |
| S6 | Real tasks | 5 scripted Code tasks on a pinned tt-station commit, in the sandbox, auto-approve, ×3 | Wall-clock to done, model time share (needs persisted `Step`s), success |
| S7 | Load time | Serve-switch timings (`load_times`) | Cold start incl. first `p150x4` weight conversion, and warm start |

**Decision rules (proposed):**

- **Developer boxes:** adopt B or B′ if S6 wall time improves by **≥15%**, or S4 model time
  per step by **≥25%**, with S5 within noise (≥ 95% at 64K pooled).
- **Shared boxes:** pick the config with the best S4 throughput at 8–16 sessions, subject to:
  - S3 chat longest gap **≤ 2 s**;
  - p95 step time **≤ 2×** the single-user figure.

  Prefer the latency profile if throughput is within 10%.
- **S2 hit overhead:** if it stays ≥ 0.6 s on every config, file it with the package authors.
  It is about 20% of model time per step, and no serving config change fixes it.

## 6. What TT Station must change

| Change | Why | Effort |
|---|---|---|
| **Persist Code step speeds**: write each `codespeed::Step` into the session's `events.jsonl` `step` event; today those events are empty | S6 and real-world model-time share | S (1–2 d) |
| **Profile-aware serving.** *Done for community packages:* `POST /community/serve` takes a `profile`, the Code picker shows each profile, and `code-speed.json` keys speeds by model + package profile. Models doesn't offer a profile choice yet | Any profile can be Code's | M (1–2 wk) |
| **Code picker covers every source.** *Done:* `GET /code/models` lists the `tt` catalog, community packages (per profile) and native vLLM bundles | Pick any working model in Code | S–M |
| ~~**Box role default**~~: dropped; there's no default Code setup to pick | | |
| **Owner-only switching on shared boxes.** `POST /serve`, `/community/serve` and the picker's `Switch` take `BearerAuth` today, so any member can stop the model for everyone. Make them owner-only (with step-up under org identity, see [enterprise-identity.md](enterprise-identity.md) §5) when `box_role = shared` | Protects other users | S |
| **Co-serve under gozer.** The companion slot leases board 1 instead of being disabled | Option c1 | M |
| **Code trace replay benchmark** (S4) | Measurement | S–M (2 d) |
| Spec decoding (c3), drafter (d) | Depends on the plugin | External, L |

## 7. Recommendation per box type (withdrawn)

*Withdrawn 2026-10-15 with the benchmark: there's no default Code setup per box type. People
pick any working model and profile in Code; switching policy is the model-control setting.
Kept for reference.*

| | Personal developer box | Shared team box |
|---|---|---|
| Config now | Keep a0 until the benchmark (about a day of box time), then **B (`p150x4` latency) or B′, whichever wins S4/S6**. Board 1 is idle anyway | **`p150x4-throughput` (a1)**, or `p150x4` latency if S3 shows throughput stalls other users. Never a0's 2 idle chips on a box people queue for |
| Why | Nobody is displaced, the KV pool doubles, and decode is estimated to rise 1.2–1.75× | ~16 concurrent 64K sessions stay in cache instead of ~8. Cache eviction (13–22 s re-reads) is what makes agents feel broken under load |
| Switching | Owner and members alike (today's behaviour) | Owner-only, audited (already logged as `model_switch`) |
| Compaction | 64K (measured, unchanged) | 64K |
| Later | c3 speculative decoding when the plugin supports it with the prefix cache; tt-lab once it has fast prefill and a server | c1 board split only if S3 shows Code and chat interfere even on the latency profile |

## 8. Decisions needed

*Decided 2026-10-15: items 1 and 2 (no benchmark, no default profile; people pick in Code).
Item 3 is each person's choice when they pick a 4-chip profile. The others stay open.*

1. **Run the benchmark protocol (§5)** before changing defaults: about 1 day of exclusive box
   time plus ~2 eng-days for the S4 replay. Approve the box time.
2. **Default profile per box role.** Recommended B or B′ for personal boxes and a1 for shared
   boxes, subject to §5. Agree the decision thresholds in §5.
3. **Give up the idle board** on 4-chip configs: acceptable on personal boxes? Shared boxes
   then lose split-mode image serving unless an owner switches.
4. **Member model switching** on shared boxes: make it owner-only (recommended)?
5. **Investigate the cache-hit overhead** with the gpt-oss package authors (`tt-hous`): 0.7–1.0
   s per hit, about 20% of a Code step. Who owns it?
6. **Topology validation of chips 2 and 3** for 2-chip profiles, plus co-serve under gozer.
   Needed only if c1 is wanted. Defer?
7. **Speculative decoding:** ask the TT vLLM plugin team for prompt-lookup or EAGLE support
   that keeps prefix caching (c3). Drop the drafter co-serve (d) as a TT Station item?
8. **tt-lab:** treat it as a reference ceiling only, and don't schedule Code on it until it has
   prefill ≥ ~3K tok/s, a server, and a KV cache.
