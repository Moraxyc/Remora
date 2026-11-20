# 线协议与接口参考

[简体中文](protocol.md) | [English](../en/protocol.md) | [返回总览](../../README.md)

## 1. 请求签名（`x-param-*`）

当服务端配置了 `sign_key_file` 时，所有 `/api/v1/*` 请求必须携带 `x-param-sign` 请求头通过 HMAC-SHA256 校验（单次请求体上限为 `1 MiB`）。签名与常时间验签实现位于 [`crates/remora-proto/src/cloud.rs`](../../crates/remora-proto/src/cloud.rs)。

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
| `400` | `1190` | `QR code expired`：二维码 ID 未知、已超过 180 秒有效期、已被拒绝或已兑换过。 |
| `429` | `1429` | `too many requests`：触发 GCRA 速率限制。 |
| `400` | `2001` | `device is offline`：目标被控端当前未连入信令房间。 |
| `400` | `2002` | `被控端正在被远控中，无法发起连接`：已有其他设备持有桌面会话且请求中 `force_join` 为 `false`。 |
| `400` | `2003` | `no such device`：设备不存在或归属于其他用户。 |
| `500` | `1000` | `internal error`：服务端内部或 SQLite 存储错误。 |

## 3. 客户端 REST 接口（`/api/v1/*`）

需要登录态的接口通过 `Authorization: Bearer <User JWT>` 传递用户令牌；客户端同时通过 `X-Param-Device-Id: <device_id>` 上报自身设备 ID。

### 设备接口

| 方法与路径 | 鉴权 | 请求体 | 响应 `data` 与行为 |
|---|---|---|---|
| `POST /api/v1/device/windows/init` | 匿名（限流） | `{"client_id": "...", "name": "..."}`（`name` 可选，其余硬件字段被忽略） | `{"device_id": "..."}`。按客户端安装标识 `client_id` 幂等注册：重复调用返回既有 `device_id` 并更新名称，平台记为 `1`（Windows）。 |
| `GET /api/v1/device/list` | User Token | 空 | `{"current_device": Entry \| null, "my_binded_devices": [Entry], "others_shared_devices": []}`。每个 `Entry` 包含 `device_id`、可选 `alias`、`platform` 与 `status`（`"CONNECTED"` 或 `"DISCONNECTED"`）。 |
| `POST /api/v1/device/{device_id}/unbind` | User Token | `{}` | `{}`。解除设备绑定，删除该被控端关联的登录会话与局域网信息，并向在线被控端发送踢出信令。 |
| `POST /api/v1/device/wake_on_lan/info` | User Token + `X-Param-Device-Id` | `{"mac": "aa:bb:cc:dd:ee:ff", "inner_ip": "192.168.1.20", "ip": "192.168.1.20", "subnet_mask": "255.255.255.0"}` | `{"turn_on_wol": true}`。保存调用方已绑定设备最近一次上报的局域网信息，用于网络唤醒。`inner_ip` 为局域网地址且必填；字段缺失或无效（MAC 须为以 `:` 或 `-` 分隔的 6 个十六进制字节，地址须为 IPv4，子网掩码须连续）时返回 HTTP `422`、code `1001`。 |

### 扫码登录接口

| 方法与路径 | 鉴权 | 请求体 | 响应 `data` 与行为 |
|---|---|---|---|
| `POST /api/v1/qrcode/gen/login` | 匿名（限流） | `{}` | `{"qrcode_jump_url": "{public_url}/login/{id}", "status_query_ticket": "..."}`。有效期 180 秒；启用 `auto_confirm` 时生成即处于 `Confirmed (4)` 状态。 |
| `POST /api/v1/qrcode/login/status` | 匿名 | `{"token": "<ticket>", "qrcode_jump_url": "...", "login_status": 1}` | 当状态仍等于 `login_status` 时最长挂起 25 秒，状态变化时立即返回 `{"status": 1\|2\|3\|4}`（`1` Waiting、`2` Scanned、`3` Expired、`4` Confirmed）。 |
| `POST /api/v1/login/by_qrcode` | 匿名 | `{"token": "<ticket>", "qrcode_jump_url": "..."}` | `{"token": "<User JWT>", "user_id": "...", "nickname": "..."}`。单次有效，调用后立即移除内存中的二维码条目。 |

### 用户与房间接口

| 方法与路径 | 鉴权 | 请求体 | 响应 `data` 与行为 |
|---|---|---|---|
| `GET /api/v1/user/info` | User Token | 空 | `{"user_id": "...", "nickname": "..."}`。 |
| `POST /api/v1/user/logout` | User Token | `{}` | `{}`。从 SQLite `sessions` 表删除当前令牌的 `sid`。 |
| `POST /api/v1/room/create` | User Token + `X-Param-Device-Id` | `{}` | 将该设备绑定至当前用户并关联当前会话，返回 `{"signaling_server": "wss://...", "signaling_list": [...], "token": "<Host Room JWT>", "ws_connect_timeout_ms": 10000}`。 |
| `POST /api/v1/room/join/by_device/{device_id}` | User Token（可选 `X-Param-Device-Id`） | `{"force_join": false}` | 校验设备归属、被控端在线状态与桌面占用冲突，返回 `{"signaling_list": [...], "token": "<Viewer Room JWT>", "ws_connect_timeout_ms": 10000, "report_token": "", "report_url": "", "publisher_platform": <u8>}`。 |

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
| `GET /api/web/devices` | Cookie | 列出当前用户绑定的设备、在线状态及 `wakeable`（设备是否上报过局域网信息；不返回任何地址）。 |
| `DELETE /api/web/devices/{id}` | Cookie + CSRF | 解绑指定设备、删除其被控端登录会话并踢出在线被控端。 |
| `POST /api/web/devices/{id}/wake` | Cookie + CSRF（限流） | 唤醒离线设备：在当前用户的在线设备中选一台上报的 IPv4 网段（地址与掩码）与目标相同的设备，向其推送 `wake_on_lan`，推送后返回 `204`。错误：`404 not_found`、`409 device_online`、`409 no_lan_report`、`409 no_wake_helper`、`429 too_many_requests`。 |
| `GET /api/web/client-sessions` | Cookie | 列出当前用户所有活跃的客户端登录会话。 |
| `DELETE /api/web/client-sessions/{id}` | Cookie + CSRF | 吊销指定客户端登录会话（若为在线被控端会话，同步踢出信令房间）。 |
| `GET /api/web/browsers` | Cookie | 列出当前用户所有活跃的浏览器登录会话。 |
| `DELETE /api/web/browsers/{id}` | Cookie + CSRF | 吊销指定浏览器登录会话。 |

## 5. Socket.IO / Engine.IO 信令协议

### 握手、心跳与二进制帧（`crates/remora-proto/src/socketio.rs`）

客户端通过 `GET /socket.io/?EIO=4&transport=websocket` 发起连接，并在 `X-NRD-AUTH` 请求头中携带 Room JWT（令牌无效、过期或设备已解绑时返回 HTTP `401`）。WebSocket 升级完成后，服务端立即下发 Engine.IO Open 文本帧 `0{"sid":"...","upgrades":[],"pingInterval":25000,"pingTimeout":20000,"maxPayload":1000000}`。此后服务端每 25 秒发送文本帧 `"2"`（Ping），客户端需在 20 秒内回复文本帧 `"3"`（Pong）。

Engine.IO 建立后，客户端发送 `"40"` 加入 Socket.IO 默认命名空间，服务端回复 `40{"sid":"<client_id>"}`（每个连接独立生成的 16 字节随机十六进制连接 ID）。带二进制附件的事件采用 Socket.IO v5 帧格式：先发送一条文本帧 `45<N>-[<ack_id>]["<event>", ...]`，JSON 中以 `{"_placeholder": true, "num": 0}` 指代附件位置，随后紧跟 `<N>` 条 WebSocket Binary 帧。

### 信令事件

| 方向 | 事件名 | 载荷与处理行为 |
|---|---|---|
| Host → Cloud | `room_info`（带 Ack） | 被控端就绪通告。服务端回复 Ack `{}`，并立即向该被控端推送 `bmsg_push`：`{"data":{"status":"online","publisher_availability_status":"ready"}}`，房间随即进入在线可连接状态。 |
| Host / Viewer → Cloud | `refresh_reconnect_key`（带 Ack） | 服务端回复 Ack `["success", {"reconnect_key": "<32位十六进制>"}]`。 |
| Viewer → Cloud | `control`（带 Ack，含 1 个二进制附件） | 请求建立控制会话。JSON 参数包含 `app_control_id`、可选 `streamer_data` 以及指向序列化 `gv_pb.ConnectOptions` 附件的 `app_data` 占位符。允许接入时，服务端分配 `ice_id` 与临时 TURN 凭据，向被控端发送 `be-controlled`，并向控制端回复 Ack `["success", {"ice_id", "client_id": <host_id>, "app_control_id", "iceServers", "force_relay"}]`；拒绝时回复 `["failure", {"reason": "host offline" \| "controlled from another device"}]`。 |
| Cloud → Host | `be-controlled`（含 1 个二进制附件） | JSON 参数包含 `ice_id`、`client_id`（控制端连接 ID）、`app_control_id`、`iceServers`、`force_relay`、`app_data` 占位符、可选 `streamer_data`，以及由 `ConnectOptions.client_type` 映射的 `controller_platform`（`1` Windows、`2` Android、`3` iOS、`4` macOS）；二进制帧原样转发 `ConnectOptions`。 |
| Host ↔ Viewer | `soac`、`forward_setting` | 转发 SDP Offer/Answer、ICE 候选与会话设置。被控端通过顶层 `client_id` 指定目标控制端；控制端通过 `data.ice_id` 指定目标会话。服务端校验会话归属后，将 JSON 中的 `client_id` 改写为发送方的真实连接 ID 再投递给对端。 |
| Cloud → Host | `bmsg_push`（`wake_on_lan`） | `{"type":"wake_on_lan","data":{"target_device":{"mac":"aa:bb:cc:dd:ee:ff","ip":"192.168.1.20","subnet_mask":"255.255.255.0"}}}`，发给 `POST /api/web/devices/{id}/wake` 选中的协助唤醒被控端，由其在所在局域网内发送魔术包。 |
| Cloud → Host | `released` | 控制端断开连接或被其他控制端通过 `force_join` 抢占时发送：`{"ice_id": "...", "client_id": "<viewer_id>"}`。 |
| Cloud → Host | `left`（无 `ice_id`） | 被控端被新连接顶替或设备被解绑时发送 `{}`，随后服务端关闭该 WebSocket。 |
| Cloud → Viewer | `left`（带 `ice_id`） | 被控端断线超过 30 秒宽限期、被踢出或当前控制端被 `force_join` 抢占时发送：`{"ice_id": "...", "client_id": "<host_id>"}`。 |

## 6. TURN 临时凭据派生（`crates/remora-cloud/src/ice.rs`）

当 `ice.servers` 包含 TURN 地址且配置了 `ice.turn_secret_file` 时，Remora 按 `coturn` 的 `use-auth-secret`（TURN REST API）规范为每次 `control` 会话派生短期凭据，并写入返回给控制端与被控端的 `iceServers` 列表。凭据自签发时刻起有效期为 24 小时（`CREDENTIAL_LIFETIME = 86400` 秒）：`username` 格式为 `<unix_expiry_timestamp>:<user_id>`（如 `1700086400:1`），`credential` 为 `base64(HMAC-SHA1(turn_secret, username))`。配置中的 STUN 地址（`stun:` / `stuns:`）与 TURN 地址（`turn:` / `turns:`）会拆分为独立的 `iceServers` 条目，仅在 TURN 条目上附带 `username` 与 `credential`。
