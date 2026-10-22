# Alerts (`/notify`, `/events`) and remote power (`/power/info`) (reference)

## Alerts

The agent watches the box every 5 s (`notify::watch_forever`) and turns changes into
events: `download_done` / `download_failed` (a `tt model pull` it started or adopted
finished), `model_ready` / `model_failed` (a model start finished), `box_online` (the agent started: useful after a power-on), `chips_hot` (hottest
chip ≥ 90 °C, re-armed below 80 °C), and `power` (announced, and waited on up to 8 s, just
before a sleep/restart/shutdown). Only transitions out of `starting`/`running` count, so an
adopted serve after a restart isn't announced as news.

| Route (authed) | |
|---|---|
| `GET /events?since=N` | `{events: [{seq, at, kind, title, body}], latest}` -- newest 200 kept. `seq` starts at the clock ×1000, so a restarted agent's events sort after the last run's. |
| `GET /notify` | `{latest_event}`: the newest event's `seq`. |

**Delivery: the apps read `/events`.** The desktop app shows new events in its bell and as
desktop notifications. The iPhone app checks `/events` instantly while open, and in the
background refreshes iOS grants now and then (`BGAppRefreshTask`), and shows new events as
local notifications (with Allow / Don't allow for an approval). Nothing goes through a
third-party push service, so a phone away from the box hears about events when it next
reaches it.

## Remote power

`POST /power {action: suspend|reboot|shutdown}` (existing; see
[power-controls.md](power-controls.md)) now sends a `power` alert first.
`GET /power/info` reports what works and what to set up, once, on the box:

| Field | |
|---|---|
| `can_power_off` / `can_reboot` / `can_suspend` | logind's answer for the agent (`yes`, `challenge`, `no`). |
| `starts_at_boot` | `loginctl` Linger: without it the agent (a user service) only runs while someone is logged in, so a box that was turned on remotely is unreachable. |
| `wired: [{name, mac, connected, wake_enabled, wake_supported}]` | Physical, non-wireless NICs. Wake-on-LAN needs one with a cable and `power/wakeup` enabled; Wi-Fi can't wake a sleeping or powered-off computer. |
| `wake_on_lan` | Whether a phone can wake the box, the wired MAC and the box's broadcast addresses, what's missing in plain words, and the one-time command: see [sleep-and-wake.md](sleep-and-wake.md). |
| `setup: [{why, command}]` | The missing steps: `sudo loginctl enable-linger <user>`; installing the polkit rule (the agent stages `deploy/tt-station-power.rules` at `$TT_CONFIG_DIR/tt-station-power.rules`); `sudo nmcli connection modify <connection> 802-3-ethernet.wake-on-lan magic` (or `ethtool -s <nic> wol g` without NetworkManager); plugging in the cable. |

`POST /power/wake-on-lan` (owners) turns Wake-on-LAN on itself where polkit lets the agent's
account, else answers with that command ([sleep-and-wake.md](sleep-and-wake.md)).

**Turning it on.** The app sends a Wake-on-LAN magic packet to the remembered wired MAC
(the phone's and the box's broadcasts, `255.255.255.255`, last address), and only when
`wake_on_lan` said it could work and the phone is on the box's network; otherwise it says
why and points at model sleep (`POST /idle/sleep`), which keeps the box reachable. iOS only
lets apps with Apple's multicast entitlement send broadcasts, so this may not leave the
phone; the dependable alternative is a Home-app smart plug plus the BIOS's "restore on AC
power loss: on", then a Shortcut that toggles the plug. Siri intents: Power My Box
(sleep/restart/shut down), Wake My Box.
