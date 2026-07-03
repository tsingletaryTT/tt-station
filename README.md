# tt-station

Plug-and-play Tenstorrent from your Mac: discover a QuietBox 2 like an AirPlay
device, run/stop models from a menu bar, hit one OpenAI-compatible `/v1`
endpoint, and burst to Tenstorrent Cloud when local chips are full.

This repo holds a design exploration and its deliverables — not shipping code yet.

## Start here

- **`CONTEXT.md`** — the full picture: concept, fact-checked facts, maturity split,
  and open next steps. Read this first (and hand it to a fresh Claude session).

## Deliverables

- `quietbox-happy-path.md` — happy-path tutorial (real-vs-shim per step)
- `quietbox-from-the-future.md` — the blog post (quick + how-it-works + counter-point)
- `QuietBox2-Datasheet.pdf` — 2-page datasheet with macOS menu-bar mockup
- `datasheet.html` — datasheet source (render: `weasyprint datasheet.html out.pdf`)
- `desk-is-a-datacenter.pptx` — one-slide infographic
- `build_slide.js` — slide generator (`node build_slide.js`, needs pptxgenjs + react-icons + sharp)

## Resume on another machine (CLI)

The chat thread itself doesn't transfer between machines, but these files do.
On the other machine:

```bash
cd ~/code/tt-station
claude
# then: "Read CONTEXT.md — this is where we left off. Let's continue with <next step>."
```

## Status

Concept / research preview. Hardware specs are real (Tenstorrent Blackhole);
the discovery/pairing daemon and menu-bar app are aspirational and unbuilt.
See `CONTEXT.md` → "Open threads / next steps."
