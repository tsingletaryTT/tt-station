# People, schedules and energy (reference)

Box features that need no account anywhere, plus serving an Orchard result. (Remote access
over WireGuard was documented here but never implemented; see below.)

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

## Remote access: not implemented

An earlier version of this page described a WireGuard tunnel set up by the agent (a
`remote/wireguard` route family and a `wireguard.rs` module). **That feature does not exist in
the code**: there is no such route in `crates/tt-station-agentd/src/routes.rs` and no WireGuard
module. Off-LAN access today means bringing your own VPN: when the box has a Tailscale address,
QR pairing carries it so the phone can reach the box away from home (`pairqr.rs`), and pairing and
TLS apply as on the LAN.

## Serving an Orchard result (`POST /orchard/runs/{name}/serve`, `POST /orchard/packaging`)

With packaging on (`package_format = "v6"`, added to tt-orchard's `bringup.toml` by
`/orchard/packaging`, marked `# set by tt-station`), stage 7 leaves a boot-checked copy at
`stages/7/verify/`. Serve stops what's serving (with consent), resets the chips, runs
`bash run.sh --port <serving port>` as the `tt-orchard-serve` user unit with `HF_HOME` pointing at
the copy's linked cache, and adopts it as the serving endpoint once `/v1/models` answers. Stopping
the model (any way) stops that unit.
