# Deployment: relay server and web console

[← Back to README](../README.md) · [简体中文](deploy.zh-CN.md)

## Relay server

The server listens on **UDP 4433**. On first start it generates a self-signed certificate in the `data/` directory next to `docker-compose.yml`. **Keep that directory**: clients recognise the server by the certificate's fingerprint, and it holds the SQLite database with the hosts' fixed control codes (`airhand.db`, plus its `-wal` / `-shm` files while the server runs). Every host that keeps its code (the desktop client with its `config.toml`, Android) gets the same code back from it, however long it has been offline.

Environment variables (see `docker-compose.yml`):

| Variable | Default | Description |
|---|---|---|
| `AIRHAND_LISTEN` | `0.0.0.0:4433` | Listen address (UDP) |
| `AIRHAND_CERT` / `AIRHAND_KEY` | `/data/cert.pem` / `/data/key.pem` | Certificate paths; generated automatically if missing |
| `AIRHAND_CERT_NAMES` | `airhand,localhost` | SANs written into an auto-generated certificate |
| `AIRHAND_DB` | `airhand.db` next to the certificate | SQLite database with the fixed control codes (code, hash of the host's secret, last seen). Without it, codes of hosts that are offline could go to other hosts (a host still gets its code back if nobody took it). If it can't be opened the server keeps the codes in memory and logs an error |
| `AIRHAND_MAX_SESSIONS` | `4` | Max simultaneous controllers per host |
| `AIRHAND_MAX_CONNS_PER_IP` | `128` | Max simultaneous connections per IPv4 address / IPv6 /64 (a web console uses one per browser tab; raise it if many hosts share one public IP) |
| `AIRHAND_NEW_CODES_PER_IP` | `30` | Max fresh control codes per IPv4 address / IPv6 /48 per 10 minutes (getting an existing code back doesn't count) |
| `AIRHAND_CONNECTS_PER_MINUTE` | `6` | Max connections per minute from one IPv4 address / IPv6 /64 to the same host (never lower than `AIRHAND_MAX_SESSIONS`; raise it if many controllers behind one public IP connect to the same host) |
| `RUST_LOG` | `info` | Log level |

The image is based on distroless, runs as a non-root user, and is about 28 MB.

**UDP buffers**: the server sets its socket's send and receive buffers to 8 MiB (the system default is only about 208 KB, which a burst of video at a few tens of Mbps fills up, dropping whole batches of packets: stutter and latency spikes). The Linux kernel silently caps anything above `net.core.rmem_max` / `wmem_max` (this can't be changed from inside the container; set it on the host), and the server logs a warning at startup when that happens:

```bash
sudo sysctl -w net.core.rmem_max=8388608 net.core.wmem_max=8388608
# to keep it after a reboot:
echo -e 'net.core.rmem_max=8388608\nnet.core.wmem_max=8388608' | sudo tee /etc/sysctl.d/90-airhand.conf
```

## Web console (Docker)

The controller can also run in a container: start it with Docker Compose and open one port in a browser to get the console, with nothing to install or build on the computer you control from. It's the same console as the desktop one (several sessions, multi-screen overview, quality, sound, clipboard…); the computers being controlled still run the Airhand client.

```bash
cd web
AIRHAND_SERVER=relay.example.com:4433 AIRHAND_SERVER_FP=<relay certificate fingerprint> docker compose up -d
# or: cp .env.example .env, edit it, then just docker compose up -d
```

Then open it in a browser:

- On the computer running the container: `http://localhost:8080`
- From other computers: `https://<this computer's address>:8080`

The same port speaks both HTTP and HTTPS. Browsers only offer WebCodecs video decoding on secure pages (HTTPS or localhost), so use https:// from other computers (opened over http://, the page links you to the https address). The certificate is a self-signed one generated on first start and kept in `web/data/` (a bind mount; keep that directory so the browser doesn't warn again); the first time, the browser warns about it, and you continue to the site. Needs Chrome / Edge, Safari 16.4+ or Firefox 130+.

Environment variables (see `web/docker-compose.yml` and `web/.env.example`):

| Variable | Default | Description |
|---|---|---|
| `AIRHAND_SERVER` | (required) | Relay server address `host:port` |
| `AIRHAND_SERVER_FP` | empty | Relay certificate fingerprint. Empty means the relay's certificate is remembered on the first connection (`data/server-trust.txt`) and a different one is refused later; delete that file if the relay really gets a new certificate |
| `AIRHAND_NAME` | `Airhand Web` | The name shown in the host's tray under who is controlling it |
| `AIRHAND_WEB_IDENTITY` | `controller-key.txt` next to `AIRHAND_WEB_CERT` (`/data/controller-key.txt` in the image) | The web console's end-to-end identity (one private key shared by all tabs, file mode 600). Computers paired with the web console recognize it by this key; without a place to keep it the key lives in memory and every restart needs pairing again. End-to-end encryption ends at the web console, so the machine running it can see the picture (see [Security](security.md#end-to-end-encryption)) |
| `AIRHAND_WEB_PASSWORD` | empty | Access password: the browser shows a login prompt, any user name works. Empty means anyone who can reach the port can use the console (connecting to a computer still needs its control code) |
| `AIRHAND_WEB_PORT` | `8080` | Port published on the Docker host |
| `AIRHAND_WEB_CERT_NAMES` | `localhost` | Names / IPs written into the generated HTTPS certificate (comma-separated). List the address you actually use if you want the OS to trust the certificate |
| `AIRHAND_WEB_CERT` / `AIRHAND_WEB_KEY` | `/data/cert.pem` / `/data/key.pem` | HTTPS certificate paths; to use your own certificate, put it in `web/data/` (or change these paths) |
| `RUST_LOG` | `info` | Log level |

- Each browser tab is a separate console with its own connection to the relay; closing or reloading the tab disconnects its sessions (the browser asks first while there are sessions). If the connection to the web service drops, the page reconnects and the sessions resume on their own
- Frames are acknowledged only once the browser has them, so the host's in-flight control covers the service-to-browser link too, and a slow link there doesn't make frames pile up. The latency in the toolbar is the service-host round trip plus the browser-service round trip, so run the service close to the browser (on the same LAN, for example)
- The clipboard is the browser's: text copied on the remote computer is written to this computer's clipboard (while the page is in front); text copied here is sent when you come back to the page, which needs the browser's permission to read the clipboard (Chrome / Edge ask the first time). Firefox doesn't let pages read the clipboard on their own, so there it only syncs from the remote side
- Outside fullscreen the browser keeps some shortcuts (Ctrl+W, Ctrl+T…) for itself; in fullscreen Chrome / Edge lock the keyboard, so the Win key, Alt+Tab and the like go to the remote computer
- Behind a reverse proxy (for your own domain and certificate): proxy to the container's `http://…:8080`, forward WebSockets (`/ws`) and keep the `Host` header (or set `X-Forwarded-Host`), otherwise the WebSocket is rejected as cross-site. A sub-path works too
- The image is based on distroless, runs as a non-root user, and is about 30 MB. It only needs outbound UDP (to the relay) and this one inbound TCP port
