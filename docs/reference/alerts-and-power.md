# Alerts (`/notify`, `/events`) and remote power (`/power/info`) (reference)

## Alerts

The agent watches the box every 5 s (`notify::watch_forever`) and turns changes into
events: `download_done` / `download_failed` (a `tt model pull` it started or adopted
finished), `model_ready` / `model_failed` (a start -- main slot or co-serve image slot --
finished), `box_online` (the agent started: useful after a power-on), `chips_hot` (hottest
chip ≥ 90 °C, re-armed below 80 °C), and `power` (pushed, and waited on up to 8 s, just
before a sleep/restart/shutdown). Only transitions out of `starting`/`running` count, so an
adopted serve after a restart isn't announced as news.

| Route (authed) | |
|---|---|
| `GET /events?since=N` | `{events: [{seq, at, kind, title, body}], latest}` -- newest 200 kept. `seq` starts at the clock ×1000, so a restarted agent's events sort after the last run's. |
| `GET /notify` | `{push: {available, problem, devices: [{name, added, token_hint}]}, latest_event}` |
| `POST /notify/devices {token, sandbox, name}` | A phone's APNs device token (hex); re-registering refreshes it. Saved in `$TT_CONFIG_DIR/notify.json` (0600). |
| `DELETE /notify/devices/{token}` | Stop pushing to that phone. |
| `POST /notify/test` | Push a test notification now: `{sent: n}`; `409` when the box can't push, `502` when Apple took none. |

**Delivery: ordinary iPhone push notifications.** The app asks permission at the moment
the user starts something slow (a download, a model load) or from Box → Notifications,
registers with iOS for remote notifications, and gives the box its device token. Each
event then goes from the box to Apple's push service (`crate::apns`): HTTP/2 to
`api.push.apple.com` (or `api.sandbox…` for Xcode builds), an ES256 provider token signed
with the app's APNs key, `apns-topic` = the bundle id, payload `{aps: {alert: {title,
subtitle: <box>, body}, sound, interruption-level}, kind}`. Tokens Apple reports dead
(`410`/`Unregistered`) are dropped; a token from the other environment (`BadDeviceToken`) is
retried there and remembered. Messages carry model names and events, never chat content.

**Setting up the key (once per box, for this prototype).** In the Apple Developer account
that signs the app: Certificates, IDs & Profiles → Keys → + → Apple Push Notifications
service; download `AuthKey_<KEYID>.p8`. Put it in `~/.config/tt-station/` with:

```toml
# ~/.config/tt-station/apns.toml
team_id = "ABCDE12345"        # Membership details → Team ID
key_id = "XYZ9876543"         # the key's id
key_file = "AuthKey_XYZ9876543.p8"
topic = "com.tenstorrent.ttstation.mobile"   # the app's bundle id
```

then `systemctl --user restart tt-station-agentd` (the log says why if push stays off). A
shipped product would keep the key on a Tenstorrent push relay instead of every box -- the
pattern Home Assistant, Plex and Synology use -- with the same app and the same device
tokens. The app's `aps-environment` entitlement needs a paid developer team.

**Until the box can push**, the app falls back to its own checks of `/events`: instantly
while open, and in background refreshes iOS grants now and then (`BGAppRefreshTask`).

## Remote power

`POST /power {action: suspend|reboot|shutdown}` (existing; see
[power-controls.md](power-controls.md)) now sends a `power` alert first.
`GET /power/info` reports what works and what to set up, once, on the box:

| Field | |
|---|---|
| `can_power_off` / `can_reboot` / `can_suspend` | logind's answer for the agent (`yes`, `challenge`, `no`). |
| `starts_at_boot` | `loginctl` Linger: without it the agent (a user service) only runs while someone is logged in, so a box that was turned on remotely is unreachable. |
| `wired: [{name, mac, connected, wake_enabled}]` | Physical, non-wireless NICs. Wake-on-LAN needs one with a cable and `power/wakeup` enabled; Wi-Fi can't wake a sleeping or powered-off computer. |
| `setup: [{why, command}]` | The missing steps: `sudo loginctl enable-linger <user>`; installing the polkit rule (the agent stages `deploy/tt-station-power.rules` at `$TT_CONFIG_DIR/tt-station-power.rules`); plugging in the cable; `nmcli … wake-on-lan magic && ethtool -s <nic> wol g`. |

**Turning it on.** The app sends a Wake-on-LAN magic packet to the remembered wired MAC
(broadcast, subnet broadcast, last address). iOS only lets apps with Apple's multicast
entitlement send broadcasts, so this may not leave the phone; the dependable alternative
is a Home-app smart plug plus the BIOS's "restore on AC power loss: on", then a Shortcut
that toggles the plug. Siri intents: Power My Box (sleep/restart/shut down), Wake My Box.
