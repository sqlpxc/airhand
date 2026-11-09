# 部署：中转服务器与网页控制台

[← 返回 README](../README.zh-CN.md) · [English](deploy.md)

## 中转服务器

服务器监听 **UDP 4433**。首次启动时会在 `docker-compose.yml` 同目录的 `data/` 下生成自签名证书，**请保留这个目录**：客户端靠证书指纹认出服务器，被控端固定的控制码也记在这里的 SQLite 数据库里（`airhand.db`，服务器运行时旁边还有 `-wal`、`-shm` 两个文件）。保存控制码的被控端（有 `config.toml` 的桌面客户端、Android）不管离线多久，都能从这里取回原来的控制码。

环境变量（见 `docker-compose.yml`）：

| 变量 | 默认值 | 说明 |
|---|---|---|
| `AIRHAND_LISTEN` | `0.0.0.0:4433` | 监听地址（UDP） |
| `AIRHAND_CERT` / `AIRHAND_KEY` | `/data/cert.pem` / `/data/key.pem` | 证书路径，不存在时自动生成 |
| `AIRHAND_CERT_NAMES` | `airhand,localhost` | 自动生成证书时写入的 SAN |
| `AIRHAND_DB` | 证书旁边的 `airhand.db` | SQLite 数据库，存固定的控制码（控制码、被控端密钥的哈希、最后在线时间）。丢了它，离线的被控端的控制码就可能分给别人（没人占用的话仍然能取回）。打不开时服务器把控制码记在内存里，并在日志里报错 |
| `AIRHAND_MAX_SESSIONS` | `4` | 每台被控端同时允许的控制端数量 |
| `AIRHAND_MAX_CONNS_PER_IP` | `128` | 同一个 IPv4 地址 / IPv6 的 /64 同时最多几条连接（网页控制台每个标签页一条；很多被控端共用一个出口 IP 时调大） |
| `AIRHAND_NEW_CODES_PER_IP` | `30` | 同一个 IPv4 地址 / IPv6 的 /48 每 10 分钟最多分配几个新控制码（取回原来的控制码不算） |
| `RUST_LOG` | `info` | 日志级别 |

镜像基于 distroless、以非 root 用户运行，约 28 MB。

## 网页控制台（Docker）

控制端也可以跑在容器里：用 Docker Compose 启动，浏览器打开一个端口就是控制台，操作的电脑上不用安装或构建任何东西。界面和桌面版的控制台完全一样（多会话、多屏总览、画质、声音、剪贴板……）；被控的电脑仍然运行 Airhand 客户端。

```bash
cd web
AIRHAND_SERVER=relay.example.com:4433 AIRHAND_SERVER_FP=<服务器证书指纹> docker compose up -d
# 也可以 cp .env.example .env，改好后直接 docker compose up -d
```

然后用浏览器打开：

- 运行容器的这台电脑：`http://localhost:8080`
- 其它电脑：`https://<这台电脑的地址>:8080`

同一个端口同时支持 HTTP 和 HTTPS。浏览器只在安全的页面（HTTPS 或 localhost）上提供 WebCodecs 视频解码，所以从其它电脑访问要用 https://（用 http:// 打开时页面会给出 https 的链接）。证书是第一次启动时生成的自签名证书，保存在 `web/data/` 里（绑定挂载，保留这个目录，浏览器就不会再次提示），浏览器第一次会提示不安全，确认继续即可。需要 Chrome / Edge、Safari 16.4+ 或 Firefox 130+。

环境变量（见 `web/docker-compose.yml` 和 `web/.env.example`）：

| 变量 | 默认值 | 说明 |
|---|---|---|
| `AIRHAND_SERVER` | （必填） | 中转服务器地址 `host:port` |
| `AIRHAND_SERVER_FP` | 空 | 中转服务器的证书指纹。留空则第一次连上时记住服务器的证书（`data/server-trust.txt`），之后证书变了就拒绝连接；服务器确实换了证书时删掉这个文件 |
| `AIRHAND_NAME` | `Airhand Web` | 被控端托盘里“谁在控制”显示的名称 |
| `AIRHAND_WEB_IDENTITY` | `AIRHAND_WEB_CERT` 所在目录里的 `controller-key.txt`（镜像里是 `/data/controller-key.txt`） | 网页控制台的端到端加密身份（所有标签页共用的一把私钥，文件权限 600）。和网页控制台配对过的电脑靠它认出网页控制台；没有地方保存时只在内存里，每次重启都要重新配对。端到端加密的终点是网页控制台，它所在的机器看得到画面（见[安全](security.zh-CN.md#端到端加密)） |
| `AIRHAND_WEB_PASSWORD` | 空 | 访问密码：浏览器弹出登录框，用户名随意。留空则任何能访问这个端口的人都能使用控制台（连接每台电脑仍然需要它的控制码） |
| `AIRHAND_WEB_PORT` | `8080` | 映射到宿主机的端口 |
| `AIRHAND_WEB_CERT_NAMES` | `localhost` | 自动生成 HTTPS 证书时写入的域名 / IP（逗号分隔）。想把证书导入系统信任时，填上实际访问用的地址 |
| `AIRHAND_WEB_CERT` / `AIRHAND_WEB_KEY` | `/data/cert.pem` / `/data/key.pem` | HTTPS 证书路径。用自己的证书时放进 `web/data/`（或者改这两个路径） |
| `RUST_LOG` | `info` | 日志级别 |

- 每个浏览器标签页是一个独立的控制台，有自己到中转服务器的连接；关闭或刷新标签页会断开它的会话（有会话时浏览器会先确认）。和网页服务的连接断了会自动重连，会话随后自动恢复
- 画面要等浏览器收到后才确认，被控端的在途帧控制因此把“服务 → 浏览器”这段网络也算进去，这段网络慢时画面不会越积越多。工具栏显示的延迟 = 服务到被控端的往返 + 浏览器到服务的往返，所以服务最好部署在离浏览器近的地方（例如同一个局域网）
- 剪贴板用的是浏览器的剪贴板：对方复制的文本写进本机剪贴板（页面在前台时）；本机复制的文本在回到这个页面时发给对方，需要浏览器允许读取剪贴板（Chrome / Edge 第一次会询问）。Firefox 不允许网页自动读取剪贴板，只能从对方同步过来
- 不全屏时，Ctrl+W、Ctrl+T 等快捷键会被浏览器拦截；全屏时 Chrome / Edge 会锁定键盘，Win、Alt+Tab 等也发给远端
- 放在反向代理后面（例如用自己的域名和证书）：代理到容器的 `http://…:8080`，需要转发 WebSocket（`/ws`）并保留 `Host` 头（或设置 `X-Forwarded-Host`），否则会被当成跨站请求拒绝。可以放在子路径下
- 镜像基于 distroless、以非 root 用户运行，约 30 MB。只需要出站的 UDP（到中转服务器）和入站的这一个 TCP 端口
