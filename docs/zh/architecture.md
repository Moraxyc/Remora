# 系统架构与安全设计

[简体中文](architecture.md) | [English](../en/architecture.md) | [返回总览](../../README.md)

## 1. 工作区结构与模块职责

Remora 工作区由协议编解码库、服务端主程序和静态单页前端三部分组成：

| 组件 | 路径 | 许可证 | 职责 |
|---|---|---|---|
| 协议编解码库 | `crates/remora-proto` | MIT OR Apache-2.0 | 纯内存编解码：REST 请求签名与 `{code, msg, data}` 信封解析（`cloud.rs`）、Engine.IO v4 / Socket.IO v5 帧编解码（`socketio.rs`），以及通过 `protox` 与 `prost-build` 从 `proto/gv_pb.proto` 生成的 `gv_pb` Protobuf 类型。 |
| 服务端主程序 | `crates/remora-cloud` | GPL-3.0-or-later | `remora` 可执行文件：Axum HTTP/WebSocket 服务、SQLite 存储、Argon2id 与 OIDC 认证、GCRA 限流、房间信令路由、TURN 凭据派生，并在编译期通过 `rust-embed` 内嵌前端构建产物。 |
| Web 控制台 | `web/` | MIT | SvelteKit 静态单页应用（`@sveltejs/adapter-static`，回退页面为 `app.html`），构建输出位于 `web/build`（或环境变量 `REMORA_WEB_DIR` 指定目录）。 |

`remora-proto` 仅包含数据结构的序列化与校验逻辑，采用 `MIT OR Apache-2.0` 双许可证发布，方便外部测试工具或客户端独立复用协议编解码器。

## 2. HTTP 路由与安全响应头

`remora_cloud::router` 按认证方式将入站请求划分为四组路由：

1. **客户端 REST 接口（`/api/v1/*`）**：当配置了 `sign_key_file` 时，整组路由挂载 `signing::require_signature` 中间件，限制请求体不超过 `1 MiB` 并校验 `x-param-sign` 签名。需要登录态的处理器从 `Authorization: Bearer <User JWT>` 提取并验证 `User`。
2. **Web 控制台接口（`/api/web/*`）**：所有响应固定附带 `Cache-Control: no-store`。受保护接口通过 `remora_session` Cookie 识别浏览器会话；`POST`、`DELETE` 等修改状态的请求还必须携带 `X-CSRF-Token` 请求头，由服务端与会话绑定的 CSRF 令牌做常时间比对。
3. **Socket.IO 信令入口（`/socket.io/`）**：接受 `EIO=4&transport=websocket` 的 WebSocket 升级请求。升级前服务端验证 `X-NRD-AUTH` 请求头中的 Room JWT，并查询 SQLite 确认 `devices.owner` 仍等于令牌中的 `sub`，从而拦截设备解绑后尚未过期的旧房间令牌。
4. **内嵌前端静态资源（`app::serve`）**：响应 `GET` 与 `HEAD` 请求；对 `/api/` 或 `/socket.io/` 下未命中的路径直接返回 `404`。`/_app/immutable/*` 下的带哈希资源返回 `Cache-Control: public, max-age=31536000, immutable`，`app.html` 等入口资源返回 `no-cache`。所有静态响应均附带 `X-Content-Type-Options: nosniff`、`X-Frame-Options: DENY`、`Referrer-Policy: no-referrer`，以及包含 SvelteKit 构建期脚本哈希的 `Content-Security-Policy`（限制 `frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'`）。

## 3. 令牌、会话与认证流程

### JWT 令牌（`token.rs`）

Remora 使用存储在 SQLite `secrets` 表中的 32 字节随机密钥（`token_key`）签发 `HS256` JWT。由于 UU 客户端会直接解码 JWT 载荷读取 `csrf` 等字段，令牌采用标准未加密 JWT 格式，并通过必填的 `kind` 字段区分用途：

| 令牌类型 | 载荷字段（Claims） | 有效期 | 校验与失效机制 |
|---|---|---|---|
| **User Token** | `kind: "user"`, `sub`, `sid`, `csrf`, `iat` | 长期有效 | 每次请求查询 SQLite `sessions` 表确认 `(sid, user_id)` 存在；客户端登出、Web 控制台吊销或解绑对应主机都会删除该行，后续请求立即返回 HTTP `422`（`code: 1120`）。 |
| **Room Token** | `kind: "room"`, `sub`, `device_id`, `role`（`host` 或 `viewer { from_device, take_over }`）, `exp` | 3600 秒（1 小时） | 到期自动失效，且每次建立 WebSocket 时核对 `devices.owner == sub`。 |

### 浏览器会话与管理员密码（`browser.rs` & `password.rs`）

用户在 Web 控制台登录成功后，服务端生成 32 字节随机十六进制字符串写入 `remora_session` Cookie（`HttpOnly; SameSite=Strict`，当 `public_url` 为 `https://` 时附加 `Secure`），有效期 30 天。SQLite `browsers` 表以该 Cookie 值的 SHA-256 摘要作为主键保存会话记录，原始会话令牌仅保存在客户端浏览器中。管理员密码登录将用户输入与 `login.password_file` 中的 Argon2id PHC 哈希放到 `tokio::task::spawn_blocking` 线程池中校验，通过后登录为内置管理员用户 `id = "1"`（`Owner`）。

### OpenID Connect 登录（`oidc.rs`）

OIDC 登录采用授权码模式（Authorization Code）配合 PKCE、`state` 与 `nonce`，在发起跳转时将校验参数存入有效期 600 秒的临时 Cookie `remora_oidc`（`Path=/api/web/oidc; HttpOnly; SameSite=Lax`）。服务端将上游提供商的 Discovery 文档与 JWKS 公钥集缓存 1 小时，单次上游 HTTP 请求超时设为 10 秒，并通过 `rustls-platform-verifier` 使用操作系统 CA 信任库校验 TLS 证书。回调验签通过后，服务端以 `(issuer, subject)` 为唯一键在 `users` 表中创建或更新用户（`id` 从 `"2"` 起递增），同步刷新显示名称。

### 扫码登录状态流转（`login.rs`）

待确认的扫码登录保存在进程内存中，生存期为 180 秒。客户端调用 `POST /api/v1/qrcode/gen/login` 时，服务端生成两组独立的随机凭证：放入 `{public_url}/login/{id}` 的页面 `id`，以及返回给客户端的 `status_query_ticket`；后续状态轮询与令牌兑换必须同时匹配这两项凭证。浏览器请求 `GET /api/web/logins/{id}` 会将状态从 `Waiting (1)` 更新为 `Scanned (2)`。用户在页面点击确认后，状态变为 `Confirmed (4)` 并通过 `tokio::sync::watch` 通道立即唤醒客户端最长 25 秒的 `qrcode/login/status` 长轮询；点击拒绝则会删除该条目，使长轮询唤醒并返回 `QR_EXPIRED (1190)`。客户端随后调用 `login/by_qrcode` 一次性取出并销毁已确认的条目，换取 User Token。当配置了 `login.auto_confirm = true` 时，条目在生成时直接处于 `Confirmed (4)` 并绑定 `Owner`（`id = "1"`）。

## 4. 信令路由与房间状态机（`rooms.rs` & `signaling.rs`）

`Rooms` 是按 `device_id` 索引的内存路由表。每个房间记录当前在线的被控端连接 `host: Option<Peer>`、处于重连宽限期内的原被控端连接标识 `departed: Option<String>`、已连入的控制端集合 `viewers: HashMap<String, Peer>`，以及按 `ice_id` 索引的活跃控制会话 `sessions: HashMap<String, Session>`（记录 `viewer`、`from_device` 与表示桌面占用的 `desktop: bool`）。

每个连入 `/socket.io/` 的 WebSocket 连接都会分配一个 16 字节随机十六进制 `client_id`（同时作为 Engine.IO `sid`）。控制端发送 `soac` 或 `forward_setting` 时，服务端先校验消息内 `/data/ice_id` 属于当前控制端连接持有的会话，随后才转发给被控端；无论消息流向哪一侧，服务端都会把载荷中的 `client_id` 改写为发送方的真实连接 ID，确保接收方拿到的连接标识可信。

当控制端发送携带二进制附件的 `control` 事件时，`rooms.rs` 会解码附件中的 `gv_pb.ConnectOptions`。若 `capture_type <= 2`（`CT_UNKNOWN`、`CT_DESKTOP`、`CT_WINDOW`）或附件解码失败，该会话被判定为桌面控制会话；文件传输、终端与端口映射等辅助会话（`capture_type > 2`）可与桌面会话并存。同一房间内同一时刻只允许一个控制端设备（按 `from_device` 区分）持有桌面会话：当已有其他设备占用桌面且新请求未设置 `force_join: true` 时，REST `/room/join/by_device` 返回错误码 `2002`，信令 `control` 回复 `["failure", {"reason": "controlled from another device"}]`；当请求携带 `force_join: true` 时，服务端先向被控端发送 `released` 并向原控制端发送 `left` 终止旧桌面会话，再为新控制端分配 `ice_id` 并向被控端下发 `be-controlled`。

由于桌面媒体流与控制指令运行在端到端的 WebRTC 通道上，被控端信令连接短暂重连时，既有远程桌面会话可继续保持。如果被控端断开 WebSocket 时房间内仍有活跃的 `sessions`，服务端会将该被控端连接 ID 记入 `departed` 并启动 30 秒定时器（`ReconnectGrace`）。只要被控端在 30 秒内重新连入同一房间，就能直接接管原有会话；若 30 秒超时仍未重连，服务端会向所有受影响的控制端发送带 `ice_id` 的 `left` 事件并清理房间。

当新的被控端连接顶替当前在线的被控端，或用户在客户端及 Web 控制台解绑设备、吊销被控端登录会话时，服务端会向原被控端发送不带 `ice_id` 的 `left` 事件（载荷为 `{}`），通知其已被踢出并随即关闭连接。

## 5. 客户端地址解析与 GCRA 限流

### 客户端地址提取（`address.rs`）

`ClientAddress` 提取器为限流器与扫码确认页面提供客户端真实 IP。对于 TCP 连接，若直接对端 IP 未命中 `trusted_proxies`，提取器直接采用 TCP 套接字的对端地址；若对端命中 `trusted_proxies`，或连接来自 Unix 域套接字，提取器会从右向左逐项解析 `X-Forwarded-For` 请求头（支持纯 IP、`IP:port`、`[IPv6]:port` 与 IPv4 映射的 IPv6 地址），返回遇到的第一个非受信任代理地址；客户端在请求头左侧附加的地址位于该跳左侧，会在扫描停止后被直接忽略。

### GCRA 限流器（`limit.rs`）

匿名接口与密码验证接口均使用通用信元速率算法（GCRA）限流。限流键对 IPv4 按单地址统计，对 IPv6 按 `/64` 前缀聚合，防止调用方在同一 `/64` 子网内轮换地址绕过单 IP 配额。每个限流器最多维护 `MAX_TRACKED = 65_536` 条记录；表满时优先清理配额已完全恢复的旧记录，若所有记录均处于限流冷却期，则将新键归入共享溢出桶以保证内存占用恒定有界。

| 限流操作 | 统计维度 | 突发上限（Burst） | 恢复速率 |
|---|---|---|---|
| Owner 密码验证 | 每客户端 IPv4 / IPv6 `/64` | 5 次 | 每 60 秒 1 次 |
| Owner 密码验证 | 全局总计 | 30 次 | 每 10 秒 1 次 |
| 生成登录二维码 | 每客户端 IPv4 / IPv6 `/64` | 10 次 | 每 10 秒 1 次 |
| 设备初始化注册 | 每客户端 IPv4 / IPv6 `/64` | 10 次 | 每 60 秒 1 次 |
| 网络唤醒请求 | 每客户端 IPv4 / IPv6 `/64` | 5 次 | 每 30 秒 1 次 |

## 6. SQLite 存储与日志脱敏

`database.rs` 打开 SQLite 时设置 `journal_mode = WAL`、`synchronous = NORMAL` 与 5 秒 `busy_timeout`，所有表均启用 `STRICT` 类型检查，并通过 `PRAGMA user_version` 顺序迁移（当前版本为 `6`）：

| 表名 | 主要列 | 用途 |
|---|---|---|
| `secrets` | `name TEXT PRIMARY KEY, value BLOB` | 持久化 32 字节的 JWT 签名密钥 `token_key`。 |
| `devices` | `id TEXT PRIMARY KEY, client_id TEXT UNIQUE, name TEXT, platform INTEGER, owner TEXT` | 按客户端安装标识 `client_id` 幂等注册设备；首次调用 `room/create` 时写入 `owner`。 |
| `sessions` | `id TEXT PRIMARY KEY, user_id TEXT, device_id TEXT, created INTEGER` | 活跃的客户端 User Token 会话；在 `device_id` 上建有索引，供设备解绑时一并清理。 |
| `users` | `id TEXT PRIMARY KEY, issuer TEXT, subject TEXT, name TEXT` | 预置 `id = '1'`（`Owner`）；OIDC 用户按 `UNIQUE (issuer, subject)` 建档。 |
| `browsers` | `id TEXT PRIMARY KEY, user_id TEXT, csrf TEXT, created INTEGER, expires INTEGER` | 浏览器登录会话，主键 `id` 为 `remora_session` Cookie 的 SHA-256 哈希。 |
| `lan_reports` | `device_id TEXT PRIMARY KEY, mac TEXT, ip TEXT, subnet_mask TEXT` | 每台设备最近一次上报的网络唤醒信息，设备离线后仍保留；解绑时删除。 |
| `files` | `id TEXT PRIMARY KEY, user_id TEXT, device_id TEXT, mime TEXT, md5 TEXT, content BLOB` | 上传的图片。`id` 随机生成并出现在文件 URL 中；每个用户的每台设备保留最近 4 份，正在使用的壁纸另外保留；设备解绑时删除其上传的文件。 |
| `wallpapers` | `device_id TEXT PRIMARY KEY, file_id TEXT` | 每台设备卡片展示的图片；解绑时删除。 |

服务端日志仅输出实体标识（`user_id`、`device_id`）、会话计数、字节长度与布尔状态。承载密钥或敏感凭据的类型（`SignKey`、`TurnSecret`、`OwnerPassword`、`Database`、`LanReport`、`FileId`）均手动实现了输出固定占位符（如 `SignKey(..)`）的 `Debug`，使 JWT、Cookie、二维码链接与 Ticket、密码及哈希、TURN 密钥与派生凭据、OIDC 客户端密钥以及 SDP 中的 `ufrag` / `pwd` 设备的 MAC、局域网地址与子网掩码，以及上传令牌与文件 URL 均被排除在日志与错误堆栈之外。
