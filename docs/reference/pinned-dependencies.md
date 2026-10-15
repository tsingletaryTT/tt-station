# Pinned third-party downloads

What TT Station downloads and runs on the box is pinned, so a moved tag or a
changed script on someone else's server can't change what runs. This page lists
the pins and how to bump them.

## Container images (Connect's apps, voice)

Every image in `crates/tt-station-agentd/src/apps.rs` (`APPS`) and
`crates/tt-station-agentd/src/voice.rs` (`SPEAK`, `LISTEN`) is written
`name:tag@sha256:<digest>`. Docker pulls and runs the digest and ignores the
tag; the tag is there so a person can see what it is. A test
(`apps::tests::every_image_is_pinned_by_digest`) fails if one isn't pinned.

Pinned 2026-10-15. Where the image was already pulled on the reference QuietBox
the digest is that pulled image's (`docker image inspect --format
'{{index .RepoDigests 0}}'`); otherwise it's the registry's current digest for
the tag (`docker buildx imagetools inspect <image:tag>`). Each digest is the
multi-platform index, so it covers linux/amd64.

| Image | Digest | From |
|---|---|---|
| `lobehub/lobe-chat:latest` | `sha256:b2d2454525523d9f0a19c79661f83ec45f13363dbadd5c1180887e77af35d872` | registry |
| `quay.io/jupyter/minimal-notebook:latest` | `sha256:877180014994396eb0857d3bad2dfedfb704fffccf54be4d1b7b50a43093f188` | registry |
| `docker.n8n.io/n8nio/n8n:latest` | `sha256:dd580fe1ab6e99e85bc66f2acf8bc8c5e9409fbe61b2e2803c5024403ef52833` | registry |
| `flowiseai/flowise:latest` | `sha256:cf880b17b1290fbaebebc76c33954bde67b3775aff95feec72a1d060cc81e43d` | registry |
| `searxng/searxng:latest` | `sha256:5903efa50bc6b99293fa059f7a0ff37eddd910210196af77d02e5536ccdfb387` | pulled on the box |
| `localdeepresearch/local-deep-research:latest` | `sha256:ce95481a35d3eebc2c1ae7bd34fad426108c3fb8d1a22f66c9e772276869c2f0` | pulled on the box |
| `itzcrazykns1337/vane:latest` | `sha256:0b61bf0d4470e9f2a9c71f794b72fab6e5a612177172770684c158a461f6896e` | registry |
| `yanwk/comfyui-boot:cpu` | `sha256:c54d7d0d744672654b0ec6b3bc02c569fb4f5eaef86ff2dac099a6e9dc231f97` | registry |
| `langflowai/langflow:latest` | `sha256:a0396e089eb97fa471ef8affc492b8c56abb3758040a5ddaa80814b0abad15bf` | registry |
| `unclecode/crawl4ai:0.9.4` | `sha256:9021b3cb5c6f12570bbcd5395638495e0a06969b3148e377b953d174af2ebc9b` | registry |
| `pyd4vinci/scrapling:0.4.15` | `sha256:1bacbc8ec90b3090d462e12f6555e241daf0dfeb684ab326ffa09d52d8226e69` | registry |
| `ghcr.io/openhands/agent-canvas:latest` | `sha256:91b7a217dcbefe8c10562523b043458ed1dfaf8ba770194b319fe754cee03f23` | registry |
| `openclaw/openclaw:2026.9.3` | `sha256:6cb72e1599b3b76e2ea3dcc2f4dd7f112247367fbecadfcdee8caa7423f4989b` | pulled on the box |
| `ghcr.io/hkuds/lightrag:v1.5.7` | `sha256:5bdbd524931b011df246fe20888d110cef691e6804c12cde636a2b746d7de27e` | registry |
| `ghcr.io/remsky/kokoro-fastapi-cpu:latest` (voice) | `sha256:ee3111d6a2c903ed62f3b4fa19543c6901205ed39fce3c177e895b34a8386b9c` | pulled on the box |
| `ghcr.io/speaches-ai/speaches:latest-cpu` (voice) | `sha256:21e3df06d842fb7802ab470dd77c25f0e8c0d22950e8d8c6ae886e851af53ef8` | pulled on the box |

### Bumping an image

1. Read the image's release notes, then get the digest the tag points at now:

   ```bash
   docker buildx imagetools inspect searxng/searxng:latest   # "Digest: sha256:…"
   ```

   (Or pull it and read `docker image inspect --format '{{index .RepoDigests 0}}'`.)
   Use the index digest (the one `imagetools` prints at the top), not one
   platform's manifest, so arm64 Macs and amd64 boxes both resolve.
2. Replace the digest after `@` in `apps.rs`/`voice.rs`, keep or update the tag,
   update the table and date above, and install the app on a box to check it.

Already-installed apps keep their container until it's re-created: a bump
reaches a box when the person reinstalls or updates the app.

## Jupyter's OpenAI client

Jupyter's start command installs the OpenAI Python client into the notebook
container: `pip install -q openai==3.26.1` (released 2026-10-08). Bump the
version in `apps.rs` the same way: read the changelog, change the number. It's
a version pin, not a hash pin: PyPI still serves the file.

## Hermes Agent's installer

`crates/tt-station-agentd/src/hermes.rs` runs Nous Research's installer from a
fixed commit of `NousResearch/hermes-agent` (`INSTALLER_COMMIT`), checked
against `INSTALLER_SHA256` before it runs, and passes the same commit to the
installer (`--commit`) so the checkout matches. Pinned 2026-10-15 to
`046094df28b712f62a5e3c728a9e59da05dcbf1e` (byte-identical to what
hermes-agent.nousresearch.com/install.sh served that day).

To bump:

```bash
C=$(git ls-remote https://github.com/NousResearch/hermes-agent.git refs/heads/main | cut -f1)
curl -fsSL -o install.sh https://raw.githubusercontent.com/NousResearch/hermes-agent/$C/scripts/install.sh
less install.sh          # read it: it runs as the box's account
sha256sum install.sh
```

Put `$C` and the sha256 in `hermes.rs`. The installer then fetches its own
tools (uv, Python, Node packages) from their usual places; those aren't pinned
by TT Station.
