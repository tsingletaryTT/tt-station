# People, schedules, energy and remote access (reference)

Four box features that need no account anywhere.

## People (`/people`, `src/people.rs`)

A person is a name on the box (up to 12). Each paired phone says who it is; phones are told apart
by their pairing token, stored only as a SHA-256 digest (`people.json`).

| Route | |
|---|---|
| `GET /people` | `{people: [{id, name, added}], me}` (`me`: this phone's person, or `null`) |
| `POST /people {name, me}` | add (same name, any case, is the same person); `me: true` also makes this phone theirs |
| `PUT /people/me {person}` | this phone is that person's, or `null`: no one in particular |
| `PATCH /people/{id} {name}`, `DELETE /people/{id}` | rename; remove (their memories are forgotten) |

Memories (`memory.rs`) carry an optional `person`. A phone sees its person's memories plus the
household's (`person: null`); a phone that is no one in particular sees only the household's.
`/memories` and the assistant's `remember`/`forget` are scoped that way. The assistant learns who
it's talking with through a task-local (`people::CALLER`, set by `/assistant` and
`/assistant/stream` from the `Who` extractor), and its prompt says "You are talking with Ana."

## Schedules (`/schedules`, `src/schedules.rs`)

`{id, mode, time: "HH:MM", days: [0..6], enabled, last_run}` in `schedules.json` (0 is Sunday; mode
ids from `modes.rs`). Checked every 30 s; due from its minute until 10 minutes after, once a day.
It skips (and notifies) while a Lab run, an Orchard bring-up or a transcription holds the box.
`GET`, `POST {mode, time, days}`, `PATCH /schedules/{id} {enabled?, mode?, time?, days?}`, `DELETE`.

## Energy (`GET /energy`, `history.rs`)

The history now keeps per-day totals (energy from the chips' power readings, tokens, requests;
about two years). `/energy` returns `{months: [{month, energy_kwh, tokens, requests, days}],
days: [last 31]}`. The phone prices it at its own $/kWh setting.

## Remote access over WireGuard (`/remote/wireguard`, `src/wireguard.rs`)

The agent makes x25519 keys (`x25519-dalek`; no `wg` binary needed) and writes, under
`<config>/wireguard/`, `tunnel.json`, `wg-tt.conf` (0600) and `install.sh`. The tunnel is
`10.77.0.0/24`, the box `10.77.0.1`, with 8 phone slots made up front so adding a phone never needs
root again. Phones route only `10.77.0.1/32` through the tunnel.

Owner steps, once: `sudo sh <config>/wireguard/install.sh` (installs `wireguard-tools` if missing,
`/etc/wireguard/wg-tt.conf`, `wg-quick@wg-tt`, a ufw rule if ufw is active) and forward UDP 51820
on the router to the box. `GET` status (`configured`, `running` = `/sys/class/net/wg-tt`, slots,
`install_command`), `POST /setup {endpoint, port, reset}`, `POST /peers {name}` → the phone's
config. The app shares it to the WireGuard app and saves `10.77.0.1` as the box's remote address;
QR pairing offers it too when the tunnel is up, and the TLS certificate names it (new certificates
only; pinning is by fingerprint, so existing ones keep working).

## Serving an Orchard result (`POST /orchard/runs/{name}/serve`, `POST /orchard/packaging`)

With packaging on (`package_format = "v6"`, added to tt-orchard's `bringup.toml` by
`/orchard/packaging`, marked `# set by tt-station`), stage 7 leaves a boot-checked copy at
`stages/7/verify/`. Serve stops what's serving (with consent), resets the chips, runs
`bash run.sh --port <serving port>` as the `tt-orchard-serve` user unit with `HF_HOME` pointing at
the copy's linked cache, and adopts it as the serving endpoint once `/v1/models` answers. Stopping
the model (any way) stops that unit.
