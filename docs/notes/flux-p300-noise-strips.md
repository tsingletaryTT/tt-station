# Draft upstream issue: FLUX on a single P300 (1x2 mesh) renders fixed-position noise strips

**Repo:** tenstorrent/tt-inference-server (`tt-media-server`) / tt-metal (`models/tt_dit`)
**Image:** `ghcr.io/tenstorrent/tt-media-inference-server:0.10.0-555f240` (tt-inference-server v0.18.0)
**Model:** FLUX.1-schnell, 4 steps, guidance 3.5, 1024×1024
**Hardware:** QuietBox 2 (two P300c cards)

## Symptom

On `--device p300` (mesh 1×2; the pipeline picks `sp=(1,0)`, `tp=(2,1)`,
`encoder_tp=(2,1)`, `vae_tp=(2,1)`, `num_links=2`), every image is correct except for
horizontal strips of garbled noise, about 8×192 px, at the **same pixel positions in every
image** regardless of prompt or seed (rows near y≈200, 295, 405, 500, 600, 710, 805, 915, 1010;
x≈64–320 or 575–830). On flat content the model also paints garbled "text" (e.g. "NO ARTIISC")
on the same grid. 8×192 px is one latent row by 24 latent columns, so the damage is at fixed
latent/token positions, and the text-like glyphs point at the DiT (attention/position path)
rather than only the VAE.

## Isolation

| Run | Chips | Result |
|---|---|---|
| `--device p300 --device-id 2,3` | board …125 | strips |
| `--device p300 --device-id 0,1` | board …128 | **same strips, same positions** |
| `--device p300x2` (whole box) | all four | **clean** (same prompts and seeds) |

So it isn't a faulty chip or board; it's the 1×2 FLUX configuration. Repro: serve FLUX.1-schnell
on `p300`, then `POST /v1/images/generations` with `{"prompt": "a plain light grey wall, evenly
lit", "seed": 11, "num_inference_steps": 4, "guidance_scale": 3.5}`.

## In tt-station

`GET /coserve` marks FLUX models with a known-issue note for half-box use, and the app shows it
under the image-model picker (`coserve::half_box_note`). Whole-box FLUX is unaffected.
