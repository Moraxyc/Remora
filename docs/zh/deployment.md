# 部署与运维指南

[简体中文](deployment.md) | [English](../en/deployment.md) | [返回总览](../../README.md)

Remora 提供客户端所需的 REST 接口、Web 登录与管理控制台以及 Socket.IO 信令服务。生产部署时，在 Remora 前方放置终止 TLS 的反向代理，将 HTTPS 与 WebSocket 请求转发至 Remora 的 TCP 端口或 Unix 域套接字；被控端（Host）与控制端（Viewer）完成信令握手后，直接通过 WebRTC 点对点或经由 `coturn` 传输桌面音视频、输入与文件数据。

```text
客户端 (Host / Viewer)
   │
   ├── HTTPS / WSS ──► TLS 反向代理 ──► Remora (TCP 或 Unix 套接字)
   │                                         └─ 签发 24 小时 TURN 凭据
   └── STUN / TURN (UDP/TCP) ─────────► coturn (跨 NAT 或隐藏两端 IP 时启用)
```

Remora 启动时会校验两项配置约束。第一，`public_url` 只填写协议、主机名与可选端口（例如 `https://remora.example` 或 `https://remora.example:8443`）；因为 UU 客户端对 `/api/v1/...` 原始请求路径计算 HMAC-SHA256 签名，反向代理必须在根路径下原样转发请求，带有路径后缀的 `public_url` 会在启动时报错退出。第二，配置文件中必须至少启用一种登录确认方式：`login.password_file`、`[oidc]` 或 `login.auto_confirm = true`。

公网可达的实例还应配置 `sign_key_file`。设备初始化注册与生成登录二维码两个接口在客户端尚未获取用户令牌时调用，配置共享签名密钥后，只有持有同一密钥的客户端才能通过 `x-param-sign` 校验。

## 1. 安装

### Debian 与 Ubuntu（APT 仓库与 `.deb` 包）

Remora 发布在 [`pkgs.moraxyc.com`](https://pkgs.moraxyc.com) APT 仓库（支持 `amd64` 与 `arm64`），添加该源后即可通过 `apt` 安装并随系统常规更新自动升级；也可以直接从 GitHub Releases 下载对应架构的 `.deb` 包离线安装。添加 APT 仓库并安装 `remora`：

```sh
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://pkgs.moraxyc.com/deb/keys/moraxyc-archive-keyring.gpg | sudo tee /etc/apt/keyrings/moraxyc-archive-keyring.gpg > /dev/null
printf '%s\n' 'deb [signed-by=/etc/apt/keyrings/moraxyc-archive-keyring.gpg] https://pkgs.moraxyc.com/deb stable main' | sudo tee /etc/apt/sources.list.d/moraxyc.list
sudo apt-get update && sudo apt-get install remora
```

软件包会创建 `remora` 系统用户与用户组、属主为 `root:remora` 且权限为 `0750` 的 `/etc/remora` 目录，以及 `remora.service` 和 `remora.socket` 两个 systemd 单元（启用 systemd 沙箱并将 SQLite 数据库存放在 `/var/lib/remora/remora.db`）。

安装后复制示例配置、生成密钥文件，并将它们授权给 `remora` 组只读：

```sh
sudo cp /usr/share/doc/remora/examples/remora.toml /etc/remora/remora.toml

head -c 32 /dev/urandom | base64 | sudo tee /etc/remora/sign.key > /dev/null
head -c 32 /dev/urandom | base64 | sudo tee /etc/remora/turn.secret > /dev/null
remora hash-password | sudo tee /etc/remora/owner.password > /dev/null

# 按实际域名与认证方式编辑 /etc/remora/remora.toml 后执行：
sudo chgrp remora /etc/remora/*
sudo chmod 0640 /etc/remora/*

# 启动常驻服务，或启用 remora.socket 按需激活
sudo systemctl enable --now remora.service
```

卸载时，`apt remove remora` 会停止服务并保留 `/etc/remora` 与 `/var/lib/remora`；`apt purge remora` 会删除 `/var/lib/remora` 数据库（内含已注册设备、登录会话与 JWT 签名密钥），同时保留 `/etc/remora`。

### NixOS（Flake 模块）

本仓库 Flake 导出 `nixosModules.default`，配置按需激活的 `remora.socket` / `remora.service`。密钥路径通过 systemd `LoadCredential` 注入服务，磁盘上的密钥文件只需对 `root` 可读。同时启用 `services.remora.coturn.enable = true` 时，模块会在本机部署共享同一密钥的 `coturn`，自动向 `services.remora.settings.ice.servers` 填入对应的 `stun:` 与 `turn:` 地址，并在 `openFirewall = true` 时放行防火墙端口。

```nix
{
  imports = [ remora.nixosModules.default ];

  services.remora = {
    enable = true;
    settings = {
      public_url = "https://remora.example";
      listen = "[::1]:8680";
      trusted_proxies = [ "127.0.0.1" "::1" ];
      sign_key_file = "/run/secrets/remora-sign";
      ice.turn_secret_file = "/run/secrets/remora-turn";
      ice.force_relay = true;
      login.password_file = "/run/secrets/remora-owner";
    };

    coturn = {
      enable = true;
      domain = "turn.example";
      openFirewall = true;
    };
  };
}
```

### 静态二进制与手动 systemd 部署

GitHub Releases 同时提供静态编译的 `remora-<tag>-<arch>.tar.gz` 归档包，内含 `remora` 可执行文件以及 `contrib/` 下的配置文件与 systemd 单元模板。可直接指定配置文件运行：

```sh
remora --config /etc/remora/remora.toml
```

使用 [`contrib/remora.service`](../../contrib/remora.service) 与 [`contrib/remora.socket`](../../contrib/remora.socket) 托管时，单元默认开启 `DynamicUser=yes`、`MemoryDenyWriteExecute=yes` 和系统调用过滤；若启用 `remora.socket`，请保持其 `ListenStream` 与 `remora.toml` 中的 `listen` 一致。

## 2. 配置说明

[`contrib/remora.toml`](../../contrib/remora.toml) 包含全部配置项及注释。配置文件中的相对路径均相对于 `remora.toml` 所在目录解析，所有密钥文件在读取时自动去掉末尾单个 `\n` 或 `\r\n`。

### 监听地址、反向代理与客户端 IP

| 配置项 | 默认值 | 说明 |
|---|---|---|
| `listen` | `"[::1]:8680"` | TCP 监听地址（`IP:Port`）或 Unix 域套接字路径（如 `"/run/remora/remora.sock"`）。 |
| `public_url` | 由 `listen` 推导 | 客户端访问的外部 Origin（如 `https://remora.example`）。当 `listen` 为通配地址（`0.0.0.0` / `[::]`）或 Unix 套接字时必填。 |
| `trusted_proxies` | `[]` | 可信反向代理的 IP 或 CIDR 列表（如 `["127.0.0.1", "::1"]`）。 |

限流器与扫码确认页面都按客户端 IP 区分请求来源。在 TCP 监听模式下，只有当直接连接的对端地址命中 `trusted_proxies` 时，Remora 才会解析 `X-Forwarded-For` 请求头，从右向左逐跳跳过可信代理，取第一个非受信任代理地址作为客户端真实 IP。若未将本机反向代理加入 `trusted_proxies`，所有经代理进入的请求都会被归入代理自身的 IP，共享同一个限流桶。通过 Unix 域套接字传入的连接默认信任 `X-Forwarded-For`；搭配 `remora.socket` 使用 Unix 套接字时，可在 socket 单元中设置 `SocketGroup` 与 `SocketMode`（例如 `www-data` 与 `0660`）授权反向代理连接。

反向代理需要向后端传递 `Host`、`X-Forwarded-For` 以及 `/socket.io/` 的 WebSocket `Upgrade` / `Connection` 头。由于客户端轮询二维码状态单次最长挂起 25 秒、Socket.IO 心跳间隔也是 25 秒，代理的读取超时应设为 60 秒以上：

```nginx
server {
    listen 443 ssl http2;
    server_name remora.example;

    location / {
        proxy_pass http://127.0.0.1:8680;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_read_timeout 90s;
    }
}
```

### 登录确认方式

Remora 支持三种登录确认方式，可单独使用或同时启用 Owner 密码与 OpenID Connect：

1. **Owner 密码（`login.password_file`）**：执行 `remora hash-password > owner.password`（或在脚本中执行 `printf '%s\n' "$PW" | remora hash-password`）生成 Argon2id 密码哈希文件。使用该密码登录 Web 控制台会认证内置管理员用户（`id = "1"`，名称 `Owner`），浏览器会话保持 30 天。密码验证受双层限流保护：每个客户端（IPv4 单地址或 IPv6 `/64` 网段）突发允许 5 次尝试、之后每 60 秒恢复 1 次；全局突发上限为 30 次、每 10 秒恢复 1 次。请设置足够长的密码，防止外部持续猜测触及全局上限而拖慢管理员登录。
2. **OpenID Connect（`[oidc]`）**：对接 Authelia、Authentik、Keycloak 等身份提供商，并在提供商处注册回调地址 `{public_url}/api/web/oidc/callback`。配置 `issuer` 与 `client_id` 后，若设置 `client_secret_file` 则作为机密客户端认证，留空则作为公共客户端通过 PKCE 认证；可选通过 `name` 自定义登录按钮文字（默认显示 `issuer` 主机名）。每个经提供商认证的 `(issuer, sub)` 自动映射为独立的 Remora 用户，拥有各自独立的设备列表。只配置 `[oidc]` 时，打开登录页会直接跳转至身份提供商；同时配置 `login.password_file` 时，页面会并列提供两种登录入口。
3. **内网自动确认（`login.auto_confirm = true`）**：二维码一经生成即自动以 `Owner`（`id = "1"`）身份确认，客户端跳过浏览器扫码直接完成登录。该模式会让任何能访问服务的客户端直接登录为 Owner，仅适用于完全受控的隔离内网。

### 请求签名、数据库与日志

`sign_key_file` 指定客户端请求签名密钥文件的路径。建议填入 32 字节以上的随机字符串，并在所有客户端配置相同密钥，使 `/api/v1/*` 接口强制校验 `x-param-sign` 请求头。Web 控制台接口（`/api/web/*`）与信令 WebSocket（`/socket.io/`，使用 Room JWT 鉴权）使用各自的会话凭据。

`database` 指定 SQLite 数据库文件路径（如 `/var/lib/remora/remora.db`），用于持久化 JWT 签名密钥、已注册设备、用户及登录会话；留空时使用内存数据库，进程重启后所有会话与设备绑定都会重置。`log` 接受 `EnvFilter` 日志过滤指令，默认为 `"info"`，排查问题时可设为 `"info,remora_cloud=debug"`。

## 3. TURN 中继（coturn）与 ICE 配置

当被控端与控制端位于对称型 NAT 后方，或希望隐藏通信双方的真实 IP 时，可部署 `coturn` 并开启 `use-auth-secret`。Remora 与 `coturn` 共享同一个静态密钥，在每次建立控制会话时派生有效期 24 小时的 HMAC-SHA1 临时凭据下发给两端。

仓库中的 [`contrib/turnserver.conf`](../../contrib/turnserver.conf) 给出了配套配置：

```ini
listening-port=3478
# 若 coturn 位于 NAT 后方，填写公网 IP
#external-ip=203.0.113.10
realm=turn.example
fingerprint

use-auth-secret
# 与 remora.toml 中 ice.turn_secret_file 的内容保持一致
static-auth-secret=CHANGE-ME

min-port=49152
max-port=65535

no-tls
no-dtls
no-cli

no-multicast-peers
denied-peer-ip=0.0.0.0-0.255.255.255
denied-peer-ip=10.0.0.0-10.255.255.255
denied-peer-ip=100.64.0.0-100.127.255.255
denied-peer-ip=127.0.0.0-127.255.255.255
denied-peer-ip=169.254.0.0-169.254.255.255
denied-peer-ip=172.16.0.0-172.31.255.255
denied-peer-ip=192.168.0.0-192.168.255.255
denied-peer-ip=::1
denied-peer-ip=fc00::-fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff
denied-peer-ip=fe80::-febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff
```

在 `remora.toml` 中配置对应的 `[ice]` 段：

```toml
[ice]
servers = ["stun:turn.example:3478", "turn:turn.example:3478"]
turn_secret_file = "/etc/remora/turn.secret"
force_relay = true
```

启用 `force_relay = true` 会指示客户端仅使用 `relay` 候选地址，所有媒体流量均经由 `coturn` 转发，通信双方只看得到中继服务器地址；开启此项时 `servers` 中必须包含至少一个 `turn:` 或 `turns:` 地址。防火墙需放行 `3478/tcp`、`3478/udp`（启用 TLS/DTLS 时还需 `5349`）以及 UDP 中继端口段 `49152-65535/udp`。示例配置中的 `denied-peer-ip` 规则用于阻止客户端借中继探测服务器所在内网；如果在同一私网（如 `192.168.0.0/16`）内测试 TURN 转发，请先从 `turnserver.conf` 中移除对应网段。

## 4. 客户端接入（`uu-remote-linux`）

本仓库 Flake 打包了打过补丁的 `uu-remote-linux`（`packages.uu-remote-linux` 与 `overlays.default`），支持通过环境变量或配置文件指定 Remora 地址与签名密钥：

```sh
export UU_BASE_URL="https://remora.example"
export UU_SIGN_KEY="与服务端 sign_key_file 一致的密钥"
```

也可以在 `uu-remote-linux` 的 JSON 配置文件中写入 `"base_url"` 和 `"sign_key"` 字段。

启动被控端 `uuremote-server` 后，客户端会向 Remora 注册设备并展示包含 `{public_url}/login/<id>` 的登录二维码。在浏览器中打开该链接并使用 Owner 密码或 OIDC 登录，核对页面显示的设备名称、平台与来源 IP 后点击 **Log in**，被控端即获取用户令牌、创建房间并接入 Socket.IO 信令。随后在控制端 `uuremote-viewer` 登录同一账号，即可在设备列表中看到在线的被控端并发起远程桌面连接。

## 5. 日常运维与故障排查

在浏览器中访问 `{public_url}/` 可打开管理控制台。**Devices** 标签页列出当前账号绑定的所有设备及在线状态，点击 **Unbind** 会解除设备绑定、删除该被控端的登录会话并向在线被控端下发踢出信令；**Client logins** 标签页列出活跃的客户端令牌并支持逐个 **Revoke**；**Browsers** 标签页列出当前账号的浏览器登录会话并支持远程注销。

启用 `remora.socket` 时，systemd 预先绑定监听套接字，并在收到首个连接时启动 `remora.service` 并移交套接字句柄。执行 `systemctl restart remora.service` 升级或重载配置期间，新到达的连接会暂存在内核套接字队列中，待新进程启动后继续处理；进程启动后会保持运行，以维持活跃的 WebSocket 信令房间。

持久化数据全部保存在 SQLite 数据库文件（默认 `/var/lib/remora/remora.db`，运行在 WAL 模式）中。在线备份请使用 SQLite 的备份命令，以确保 `-wal` 日志中的事务一并写入备份文件：

```sh
sudo sqlite3 /var/lib/remora/remora.db ".backup /tmp/remora-backup.db"
```

迁移服务器时，将备份好的 `remora.db` 与 `/etc/remora/` 下的配置文件及密钥一同复制到新主机即可保留所有已绑定设备与登录态。

### 常见报错与处理

| 现象或报错信息 | 原因 | 解决办法 |
|---|---|---|
| 启动报错：`nobody can confirm logins` | 未配置任何登录确认方式 | 在 `remora.toml` 中配置 `login.password_file`、`[oidc]` 或 `login.auto_confirm = true`。 |
| 启动报错：`public_url is required when listening on all addresses or a Unix socket` | `listen` 为 `0.0.0.0`、`[::]` 或 Unix 套接字时未设置 `public_url` | 设置只含协议、域名与可选端口的 `public_url = "https://..."`。 |
| 启动报错：`relaying every session needs a TURN server` | 开启了 `ice.force_relay = true`，但 `ice.servers` 中没有 `turn:` / `turns:` 地址 | 在 `ice.servers` 中加入 TURN 地址并配置 `ice.turn_secret_file`。 |
| 启动日志警告：`the socket systemd passed is not on listen` | `remora.socket` 的 `ListenStream` 与 `remora.toml` 的 `listen` 不一致 | 对齐两处地址后执行 `systemctl daemon-reload && systemctl restart remora.socket`。 |
| 密码登录返回 `429 Too Many Requests` | 触发密码尝试限流，或反向代理未加入 `trusted_proxies` | 等待 60 秒后重试；确认 `trusted_proxies` 包含反代 IP 且反代传递了 `X-Forwarded-For`。 |
| 扫码页面提示已过期或客户端反复刷新二维码 | 超过 180 秒二维码有效期，或反代提前断开 25 秒长轮询 | 重新生成二维码；将反向代理的读取超时（如 `proxy_read_timeout`）调至 60 秒以上。 |
| 客户端请求返回 HTTP `403`（`code: 1002`） | `sign_key_file` 密钥与客户端不一致，或反代改写了请求路径 | 核对服务端 `sign.key` 与客户端 `UU_SIGN_KEY` 完全一致，并确保反代按原始路径转发。 |
| OIDC 登录后跳回 `?sign_in=failed` | 回调 URI 不匹配、系统时间偏差或 IdP 证书未受系统信任 | 确认 IdP 注册的回调地址为 `{public_url}/api/web/oidc/callback`，校准系统时钟并检查系统 CA 证书库。 |
| 设备显示在线但远程桌面无法建立连接 | TURN 密钥不一致、UDP 端口未放行或 NAT 后未填 `external-ip` | 核对 `turn.secret` 与 `static-auth-secret`，放行 `3478` 及 `49152-65535/udp`，并在 NAT 环境下为 `coturn` 配置 `external-ip`。 |
