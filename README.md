# TT Station

TT Station turns a Tenstorrent box (a QuietBox, a Galaxy server, or a workstation with
Tenstorrent cards, Blackhole or Wormhole) into a private AI service on your own network.
The box runs the models; you use them from the TT Station desktop app on a Mac or Linux
computer, from the TT Station iPhone app, or from any tool that speaks the OpenAI API.
Nothing leaves your network: the apps talk only to your box, over HTTPS pinned to the
box's own certificate.

## Install on the box

On the box (Ubuntu, x86-64), install the `tt-station` package that came with the release:

```bash
sudo apt install ./tt-station_0.12.0_amd64.deb
systemctl --user enable --now tt-station-agentd   # start TT Station for your account
sudo loginctl enable-linger "$USER"               # keep it running when nobody is logged in
```

It starts with no configuration: the box is named after its hostname and answers on port
8765 (HTTPS on 8766). Serving models also needs Docker, the Tenstorrent drivers and
tt-inference-server; [docs/reference/installing.md](docs/reference/installing.md) has the
full steps for a new box.

## Get the apps

- **Mac:** [desktop/mac/README.md](desktop/mac/README.md) (a zip for Apple Silicon or Intel).
- **Linux:** [desktop/linux/README.md](desktop/linux/README.md) (the `tt-station-desktop` package).
- **iPhone:** [ios/README.md](ios/README.md) (built with Xcode and your own Apple team).

## Connect and pair

The desktop app finds boxes on your network (or type the box's address).

- **Owners.** Sign in with your account on the box (the username and password you'd SSH
  in with). The app first checks the box's SSH host key over the box's pinned HTTPS, so it
  knows it's talking to your box. That computer becomes an owner.
- **Members.** Choose **Use a pairing code**: the box shows a code in TT Station on its
  screen and on any phone paired with it.
- **iPhone.** Scan the pairing QR in TT Station on the box (Settings → Devices and
  pairing) with the iPhone's Camera.
- **No screen on the box and no paired phone?** In a terminal on the box, run
  `tt-station-agentd pair`: it prints a pairing QR code and link.

Owners can change anyone's role in Settings → Devices and pairing.

## Use it from other tools

The box serves one OpenAI-compatible API at `https://<box>.local:8766/v1`, following
whatever model it's running. In the app, open **Connect** to make a key for each tool and
copy ready-made setup for Open WebUI, Continue, the OpenAI SDKs and curl. The certificate
is the box's own (self-signed), so a tool must trust or pin it; Connect shows its
fingerprint. Details: [docs/reference/gateway.md](docs/reference/gateway.md).

## Documentation

- [docs/reference/](docs/reference/): how each part works, starting with
  [installing.md](docs/reference/installing.md), [gateway.md](docs/reference/gateway.md)
  and [your-data.md](docs/reference/your-data.md).
- [docs/security-faq.md](docs/security-faq.md): what the box exposes, how access works,
  and how to lock it down ([box-hardening.md](docs/reference/box-hardening.md)).

## Build from source

| What | How |
|---|---|
| Box agent and CLI | `cargo build --release` (Rust toolchain pinned in `rust-toolchain.toml`); `cargo test --workspace` |
| Box package (`tt-station` .deb) | `./build-deb.sh` (needs `devscripts`, `debhelper` and Node 22; see [installing.md](docs/reference/installing.md)) |
| Desktop apps | `cd desktop && npm install`, then `desktop/mac/build-mac.sh` or `desktop/linux/build-deb.sh` |
| iPhone app | `cd ios && xcodegen`, then build in Xcode (see [ios/README.md](ios/README.md)) |

No hardware? `cargo run -p mock-box -- serve --ctrl-port 8899` runs a fake box for
development.

## License

Apache-2.0: see [LICENSE](LICENSE) and [NOTICE](NOTICE). Third-party software and
assets: [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md).
