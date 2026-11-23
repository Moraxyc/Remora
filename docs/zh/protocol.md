# 线协议与接口参考

[简体中文](protocol.md) | [English](../en/protocol.md) | [返回总览](../../README.md)

## 1. 请求签名（`x-param-*`）

当服务端配置了 `sign_key_file` 时，所有 `/api/v1/*` 请求必须携带 `x-param-sign` 请求头通过 HMAC-SHA256 校验（单次请求体上限为 `1 MiB`）。签名与常时间验签实现位于 [`crates/remora-proto/src/control/signing.rs`](../../crates/remora-proto/src/control/signing.rs) 与 [`crates/remora-proto/src/server/verify.rs`](../../crates/remora-proto/src/server/verify.rs)。

签名按以下顺序计算：

1. 收集所有名称以 `x-param-` 开头（忽略大小写）的 HTTP 请求头，排除 `x-param-sign` 本身。
2. 将这些请求头名称转为小写，按字节字典序升序排列，格式化为 `name=value` 并以 `&` 拼接。
3. 依次拼接以下四段字节流并计算 HMAC-SHA256：大写 HTTP 方法（如 `GET`、`POST`）、原始请求路径与查询串（如 `/api/v1/device/list`）、步骤 2 得到的参数串，以及原始请求体字节（空请求体 `""` 与 `"{}"` 会得出不同的摘要）。
4. 将 32 字节 MAC 编码为 64 位小写十六进制字符串，填入 `x-param-sign` 请求头。

## 2. 响应信封与错误码（`/api/v1/*`）

所有 `/api/v1/*` 接口均返回统一的 JSON 信封：

```json
{
  "code": 0,
  "msg": "ok",
  "data": {}
}
```

客户端以 HTTP 状态码 `200` 且 `code == 0` 判定调用成功。错误响应使用以下状态码与业务码：

| HTTP 状态 | `code` | `msg` 与触发条件 |
|---|---|---|
| `422` | `1001` | `invalid request`：JSON 解析失败、缺少必填字段或请求体超过 1 MiB。 |
| `403` | `1002` | `invalid signature`：缺少 `x-param-sign` 或 HMAC 摘要不匹配。 |
| `422` | `1120` | `login expired`：缺少 User Token、签名无效或对应 `sid` 会话已注销；客户端收到后应清除本地令牌并重新发起扫码登录。 |
| `400` | `1130` | 无法发起远程协助（不能对同账号设备发起协助，或对方未开启/未应答协助）。 |
| `400` | `1131` | 分享设备 ID 或远程协助验证码错误。 |
| `400` | `1134` | 对方已拒绝或取消本次确认式远程协助。 |
| `400` | `1136` | 等待对方在桌面确认远程协助请求（响应 `data` 附带 `control_id` 供轮询）。 |
| `400` | `1137` | 不能将同账号设备加入分享收藏列表。 |
| `400` | `1190` | `QR code expired`：二维码 ID 未知、已超过 180 秒有效期、已被拒绝或已兑换过。 |
| `429` | `1429` | `too many requests`：触发 GCRA 速率限制。 |
| `400` | `2001` | `device is offline`：目标被控端当前未连入信令房间。 |
| `400` | `2002` | `被控端正在被远控中，无法发起连接`：已有其他设备持有桌面会话且请求中 `force_join` 为 `false`。 |
| `400` | `2003` | `no such device`：设备不存在或归属于其他用户。 |
| `400` | `2004` | `guest room required`：访客被控端尚未调用 `guest/room/create` 就查询 `guest/share/info`。 |
| `500` | `1000` | `internal error`：服务端内部或 SQLite 存储错误。 |

## 3. 客户端 REST 接口（`/api/v1/*`）

需要登录态的接口通过 `Authorization: Bearer <User JWT>` 传递用户令牌；客户端同时通过 `X-Param-Device-Id: <device_id>` 上报自身设备 ID。

### 设备接口

| 方法与路径 | 鉴权 | 请求体 | 响应 `data` 与行为 |
|---|---|---|---|
| `POST /api/v1/device/windows/init` | 匿名（限流） | `{"client_id": "...", "name": "...", "controllable": true}`（`name`、`controllable` 可选，其余硬件字段被忽略） | 返回完整 `Entry`（包含 `device_id`）。按客户端安装标识 `client_id` 幂等注册：重复调用返回既有 `device_id` 并更新名称及 `X-Param-VN`，平台记为 `1`（Windows）。 |
| `POST /api/v1/device/macos/init` | 匿名（限流） | `{"client_id": "...", "name": "...", "controllable": true}`（`name`、`controllable` 可选） | 与 `windows/init` 行为一致，平台记为 `4`（macOS）。 |
| `POST /api/v1/device/mac_controllable` | User Token + `X-Param-Device-Id` | `{"controllable": true\|false}` | `{}`。更新调用方设备（未绑定或归属于当前用户）是否允许被远程控制。 |
| `GET /api/v1/device/list` | User Token（可选 `X-Param-Device-Id`） | 空 | `{"current_device": Entry \| null, "my_binded_devices": [Entry], "others_shared_devices": [Entry]}`（含当前仍活跃的曾协助设备）。每个 `Entry` 包含 `device_id`、可选 `alias`、`platform`、`status`（`"CONNECTED"` 或 `"DISCONNECTED"`）、`participants_info`、可选 `wallpaper_url`、`version_name`、`support_wol`、`controllable`、`publisher_availability_status`（桌面端为 `"ready"` / `"control_off"` / `"offline"`）、`controlled_support` 与 `update_started_at`。 |
| `GET /api/v1/device/groups/of/my` | User Token（可选 `X-Param-Device-Id`） | 空 | 按形态分组返回名下设备：`{"current_device_id": string \| null, "mobile_devices": [Entry], "desktop_devices": [Entry], "tv_devices": [Entry]}`。 |
| `GET /api/v1/device/{device_id}/preferences` | User Token | 空 | 返回指定已绑定设备的工具栏排序配置 `{"device_preferences": {"device_id", "tools": {"sequence": [...]}}}`。 |
| `GET /api/v1/device/share/info` | User Token + `X-Param-Device-Id` | 空 | 返回调用方设备的远程协助 9 位识别码 `connect_id`、`connect_code`（`""`）与显示名称 `alias`。 |
| `GET /api/v1/app/list/of/device/{device_id}` | User Token | 空 | 返回指定已绑定设备上报的应用列表 `{"device_app_list": [...]}`。 |
| `POST /api/v1/app/list/of/current_device` | User Token + `X-Param-Device-Id` | `{"device_app_list": [...]}` | 持久化当前已绑定被控端上报的应用列表并回显载荷。 |
| `POST /api/v1/device/{device_id}/unbind` | User Token | `{}` | `{}`。解除设备绑定，删除该被控端关联的登录会话、局域网信息、壁纸、上传文件与应用列表，并向在线被控端发送踢出信令。 |
| `POST /api/v1/device/wake_on_lan/info` | User Token + `X-Param-Device-Id` | `{"mac": "aa:bb:cc:dd:ee:ff", "inner_ip": "192.168.1.20", "ip": "192.168.1.20", "subnet_mask": "255.255.255.0"}` | `{"turn_on_wol": true}`。保存调用方已绑定设备最近一次上报的局域网信息，用于网络唤醒。`inner_ip` 为局域网地址且必填；字段缺失或无效（MAC 须为以 `:` 或 `-` 分隔的 6 个十六进制字节，地址须为 IPv4，子网掩码须连续）时返回 HTTP `422`、code `1001`。 |

### 文件与壁纸

被控端会上传桌面壁纸，用作设备卡片的背景。图片保存在 SQLite 中，任何持有图片 URL 的人都可以免凭据获取；每个 URL 带有一个 128 位随机 id，对应的内容永不改变。

| 接口 | 鉴权 | 请求体 | 响应与行为 |
|---|---|---|---|
| `GET /api/v1/tool/fp/token?filetype=1` | User Token | 空 | `{"token": "...", "req_url": "{public_url}/api/files", "ttl": 3600, "max_size": 10485760}`。客户端在 `ttl` 秒后才重新获取令牌，在此之前可多次复用。令牌在 `ttl` 到期后仍有效 10 分钟，确保客户端重新获取前的上传不会失败；前提是签发它的登录会话仍然有效。不检查 `filetype`。 |
| `POST /api/files` | `Authorization: <上传令牌>`（可直接写令牌，也可加 `Bearer ` 前缀） | 原始图片字节，或 `multipart/form-data`（取第一个文件部分） | 不使用 UU 响应信封：成功时返回 HTTP `200` 与 `{"url": "{public_url}/api/files/{id}", "fsize": 123, "md5": "<hex>", "mime": "image/png"}`。按文件内容识别并接受 PNG、JPEG、WebP、GIF 与 BMP（不依赖 `Content-Type`，也可以不带），大小不超过 `max_size` 字节。错误为 `{"error": ...}`：`401 unauthorized`、`400 empty`、`413 too_large`、`415 not_an_image`。每个用户的每台设备保留最近 4 次上传，正在使用的壁纸另外保留。 |
| `GET /api/files/{id}` | 无（URL 中的 `id` 即凭据） | — | 返回图片，附带图片类型、`Cache-Control: public, max-age=31536000, immutable`、`nosniff` 与沙箱 CSP。忽略查询串（如缩放参数）。 |
| `POST /api/v1/device/wallpaper` | User Token + `X-Param-Device-Id` | `{"wallpaper_url": "<上传返回的 url>"}` | `{}`。把调用方已绑定设备的壁纸设为该用户上传过的某个文件；按路径中的 id 匹配，不论 URL 指向哪个源站；传 `""` 表示清除壁纸。URL 对应的文件不存在时返回 `422` / `1001`；调用方设备未绑定时返回 `400` / `2003`。 |

### 扫码登录接口

| 方法与路径 | 鉴权 | 请求体 | 响应 `data` 与行为 |
|---|---|---|---|
| `POST /api/v1/qrcode/gen/login` | 匿名（限流） | `{}` | `{"qrcode_jump_url": "{public_url}/login/{id}", "status_query_ticket": "..."}`。有效期 180 秒；启用 `auto_confirm` 时生成即处于 `Confirmed (4)` 状态。 |
| `POST /api/v1/qrcode/login/status` | 匿名 | `{"token": "<ticket>", "qrcode_jump_url": "...", "login_status": 1}` | 当状态仍等于 `login_status` 时最长挂起 25 秒，状态变化时立即返回 `{"status": 1\|2\|3\|4}`（`1` Waiting、`2` Scanned、`3` Expired、`4` Confirmed）。 |
| `POST /api/v1/login/by_qrcode` | 匿名 | `{"token": "<ticket>", "qrcode_jump_url": "..."}` | `{"token": "<User JWT>", "user_id": "...", "nickname": "..."}`。单次有效，调用后立即移除内存中的二维码条目。 |

### 用户与房间接口

| 方法与路径 | 鉴权 | 请求体 | 响应 `data` 与行为 |
|---|---|---|---|
| `GET /api/v1/user/info` | User Token | 空 | 返回 `{"user_id": "...", "nickname": "..."}` 及官方桌面端兼容字段（`member_type`、`member_display_status`、`member_expired_at`、`coin_balance` 等）。 |
| `POST /api/v1/user/logout` | User Token | `{}` | `{}`。从 SQLite `sessions` 表删除当前令牌的 `sid`。 |
| `POST /api/v1/room/create` | User Token + `X-Param-Device-Id` | `{}` | 将该设备绑定至当前用户、记录 `X-Param-VN` 并关联当前会话，返回 `{"signaling_server": "wss://...", "signaling_list": [...], "sni_required_signaling_list": [...], "token": "<Host Room JWT>", "ws_connect_timeout_ms": 16000, "max_reconnect_delta": 300, "streamer_retry_delta_ms": 1000, "international_connect": false}`。 |
| `POST /api/v1/room/join/by_device/{device_id}` | User Token（可选 `X-Param-Device-Id`） | `{"force_join": false}` | 校验设备归属、被控端在线状态与桌面占用冲突，返回 `{"signaling_server": "wss://...", "signaling_list": [...], "sni_required_signaling_list": [...], "token": "<Viewer Room JWT>", "ws_connect_timeout_ms": 16000, "max_reconnect_delta": 300, "streamer_retry_delta_ms": 1000, "international_connect": false, "report_token": "", "report_url": "", "publisher_platform": <u8>}`。 |

### 访客与远程协助分享接口

Remora 支持未登录的访客被控端（`Guest Token` + `X-Param-Guest-Id`）与已登录被控端以 9 位 `connect_id` 发起远程协助；服务端不在数据库中保存明文验证码，而是通过 `bmsg_push` 实时协调验证模式、加盐摘要校验（`SHA-256(salt || connect_code)`）与桌面确认弹窗。

| 方法与路径 | 鉴权 | 说明 |
|---|---|---|
| `POST /api/v1/guest/create` | 匿名（限流 + `X-Param-Device-Id`） | 为已注册设备签发 `{"guest_id", "token"}` 访客令牌。 |
| `POST /api/v1/guest/room/create` | Guest Token | 创建或续期访客被控端的信令房间，返回 `RoomCreate` 结构与访客 Host Room JWT。 |
| `POST /api/v1/guest/share/info` | Guest Token | 要求先创建访客房间（否则返回 `400` / `2004`），激活并返回 `{"connect_id", "connect_code": "", "alias"}`。 |
| `POST /api/v1/guest/share/cancel`、`POST /api/v1/device/share/cancel` | Guest Token / User Token | 停用当前设备的分享状态并仅踢出受协助（`shared`）控制端会话，不影响设备所有者自身连接。 |
| `POST /api/v1/device/share/info/refresh_id`（及 `/refresh`） | User Token + `X-Param-Device-Id` | 轮换已绑定设备的 9 位 `connect_id`。 |
| `POST /api/v2/room/share/control_mode` | User Token | 向目标在线被控端推送 `bmsg_push` (`get_control_mode`) 查询 `can_remote_control`（并附带 `allow_control`）、`control_mode`（`by_password` / `by_confirmation` / `password_confirmation`）与 `need_password`；macOS 端省略上报时在 800 ms 宽限后按 `controllable` 回落。 |
| `POST /api/v1/guest/room/share/upload_control_mode`、`POST /api/v2/room/share/upload_control_mode` | Guest Token / User Token | 被控端应答 `get_control_mode` 挑战，上报当前协助开关与验证模式。 |
| `POST /api/v1/guest/room/share/upload/sign`、`POST /api/v2/room/share/upload_sign` | Guest Token / User Token | 被控端应答 `remote_control` 挑战，上报 `SHA-256(salt || code)` 摘要与 `can_remote_control`。 |
| `POST /api/v1/guest/room/share/confirmation`、`POST /api/v2/room/share/confirmation` | Guest Token / User Token | 被控端提交桌面确认结果（允许或拒绝）。 |
| `POST /api/v1/room/join/share/by_code`、`POST /api/v2/room/join/share/by_code` | User Token（限流） | 向目标被控端发送带随机 `salt` 的 `remote_control` 推送并在服务端校验摘要，通过后返回 `AssistRoomConfig` 与受协助 Viewer Room JWT。 |
| `POST /api/v2/room/join/share/by_confirmation` | User Token（限流） | 发起或轮询确认式远程协助；等待期间返回 `400` / `1136`（携带 `control_id`），拒绝返回 `400` / `1134`，同意后返回 `AssistRoomConfig`。 |
| `POST /api/v2/room/share/cancel_remote_assist` | User Token | 控制端取消等待中的确认请求并向被控端推送 `remote_control_by_confirmation_cancel`。 |
| `GET /api/v1/device/share/saved{,/favorites,/recent}`、`POST /api/v1/device/share/saved/favorites`、`DELETE /api/v1/device/share/saved/{kind}/{publisher_device_id}`、`POST /api/v1/device/share/{id}/unbind` | User Token | 管理控制端的最近协助记录、收藏列表与已分享设备解绑。 |

### 客户端兼容与更新存根接口

为防止官方桌面客户端（macOS / Windows）在启动与会话期间限制画质或拉取上游二进制更新，Remora 提供了以下兼容接口：

| 方法与路径 | 鉴权 | 说明 |
|---|---|---|
| `GET /api/v1/member/entitlements` | User Token | 解锁自托管串流与输入能力（`bluray` 画质、`144` FPS、最高 `500 Mbps` 自定义码率、HDR、真彩、第二屏幕、多通道、不限速文件与剪贴板传输等）。 |
| `POST /api/v1/tool/configures_with_version` | 匿名 / 签名 | 按请求的配置名返回 `status: 1 \| 2` 与 `data: null`，指示客户端保留内置默认配置。 |
| `GET /api/v1/cloud_pc/ui_switch` | User Token | 返回 `{"switch": false}`。 |
| `GET /api/v1/message/summary` | User Token | 返回 `{"no_read_num": 0}`。 |
| `GET /api/v1/campaign/strategies` | User Token | 返回 `{"strategy": null}`。 |
| `GET /api/v1/release/fetch/popups/info` | User Token | 返回 `{"popups": []}`。 |
| `GET /api/v1/release/fetch/update`、`GET /api/v1/release/fetch/update/{device_id}` | User Token | 返回 `{"update_type": "NoUpdate", "url": "", ...}`，避免自托管客户端从上游下载安装包覆盖自身。 |

## 4. Web 控制台接口（`/api/web/*`）

`/api/web/*` 直接返回 JSON 对象，错误响应格式为 `{"error": "<code>"}`，且所有响应携带 `Cache-Control: no-store`。需要认证的接口读取 `remora_session` Cookie，其中 `POST` 与 `DELETE` 请求还需携带与当前浏览器会话匹配的 `X-CSRF-Token` 请求头。

| 方法与路径 | 鉴权 | 说明 |
|---|---|---|
| `GET /api/web/session` | 匿名 | 返回当前登录用户、CSRF 令牌以及服务端启用的认证方式（`password`、`oidc`、`secure`）。 |
| `POST /api/web/session` | 匿名（限流） | 提交 `{"password": "..."}` 验证 Owner 密码，成功后写入 `remora_session` Cookie 并返回 `204`。 |
| `DELETE /api/web/session` | Cookie + CSRF | 注销当前浏览器会话并清除 Cookie。 |
| `GET /api/web/oidc/start?next=<path>` | 匿名 | 生成 PKCE / `state` / `nonce`，写入 `remora_oidc` 临时 Cookie 并重定向至 OIDC 提供商。 |
| `GET /api/web/oidc/callback` | 匿名 | 校验 OIDC 授权码与 ID Token，建档或更新用户并写入 `remora_session` Cookie。 |
| `GET /api/web/logins/{id}` | 匿名（URL `id` 凭证） | 将二维码标记为已扫描（`Scanned`），返回发起设备的名称、平台、来源 IP、创建时间及确认状态。 |
| `POST /api/web/logins/{id}` | Cookie + CSRF | 提交 `{"decision": "confirm" \| "deny"}` 确认或拒绝该次客户端扫码登录。 |
| `GET /api/web/devices` | Cookie | 列出当前用户绑定的设备、在线状态、`wakeable`（设备是否上报过局域网信息；不返回任何地址）及 `wallpaper`（壁纸图片的路径，没有时为 `null`）。 |
| `DELETE /api/web/devices/{id}` | Cookie + CSRF | 解绑指定设备、删除其被控端登录会话并踢出在线被控端。 |
| `POST /api/web/devices/{id}/wake` | Cookie + CSRF（限流） | 唤醒离线设备：在当前用户的在线设备中选一台上报的 IPv4 网段（地址与掩码）与目标相同的设备，向其推送 `wake_on_lan`，推送后返回 `204`。错误：`404 not_found`、`409 device_online`、`409 no_lan_report`、`409 no_wake_helper`、`429 too_many_requests`。 |
| `GET /api/web/client-sessions` | Cookie | 列出当前用户所有活跃的客户端登录会话。 |
| `DELETE /api/web/client-sessions/{id}` | Cookie + CSRF | 吊销指定客户端登录会话（若为在线被控端会话，同步踢出信令房间）。 |
| `GET /api/web/browsers` | Cookie | 列出当前用户所有活跃的浏览器登录会话。 |
| `DELETE /api/web/browsers/{id}` | Cookie + CSRF | 吊销指定浏览器登录会话。 |

## 5. Socket.IO / Engine.IO 信令协议

### 握手、心跳与二进制帧（`crates/remora-proto/src/signal/`）

客户端通过 `GET /socket.io/?EIO=4&transport=websocket` 发起连接，并在 `X-NRD-AUTH` 请求头中携带 Room JWT（令牌无效、过期或设备已解绑时返回 HTTP `401`）。WebSocket 升级完成后，服务端立即下发 Engine.IO Open 文本帧 `0{"sid":"...","upgrades":[],"pingInterval":25000,"pingTimeout":20000,"maxPayload":1000000}`。此后服务端每 25 秒发送文本帧 `"2"`（Ping），客户端需在 20 秒内回复文本帧 `"3"`（Pong）。

Engine.IO 建立后，客户端发送 `"40"` 加入 Socket.IO 默认命名空间，服务端回复 `40{"sid":"<client_id>"}`（每个连接独立生成的 16 字节随机十六进制连接 ID）。带二进制附件的事件采用 Socket.IO v5 帧格式：先发送一条文本帧 `45<N>-[<ack_id>]["<event>", ...]`，JSON 中以 `{"_placeholder": true, "num": 0}` 指代附件位置，随后紧跟 `<N>` 条 WebSocket Binary 帧。

### 信令事件

| 方向 | 事件名 | 载荷与处理行为 |
|---|---|---|
| Host → Cloud | `room_info`（带 Ack） | 被控端就绪通告。服务端回复 Ack `{"room_id","publisher","you","subscribers","iceServers","reconnect_buffer"}`，并立即向该被控端推送 `bmsg_push`：`{"data":{"status":"online","publisher_availability_status":"ready"}}`，房间随即进入在线可连接状态。 |
| Host / Viewer → Cloud | `refresh_reconnect_key`（带 Ack） | 服务端回复 Ack `["success", {"reconnect_key": "<32位十六进制>"}]`。 |
| Viewer → Cloud | `control`（带 Ack，含 1 个二进制附件） | 请求建立控制会话。JSON 参数包含 `app_control_id`、可选 `streamer_data` 以及指向序列化 `gv_pb.ConnectOptions` 附件的 `app_data` 占位符。允许接入时，服务端分配 `ice_id` 与临时 TURN 凭据，向被控端发送 `be-controlled`，并向控制端回复 Ack `["success", {"ice_id", "client_id": <host_id>, "app_control_id", "iceServers", "force_relay"}]`；拒绝时回复 `["failure", {"reason": "host offline" \| "controlled from another device"}]`。 |
| Cloud → Host | `be-controlled`（含 1 个二进制附件） | JSON 参数包含 `ice_id`、`client_id`（控制端连接 ID）、`app_control_id`、`iceServers`、`force_relay`、`app_data` 占位符、可选 `streamer_data`，以及由 `ConnectOptions.client_type` 映射的 `controller_platform`（`1` Windows、`2` Android、`3` iOS、`4` macOS）；二进制帧原样转发 `ConnectOptions`。 |
| Host ↔ Viewer | `soac`、`forward_setting` | 转发 SDP Offer/Answer、ICE 候选与会话设置。被控端通过顶层 `client_id` 指定目标控制端；控制端通过 `data.ice_id` 指定目标会话。服务端校验会话归属后，将 JSON 中的 `client_id` 改写为发送方的真实连接 ID 再投递给对端。 |
| Cloud → Host | `bmsg_push`（`wake_on_lan`） | `{"type":"wake_on_lan","data":{"target_device":{"mac":"aa:bb:cc:dd:ee:ff","ip":"192.168.1.20","subnet_mask":"255.255.255.0"}}}`，发给 `POST /api/web/devices/{id}/wake` 选中的协助唤醒被控端，由其在所在局域网内发送魔术包。 |
| Cloud → Host | `bmsg_push`（远程协助挑战） | 包括 `get_control_mode`（`{"control_id"}`）、`remote_control`（`{"control_id","salt"}`）、`remote_control_by_confirmation`（`{"control_id","device_name","user_id"}`）与 `remote_control_by_confirmation_cancel`（`{"control_id"}`），用于协调远程协助校验与桌面确认。 |
| Cloud → Host | `released` | 控制端断开连接或被其他控制端通过 `force_join` 抢占时发送：`{"ice_id": "...", "client_id": "<viewer_id>"}`。 |
| Cloud → Host | `left`（无 `ice_id`） | 被控端被新连接顶替或设备被解绑时发送 `{}`，随后服务端关闭该 WebSocket。 |
| Cloud → Viewer | `left`（带 `ice_id`） | 被控端断线超过 30 秒宽限期、被踢出或当前控制端被 `force_join` 抢占时发送：`{"ice_id": "...", "client_id": "<host_id>"}`。 |

## 6. TURN 临时凭据派生（`crates/remora-cloud/src/relay/ice.rs`）

当 `ice.servers` 包含 TURN 地址且配置了 `ice.turn_secret_file` 时，Remora 按 `coturn` 的 `use-auth-secret`（TURN REST API）规范为每次 `control` 会话派生短期凭据，并写入返回给控制端与被控端的 `iceServers` 列表。凭据自签发时刻起有效期为 24 小时（`CREDENTIAL_LIFETIME = 86400` 秒）：`username` 格式为 `<unix_expiry_timestamp>:<user_id>`（如 `1700086400:1`），`credential` 为 `base64(HMAC-SHA1(turn_secret, username))`。配置中的 STUN 地址（`stun:` / `stuns:`）与 TURN 地址（`turn:` / `turns:`）会拆分为独立的 `iceServers` 条目，仅在 TURN 条目上附带 `username` 与 `credential`。

## 7. WebRTC 会话层与数据通道（`crates/remora-proto/src/session/`）

信令握手与 ICE 连通后，端点间直接通过 WebRTC 传输媒体轨道与五个 SCTP 数据通道，其线协议类型定义在 [`crates/remora-proto/proto/gv_pb.proto`](../../crates/remora-proto/proto/gv_pb.proto) 与 `crates/remora-proto/src/session/`：

- **数据通道**：`CONTROL_DATA_CHANNEL`（`gv_pb.Message` 控制信封与 JSON 输入事件）、`TEXT_DATA_CHANNEL`（剪贴板与系统 RPC）、`FILE_DATA_CHANNEL`（文件传输 RPC 与端口映射帧）、`BINARY_DATA_CHANNEL`（终端消息）及 `STREAMER_DATA_CHANNEL`。
- **控制握手与输入**：`CONTROL_DATA_CHANNEL` 打开后双方通过 `gv_pb.Message` 交换 `EchoRequest`/`EchoResponse`（携带 `FeatureFlag` 版本协商）、`SimpleAction`、`CaptureConfig`、`CodecNegotiation` 与 `RpcRequest`/`RpcResponse`；控制端以 JSON 格式发送键盘、鼠标与文本输入事件。
- **文件传输与端口映射**：文件 RPC 请求可经控制、文本或文件通道发送；应答（含 `RemoveDir` 的结果）统一走 `FILE_DATA_CHANNEL`。端口映射也走文件通道，使用封装在 `gv_pb.Message` 中的 `PortMappingFrame`。
- **终端与系统**：二进制 `TERM` 帧和 protobuf `TerminalSessionChanged` 通知走 `BINARY_DATA_CHANNEL`。控制、文本与二进制通道均可识别终端控制 RPC。控制与文本通道均可识别隐私屏、标注（`Draw`）和虚拟音频策略 RPC。隐私屏与标注应答走 `TEXT_DATA_CHANNEL`。`VirtualAudioDriverPolicy` 应答通道由发送方指定，无操作探测应答的 `error_code` 为 `0`。
- **剪贴板与音视频**：支持分片 `ClipboardChange`（每片最大 8192 字节）与 `ClipboardTextChangeRequest` RPC（最大 1 MiB）；音视频使用 H.264 / H.265 与 Opus，并附带 `video-capture-index`、`video-frame-is-new-frame`、`video-feedback-latency` 与 `video-frame-sending-delay` RTP 头扩展。
