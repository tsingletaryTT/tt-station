# tt-station — session context

*A carry-over doc so a fresh Claude Code (CLI) session on another machine can pick up with full context. Written 2026-07-02.*

## What this is

A blue-sky-but-grounded design exploration: **turn a Tenstorrent QuietBox 2 on your LAN into a plug-and-play appliance you drive from a Mac** — discover it like an AirPlay device, run/stop models from a menu bar, hit one OpenAI-compatible `/v1` endpoint, and transparently burst to Tenstorrent Cloud when local chips are full. The working name for the missing glue is **`tt-station`**.

Core thesis: **~80% already exists** (dstack + tt-inference-server + console.tenstorrent.com + macOS Bonjour). The unbuilt part is a small **discovery/pairing daemon + CLI/menu-bar veneer**, plus a local-first cloud-burst policy.

## Deliverables in this folder

| File | What it is |
|------|-----------|
| `quietbox-happy-path.md` | The original happy-path tutorial: discover → pair → `tt run` → routed `/v1` → self-healing, each step flagged real-vs-shim. |
| `quietbox-from-the-future.md` | Expanded "blog post from a possible future" — quick version + detailed "how it works" + cloud-sync section + fact-checked **counter-point** section. |
| `desk-is-a-datacenter.pptx` | One-slide infographic of the concept (Mac → QuietBox → Cloud behind one `/v1`). |
| `QuietBox2-Datasheet.pdf` | Two-page datasheet, "QuietBox 2 — as your Mac sees it," with a macOS menu-bar mockup hero + real Blackhole specs. |
| `datasheet.html` | Source for the datasheet PDF (rendered via WeasyPrint). |
| `build_slide.js` | pptxgenjs generator for the infographic slide. |

## Verified facts (fact-checked via internal Glean + public docs)

- **dstack** supports Tenstorrent; **0.20.22** added Blackhole (PCIe / LoudBox / QuietBox / Galaxy). TT path is **SSH fleets**. Serving is **tt-inference-server** wrapping **vLLM**, exposing OpenAI-compatible `/v1`. QuietBox 2 GPU spec in dstack: `p300:32GB:4` (per-chip) or `p300:64GB:2` (per-card).
- **console.tenstorrent.com** is real: OpenAI-compatible endpoint at `https://console.tenstorrent.com/v1`, API keys minted at `/dashboard/inference/keys`, Bearer auth, `/models` route.
- **tt-operator** exists and is public (**v0.1.0**) but is **DRA-based** (Kubernetes Dynamic Resource Allocation — `ResourceSlices`, `deviceClass tenstorrent.com`), *not* a classic kubelet device-plugin. Allocation is **beta in v0.1; hardening staged for v0.2**. (This was a correction — an earlier draft wrongly called it a device-plugin.)
- **Blackhole per-chip specs** (official docs, p150a-class Tensix processor): 120 Tensix cores, 16× SiFive x280 "big RISC-V" cores, 180 MB SRAM, 32 GB GDDR6 (256-bit), 512 GB/s, 664 TFLOPS BLOCKFP8, up to 1.35 GHz, 300 W TBP, PCIe 5.0 ×16, 4× QSFP-DD 800G. **p300** is the dual-chip card; **QuietBox 2 = 2× p300 = 4 Blackhole chips** → ~480 Tensix cores, 128 GB GDDR6, ~2.6 PFLOPS BLOCKFP8 aggregate.
- **macOS discovery** is a freebie: Bonjour resolves `.local` natively; Avahi ships on the box's Ubuntu 22.04. Native client APIs: `NWBrowser` (Network.framework), SwiftUI `MenuBarExtra`; Keychain for tokens; Tailscale MagicDNS for beyond-LAN.

## Maturity split (honest)

- **Ships today:** dstack Blackhole support; tt-inference-server (vLLM) `/v1`; console.tenstorrent.com `/v1` + keys; macOS Bonjour/`.local`.
- **The thin shim (unbuilt):** `tt-station` discovery + 6-digit pairing daemon; menu-bar app & `tt` CLI; `OPENAI_BASE_URL` auto-export; local-first burst policy with budget guardrails.
- **Reality check:** tt-operator DRA is v0.1 beta; mDNS is often blocked on corporate LANs; single-owner daemon is a new SPOF/authz surface; cross-box Ethernet fabric for one model and device remoting ("TT/IP") remain moonshots.

## Open threads / next steps discussed (not yet done)

1. Spec the `tt-station` discovery/pairing: the `_tenstorrent._tcp` Bonjour TXT record format + the 6-digit pairing → Keychain-token handshake.
2. Design the `tt login` OAuth **device-authorization** flow that turns a console.tenstorrent.com sign-in into a registered dstack backend; define the local-first `--anywhere` placement policy + `tt budget` guardrails.
3. Scrappy PoC: a `tt` CLI that discovers a mock box (mDNS) and calls dstack's HTTP API.
4. "TT/IP" moonshot: what a remote-UMD shim would have to intercept at the luwen/UMD boundary to make a Mac believe it has local chips.
5. Cross-box fabric: extend the intra-box QSFP-DD Ethernet mesh across the LAN so one model spans multiple boxes.
6. Doc polish: light-print datasheet variant; fold the menu-bar mockup into the blog post as its hero image.

## Sources

- dstack Tenstorrent docs: https://dstack.ai/docs/examples/accelerators/tenstorrent/ · repo: https://github.com/dstackai/dstack
- tt-inference-server: https://github.com/tenstorrent/tt-inference-server
- tt-operator (v0.1.0, DRA): https://github.com/tenstorrent/tt-operator
- Blackhole PCIe specifications: https://docs.tenstorrent.com/aibs/blackhole/specifications.html
- Tenstorrent Cloud Console: https://console.tenstorrent.com/
- Internal Glean confirmed dstack 0.20.22 Blackhole support, console `/v1`, and tt-operator v0.1/v0.2 status.
