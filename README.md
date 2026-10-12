# webtop

Lightweight web resource monitor: CPU, RAM, NVIDIA GPU (SM, Tensor, VRAM) and a process list.
Single ~1.5 MB binary with the UI embedded. CPU, memory and GPU are recorded continuously into a 15-minute buffer, so charts are already full when you open the page;
the process list (the expensive part) is only collected while a browser is watching.
The CPU card switches between an overall chart and a per-CPU view: a small chart per logical CPU, grouped by physical core (SMT siblings),
L3 domain (AMD CCD / Intel ring) and socket, as read from `/sys/devices/system/cpu`.

```
cargo build --release
./target/release/webtop -i tailscale0 --allow 100.64.0.0/10,fd7a:115c:a1e0::/48 --tailscale-cert
```

| Flag | Meaning |
|---|---|
| `-i, --interface IFACE` | Bind to every address on this interface (repeatable). Default: all interfaces. |
| `-b, --bind IP` | Bind to an explicit address (repeatable). |
| `-p, --port N` | Port (default 9898). |
| `-a, --allow IP/CIDR,...` | Client IP whitelist. Requests from anything else get 403. |
| `-d, --deny IP/CIDR,...` | Client IP blacklist, checked before `--allow`. |
| `--interval MS` | Sampling interval when nobody is watching, and the UI default (1000). Viewers can pick 250 ms–5 s in the UI; the fastest active viewer wins. |
| `--wait-iface SECS` | Wait for the interface to get an address at boot (default 60). |
| `--tailscale-cert` | Serve HTTPS with a certificate from `tailscale cert` for this node's MagicDNS name (renewed automatically). Tailscale clients on plain HTTP are redirected. |
| `--tls-cert FILE`, `--tls-key FILE` | Serve HTTPS with your own PEM certificate/key (reloaded when the files change). With `--tailscale-cert`: where to store the fetched ones. |
| `--list-interfaces` | Print interfaces and their addresses. |

## HTTPS over Tailscale
With `--tailscale-cert`, webtop serves `https://<host>.<tailnet>.ts.net:9898` using a Let's Encrypt certificate obtained through tailscaled.
Plain `http://` requests from Tailscale addresses are redirected there; other clients (e.g. on a LAN interface) keep getting plain HTTP on the same port. One-time setup:
1. Enable **HTTPS Certificates** in the Tailscale admin console (DNS page; MagicDNS must be on).
2. If webtop doesn't run as root: `sudo tailscale set --operator=$USER`.

The certificate is stored in `$STATE_DIRECTORY`, `$XDG_STATE_HOME/webtop` or `~/.local/state/webtop`, and refreshed every 12 h.
Client IPs are still the real Tailscale IPs, so `--allow`/`--deny` keep working (unlike putting `tailscale serve` in front).

## GPU metrics
- **SM / Tensor** come from NVML GPM (Hopper, Blackwell and newer): real SM activity and tensor-pipe activity.
  On older GPUs, SM falls back to NVML "GPU utilization" (time any kernel was running), and Tensor shows "–".
- NVML is loaded at runtime, so the binary still runs on machines without an NVIDIA GPU.

## systemd (user service)
`~/.config/systemd/user/webtop.service`:
```ini
[Unit]
Description=webtop
After=network-online.target tailscaled.service

[Service]
ExecStart=%h/.local/bin/webtop -i tailscale0 --allow 100.64.0.0/10,fd7a:115c:a1e0::/48 --tailscale-cert
Restart=on-failure

[Install]
WantedBy=default.target
```
Then run `systemctl --user enable --now webtop` (plus `loginctl enable-linger $USER` to start it at boot).

## License
Licensed under the [Apache License, Version 2.0](LICENSE).
