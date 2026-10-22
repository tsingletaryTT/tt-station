# Your box as an OpenAI API: `/v1`, app keys, `/connect`, `/bench`, `/creations` (reference)

## Why

A box is most useful when *everything* can use it -- not just the phone app.
The agent exposes one stable, OpenAI-compatible endpoint. From other computers
it's HTTPS on the TLS port, with the box's own self-signed certificate (`GET
/connect` lists the addresses and the certificate's SHA-256 fingerprint to pin or
trust); plain HTTP on the control port answers on the box itself, and on the LAN
only if an owner allows it in Settings:

```
https://<box>.local:8766/v1     # from other computers
http://127.0.0.1:8765/v1        # on the box
```

It follows whatever the box is doing: chat and completions go to the served chat
model, images to the image model. Clients never
chase ports or model ids: the `model` a client sends is rewritten to the one
serving, so tools that insist on `gpt-4o` just work.

## Routes

| Route | Auth | |
|---|---|---|
| `GET /v1/models` | app key | The served models, OpenAI-shaped, plus our `kind` (`chat`, `image`). |
| `POST /v1/chat/completions`, `/v1/completions`, `/v1/embeddings` | app key | Forwarded to the chat model; streaming (SSE) passes straight through. `503` when no chat model serves. |
| `POST /v1/images/generations` | app key | OpenAI's image API on tt-media-server: `n` → `number_of_images` (1-4), steps default to 4 for FLUX schnell else 20, `guidance_scale` defaults to 3.5; answers `{created, data: [{b64_json}]}`. `response_format: "url"` is refused (`400`). |
| `GET /keys`, `POST /keys {name}`, `DELETE /keys/{id}` | paired | Mint (secret returned once), list (hint, requests, last used), revoke. At most 32. |
| `GET /connect` | paired | `{urls: [{label, url}], models, keys, fingerprint, lan_http}` -- the Bonjour name and LAN IP over HTTPS, plain HTTP on the LAN when allowed, and `127.0.0.1`. |
| `POST /bench` | paired | Benchmark what serves (below). `409` while one runs or nothing serves. |
| `GET /creations/{id}` | paired | A JPEG the assistant drew in chat (`create_image`). |

**App keys** (`ttq-…`) are stored in `$TT_CONFIG_DIR/app-keys.json` (mode 0600),
compared in constant time, and count their use (saved at most once a minute).
A paired device's own token also works on `/v1`. Errors are OpenAI-shaped
(`{"error": {"message", "type", "code"}}`) so clients show them.

Media servers (tt-media-inference-server 0.10) check `Bearer $API_KEY` even with
`NO_AUTH`; the agent sends `$TT_MEDIA_API_KEY` or the server default.

## Benchmark (`POST /bench`)

Against what serves, sequentially: one streamed answer (256 tokens, temperature
0: time to first token, decode tokens/s), a batch of 8 parallel requests (128
tokens each: aggregate tokens/s), one image (4-step FLUX schnell) when an image
model serves, and the chips' mean power from the metrics recorder. The app turns
`wh_per_million_tokens` into a cost at the user's electricity price.

## Images in chat (`create_image`)

A read-only assistant tool: renders with the image model that serves,
saves the JPEGs to `$TT_CONFIG_DIR/creations` (newest 300 kept), and hands the
model only their ids; the action report carries `images: [id]` for the client to
fetch. Without an image model it tells the model how to get one (the `images` mode).

## Media-server stall watchdog

tt-media-inference-server 0.10 can deadlock at startup. Most likely cause: it queues a warm-up PIL
image (whose pickling, on the queue's feeder thread, imports a PIL plugin under
the import lock) milliseconds before forking the device worker, which inherits
the held lock and hangs on its own PIL import -- "Model is not ready" forever.
During a start, once a media-server container has
run 6 minutes, the agent checks `docker top` once a minute: if every worker of
the `uvicorn … main:app` server has used under 20 s of CPU and stayed under
2 GB (a deadlocked FLUX worker: ~1 s, 460 MB; a loading one: tens of seconds
and 20+ GB), it `docker restart`s the container, at most 5 times per start (it hit 4 of 6 starts on one box, 3 in a row).
