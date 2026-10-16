# Box hardening

How a QuietBox running TT Station is locked down on the network, and how to
apply it: `sudo deploy/harden-box.sh` (safe to run again).

## What it changes

| Area | Setting | Why |
|---|---|---|
| SSH | Key sign-in only; no passwords, no keyboard-interactive | Nothing for password guessing to try. Paired computers already have a key (pairing installs one, labelled `ttstation:<computer>` in `~/.ssh/authorized_keys`). |
| SSH | No root sign-in, 3 tries per connection, 30 s to sign in | Smaller target, slower guessing. |
| Firewall (ufw) | Incoming denied except SSH (22/tcp), TT Station (8765/tcp, 8766/tcp) and mDNS (5353/udp) | Paired devices use HTTPS (8766). Discovery connects to the advertised port (8765) to resolve the box; the agent refuses plain HTTP from other machines itself. |
| Firewall (ufw) | Everything allowed from the box's own Docker networks | Containers (the Code sandbox gate, apps) reach the agent and the model through the Docker bridge. |
| Lockout (fail2ban) | An address is banned for an hour after 5 failed SSH sign-ins in 10 minutes | Stops scanning and key guessing. Watches `ssh.service` (Ubuntu's name), counting key refusals too. |

Docker-published ports bypass ufw. That's why services meant for this
computer only (the model server, Local Deep Research) are published on
`127.0.0.1`; see [tt-inference-server-docker.md](tt-inference-server-docker.md).

## What people notice

- Signing in to the box from a new computer with its username and password no
  longer works. Pair it with a code shown on the box instead (or add its SSH
  key at the box). Computers already paired keep working.
- Anything else on the network that expected to reach a port on the box (other
  than the three above) is refused. Open it deliberately with `sudo ufw allow`.

## Checking it

```bash
sudo sshd -T | grep -E '^(passwordauthentication|permitrootlogin|maxauthtries) '
sudo ufw status verbose
sudo fail2ban-client status sshd
```

## Undoing it

```bash
sudo rm /etc/ssh/sshd_config.d/01-tt-station.conf && sudo systemctl reload ssh
sudo ufw disable
sudo rm /etc/fail2ban/jail.d/tt-station.local && sudo systemctl restart fail2ban
```

## Remote access, end to end

1. The desktop app finds the box over mDNS and reads its TLS certificate.
2. Pairing is by a code shown on the box, or over SSH with a key the computer
   already has. Over SSH the box reports its certificate again; the app refuses
   if it doesn't match the one it saw on the network.
3. The box issues a 256-bit token for that device. Every later request is HTTPS
   (TLS 1.3) to that exact certificate (pinned; no certificate authority can
   vouch for an impostor).
