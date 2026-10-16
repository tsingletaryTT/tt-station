# Sleep and wake

Two kinds of sleep, for different jobs:

| | Model sleep | Box sleep (suspend) |
|---|---|---|
| What stops | The served model; the chips go idle | The whole machine |
| Box reachable | Yes: the agent answers, over Wi-Fi or Ethernet | No |
| Waking | `POST /idle/wake`, or the next Chat, Tori, `/v1` or Code request (`wake_on_request`) | Wake-on-LAN magic packet, or the power button |
| Who | Whoever may start and stop models (the owners' model-control rule) | Owners |
| Works over Wi-Fi | Yes | Waking it doesn't: a Wi-Fi card is off while the machine sleeps |

On a box that's only on Wi-Fi, model sleep is the one to use: it saves most of the power
(the four Blackhole chips serving draw a couple of hundred watts doing nothing) and the box
stays reachable. The phone offers it in place of suspend whenever it can't wake the box.

## Model sleep

- `GET /idle`: `{settings: {enabled, after_minutes, wake_on_request}, asleep: {target,
  community, label, at} | null, idle_for_s, waking}`.
- `PUT /idle`: idle sleep, off by default. With `enabled`, the agent stops the model after
  `after_minutes` (5 to 1440) with nothing running or generated on it (read from vLLM's
  `/metrics`, so every client counts) and no busy Code session or routine.
- `POST /idle/sleep`: put the model to sleep now, the same way (it's remembered as asleep, so
  it wakes the same ways). `409` when nothing is serving. Logged as `model_sleep`.
- `POST /idle/wake`: start the model that was put to sleep. `409` when nothing is asleep.

All three writes follow the model-control rule (`403 "Only owners can start or stop models on
this box."` for members when owners have said so).

## Box sleep and Wake-on-LAN

`POST /power {action: suspend|reboot|shutdown}` (owners) is described in
[power-controls.md](power-controls.md), and the one-time polkit and linger setup in
[alerts-and-power.md](alerts-and-power.md).

### What has to be true

Wake-on-LAN wakes a sleeping (and, where the firmware allows, powered-off) computer when its
**wired** network port receives a "magic packet": six `FF` bytes, then the port's MAC sixteen
times, usually as a UDP broadcast to port 9. For that:

1. **A cable**: the port is plugged into the same network as the phone. Wi-Fi can't wake a
   computer.
2. **Wake-on-LAN on, for good**: the port is set to wake on a magic packet, and the setting
   survives reboots. On a QuietBox with NetworkManager, that's the connection's
   `802-3-ethernet.wake-on-lan magic`, which NetworkManager applies whenever the connection
   comes up (so it can be set before the cable is plugged in).
3. **The firmware lets it**: some boards also have a BIOS setting ("Wake on LAN", "Power on
   by PCI-E"); waking from powered off depends on it. Waking from suspend is the dependable case.
4. **The phone on the same network**: the packet is a local broadcast; it doesn't cross
   routers or come in from the internet (a VPN such as Tailscale can't carry it either).

### What the agent reports

`GET /power/info` → `wake_on_lan`:

| Field | |
|---|---|
| `possible` | All of the above that the box can see: a wired port that can wake the machine, set to wake, with a cable in. |
| `nic`, `mac` | The wired port and its MAC: what the packet carries. Not `/status`'s `mac`, which follows the default route and is the Wi-Fi card on a QuietBox. |
| `supported` | The port can wake the machine at all (it has a `power/wakeup` switch in sysfs). |
| `enabled`, `persistent` | Set to wake now (the kernel's switch) or at its next connection; `persistent` when NetworkManager's connection says `magic`. |
| `connected` | Cable in, link up. |
| `connection` | The NetworkManager connection the setting belongs on. |
| `broadcast` | The box's LAN broadcast addresses (Docker bridges and VPNs left out). |
| `why_not` | What's missing, in plain words; `null` when `possible`. |
| `enable_command` | The one-time command that turns it on, while it's off. |

All of it is read without root: sysfs, `nmcli -t`/`nmcli -g`, `ip -j -4 addr`.

### Turning it on

`POST /power/wake-on-lan` (owners) runs, as the agent's own account,

```
nmcli --wait 15 connection modify <connection> 802-3-ethernet.wake-on-lan magic
```

(and `nmcli device reapply <nic>` when the link is up). That works only where polkit already
lets the agent's account change system network settings: the agent never uses sudo and no
password prompt can reach a phone. Otherwise the answer is `{applied: false, message,
command}` with the exact command for the owner to run once on the box, e.g.

```
sudo nmcli connection modify netplan-en 802-3-ethernet.wake-on-lan magic
```

Without NetworkManager the command is `sudo ethtool -s <nic> wol g`, which lasts only until
the next restart. `409` when the box has no wired port that can wake it. Logged as
`wake_on_lan` (`ok` or `needs_setup`).

### This QuietBox, as found (2026-10-15)

`enp10s0` (wired, `power/wakeup` present but `disabled`, unplugged) and `wlp9s0` (Wi-Fi,
carrying the default route). NetworkManager's `netplan-en` connection would carry the
setting; it's unset. So: `possible: false`, "Plug a network cable into enp10s0 and turn on
Wake-on-LAN". Nothing was changed on the box while building this.

## The phone

- Power shows **The model** (sleep now, wake, the idle rule) above **The whole box** (sleep,
  restart, shut down for owners), each confirm saying what it means for everyone on the box.
- At pairing and on each start, the phone stores the wired MAC, the broadcast addresses and
  whether waking was possible (`WakeTarget`), because a sleeping box can't be asked.
- When the box doesn't answer, a banner offers Retry and Wake. Wake (`WakePlan`) sends to the
  phone's Wi-Fi broadcast, the box's broadcasts, `255.255.255.255` and the box's last address,
  on ports 9 and 7, but only when it can work; otherwise it says why: never learned, Wi-Fi
  only or not set up (and to use model sleep next time), not on Wi-Fi, or on another network.
- Before suspend or shut down, if the phone couldn't wake the box again, the confirm says so
  and offers "Sleep just the model instead".
- **iOS limit:** sending to a broadcast address needs Apple's multicast networking
  entitlement (`com.apple.developer.networking.multicast`), which Apple grants on request.
  Without it only the last-address packet leaves the phone, and it reaches a sleeping box only
  while the network still remembers the box's address. A Home-app smart plug with the BIOS set
  to power on after power loss is the dependable fallback.
