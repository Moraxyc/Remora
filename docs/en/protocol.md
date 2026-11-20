# Wire Protocol and API Reference

[简体中文](../zh/protocol.md) | [English](protocol.md) | [Overview](../../README.en.md)

## 1. Request Signing (`x-param-*`)

When `sign_key_file` is configured, every `/api/v1/*` endpoint requires a valid `x-param-sign` HMAC-SHA256 signature header (with request bodies capped at `1 MiB`). The signing and constant-time verification implementation lives in [`crates/remora-proto/src/cloud.rs`](../../crates/remora-proto/src/cloud.rs).

Compute the signature in four steps:

1. Collect all HTTP request headers whose names start with `x-param-` (case-insensitive), excluding `x-param-sign` itself.
2. Convert those header names to lowercase, sort them in ascending lexicographical byte order, format each pair as `name=value`, and join them with `&`.
3. Concatenate the uppercase HTTP method (such as `GET` or `POST`), the raw request path and query string (such as `/api/v1/device/list`), the joined `x-param-*` string from step 2, and the raw request body bytes (an empty body `""` and `"{}"` produce different digests), then compute HMAC-SHA256 over the combined byte stream.
4. Encode the 32-byte MAC as a 64-character lowercase hexadecimal string and send it in `x-param-sign`.

## 2. Response Envelope and Error Codes (`/api/v1/*`)

Every `/api/v1/*` handler returns a JSON envelope:

```json
{
  "code": 0,
  "msg": "ok",
  "data": {}
}
```

Clients treat HTTP `200` with `code == 0` as success. Errors return the following status and application codes:

| HTTP Status | `code` | `msg` and Condition |
|---|---|---|
| `422` | `1001` | `invalid request`: Malformed JSON, missing required fields, or body larger than 1 MiB. |
| `403` | `1002` | `invalid signature`: Missing `x-param-sign` header or HMAC mismatch. |
| `422` | `1120` | `login expired`: Missing, malformed, or revoked User Token; clients clear stored credentials and prompt for login. |
| `400` | `1190` | `QR code expired`: Unknown, expired (180 s), denied, or already redeemed QR login. |
| `429` | `1429` | `too many requests`: GCRA rate limit exceeded. |
| `400` | `2001` | `device is offline`: Target host is not connected to a signaling room. |
| `400` | `2002` | `被控端正在被远控中，无法发起连接`: Another viewer device holds the desktop session and `force_join` is `false`. |
| `400` | `2003` | `no such device`: Device ID does not exist or belongs to another user. |
| `500` | `1000` | `internal error`: Internal server or SQLite failure. |

## 3. UU Client REST API (`/api/v1/*`)

Authenticated endpoints read the user token from `Authorization: Bearer <User JWT>`, and clients report their local installation ID in `X-Param-Device-Id: <device_id>`.

### Devices

| Endpoint | Auth | Request Body | Response `data` and Behavior |
|---|---|---|---|
| `POST /api/v1/device/windows/init` | None (rate-limited) | `{"client_id": "...", "name": "..."}` (`name` optional; hardware fields ignored) | `{"device_id": "..."}`. Idempotent by `client_id`: re-registering returns the existing `device_id`, updates `name`, and sets platform `1` (Windows). |
| `GET /api/v1/device/list` | User Token | Empty | `{"current_device": Entry \| null, "my_binded_devices": [Entry], "others_shared_devices": []}`. Each `Entry` has `device_id`, optional `alias`, `platform`, `status` (`"CONNECTED"` or `"DISCONNECTED"`), and optional `wallpaper_url`. |
| `POST /api/v1/device/{device_id}/unbind` | User Token | `{}` | `{}`. Unbinds the device, deletes its host login sessions, LAN report, wallpaper and uploaded files, and kicks any connected host. |
| `POST /api/v1/device/wake_on_lan/info` | User Token + `X-Param-Device-Id` | `{"mac": "aa:bb:cc:dd:ee:ff", "inner_ip": "192.168.1.20", "ip": "192.168.1.20", "subnet_mask": "255.255.255.0"}` | `{"turn_on_wol": true}`. Stores the calling bound device's last LAN report for Wake-on-LAN. `inner_ip` is the LAN address and is required; a missing or invalid field (MAC of six hex bytes separated by `:` or `-`, IPv4 addresses, contiguous netmask) answers HTTP `422` with code `1001`. |

### Files and Wallpapers

Hosts publish an image of their desktop wallpaper for device cards. Images are stored in SQLite and served without credentials to anyone holding their URL; each URL carries a random 128-bit id and never changes content.

| Endpoint | Auth | Request Body | Response and Behavior |
|---|---|---|---|
| `GET /api/v1/tool/fp/token?filetype=1` | User Token | Empty | `{"token": "...", "req_url": "{public_url}/api/files", "ttl": 3600, "max_size": 10485760}`. Clients fetch a new token once `ttl` seconds have passed; until then they reuse it for any number of uploads. The token works for 10 minutes past `ttl`, so that uploads before the next fetch succeed, and only while its login session stays open. `filetype` is not checked. |
| `POST /api/files` | `Authorization: <upload token>` (bare, or after `Bearer `) | Raw image bytes, or `multipart/form-data` whose first file part is the image | Not a UU envelope: HTTP `200` with `{"url": "{public_url}/api/files/{id}", "fsize": 123, "md5": "<hex>", "mime": "image/png"}`. PNG, JPEG, WebP, GIF and BMP are accepted by their content, whatever `Content-Type` says (or without one), up to `max_size` bytes. Errors are `{"error": ...}` with `401 unauthorized`, `400 empty`, `413 too_large`, `415 not_an_image`. The last 4 uploads of each user and device are kept, besides current wallpapers. |
| `GET /api/files/{id}` | None (URL `id` capability) | — | The image, with its type, `Cache-Control: public, max-age=31536000, immutable`, `nosniff` and a sandboxing CSP. Query strings, such as resizing parameters, are ignored. |
| `POST /api/v1/device/wallpaper` | User Token + `X-Param-Device-Id` | `{"wallpaper_url": "<url from upload>"}` | `{}`. Sets the calling bound device's wallpaper to one of the user's uploaded files, matched by the id in its path whatever origin it names; `""` clears it. A URL of no such file answers `422` / `1001`; an unbound caller answers `400` / `2003`. |

### QR Login

| Endpoint | Auth | Request Body | Response `data` and Behavior |
|---|---|---|---|
| `POST /api/v1/qrcode/gen/login` | None (rate-limited) | `{}` | `{"qrcode_jump_url": "{public_url}/login/{id}", "status_query_ticket": "..."}`. Valid for 180 seconds; starts in `Confirmed (4)` when `auto_confirm = true`. |
| `POST /api/v1/qrcode/login/status` | None | `{"token": "<ticket>", "qrcode_jump_url": "...", "login_status": 1}` | Holds the request for up to 25 seconds while status equals `login_status`, returning immediately on change with `{"status": 1\|2\|3\|4}` (`1` Waiting, `2` Scanned, `3` Expired, `4` Confirmed). |
| `POST /api/v1/login/by_qrcode` | None | `{"token": "<ticket>", "qrcode_jump_url": "..."}` | `{"token": "<User JWT>", "user_id": "...", "nickname": "..."}`. Single-use redemption that removes the in-memory QR entry. |

### Users and Rooms

| Endpoint | Auth | Request Body | Response `data` and Behavior |
|---|---|---|---|
| `GET /api/v1/user/info` | User Token | Empty | `{"user_id": "...", "nickname": "..."}`. |
| `POST /api/v1/user/logout` | User Token | `{}` | `{}`. Deletes the token's `sid` row from SQLite `sessions`. |
| `POST /api/v1/room/create` | User Token + `X-Param-Device-Id` | `{}` | Binds the device to the calling user and links the session, returning `{"signaling_server": "wss://...", "signaling_list": [...], "token": "<Host Room JWT>", "ws_connect_timeout_ms": 10000}`. |
| `POST /api/v1/room/join/by_device/{device_id}` | User Token (optional `X-Param-Device-Id`) | `{"force_join": false}` | Verifies device ownership, host online state, and desktop exclusivity, returning `{"signaling_list": [...], "token": "<Viewer Room JWT>", "ws_connect_timeout_ms": 10000, "report_token": "", "report_url": "", "publisher_platform": <u8>}`. |

## 4. Web Console API (`/api/web/*`)

`/api/web/*` endpoints return plain JSON objects, report errors as `{"error": "<code>"}`, and set `Cache-Control: no-store`. Authenticated routes read the `remora_session` cookie, and `POST` / `DELETE` handlers also require a matching `X-CSRF-Token` header.

| Endpoint | Auth | Description |
|---|---|---|
| `GET /api/web/session` | None | Returns the signed-in user, CSRF token, and enabled authentication capabilities (`password`, `oidc`, `secure`). |
| `POST /api/web/session` | None (rate-limited) | Verifies `{"password": "..."}` against the Owner password hash, sets `remora_session`, and returns `204`. |
| `DELETE /api/web/session` | Cookie + CSRF | Deletes the current browser session and clears the cookie. |
| `GET /api/web/oidc/start?next=<path>` | None | Generates PKCE / `state` / `nonce`, sets the `remora_oidc` cookie, and redirects to the OIDC provider. |
| `GET /api/web/oidc/callback` | None | Exchanges the authorization code, verifies the ID token, upserts the user, and sets `remora_session`. |
| `GET /api/web/logins/{id}` | None (URL `id` capability) | Marks the QR login `Scanned` and returns device name, platform, client IP, creation timestamp, and confirmation state. |
| `POST /api/web/logins/{id}` | Cookie + CSRF | Submits `{"decision": "confirm" \| "deny"}` for the pending QR login. |
| `GET /api/web/devices` | Cookie | Lists devices bound to the signed-in user with online status, `wakeable` (whether the device reported its LAN; addresses are never returned), and `wallpaper` (the path of its wallpaper image, or `null`). |
| `DELETE /api/web/devices/{id}` | Cookie + CSRF | Unbinds the device, deletes its host login sessions, and kicks any online host. |
| `POST /api/web/devices/{id}/wake` | Cookie + CSRF (rate-limited) | Wakes an offline device: pushes `wake_on_lan` to one of the user's online devices whose reported IPv4 network (address and netmask) equals the target's, and returns `204` once sent. Errors: `404 not_found`, `409 device_online`, `409 no_lan_report`, `409 no_wake_helper`, `429 too_many_requests`. |
| `GET /api/web/client-sessions` | Cookie | Lists active client User Token sessions for the signed-in user. |
| `DELETE /api/web/client-sessions/{id}` | Cookie + CSRF | Revokes a client session (and kicks the host if the session belongs to an online host). |
| `GET /api/web/browsers` | Cookie | Lists active browser sessions for the signed-in user. |
| `DELETE /api/web/browsers/{id}` | Cookie + CSRF | Revokes a browser session. |

## 5. Socket.IO / Engine.IO Signaling

### Handshake, Heartbeat, and Binary Framing (`crates/remora-proto/src/socketio.rs`)

Clients open a WebSocket to `GET /socket.io/?EIO=4&transport=websocket` with `X-NRD-AUTH: <Room JWT>` (invalid, expired, or unbound device tokens return HTTP `401`). Upon upgrade, the server sends the Engine.IO Open text frame `0{"sid":"...","upgrades":[],"pingInterval":25000,"pingTimeout":20000,"maxPayload":1000000}`. The server then sends a text frame `"2"` (Ping) every 25 seconds and expects `"3"` (Pong) within 20 seconds.

After receiving the Open frame, the client sends `"40"` to connect to the default Socket.IO namespace, and the server replies `40{"sid":"<client_id>"}` with a fresh 16-byte random hex connection ID. Events carrying binary payloads use Socket.IO v5 framing: a text frame `45<N>-[<ack_id>]["<event>", ...]` references each attachment in JSON via `{"_placeholder": true, "num": 0}` and is followed immediately by `<N>` WebSocket binary frames.

### Signaling Events

| Direction | Event | Payload and Behavior |
|---|---|---|
| Host → Cloud | `room_info` (Ack) | Host readiness notification. The server acks `{}` and immediately sends `bmsg_push`: `{"data":{"status":"online","publisher_availability_status":"ready"}}`. The host now counts as online. |
| Host / Viewer → Cloud | `refresh_reconnect_key` (Ack) | Server acks `["success", {"reconnect_key": "<32-char-hex>"}]`. |
| Viewer → Cloud | `control` (Ack + 1 binary frame) | Requests a control session. The JSON payload carries `app_control_id`, optional `streamer_data`, and an `app_data` placeholder pointing to serialized `gv_pb.ConnectOptions`. When granted, the server allocates an `ice_id` and 24-hour TURN credentials, emits `be-controlled` to the host, and acks `["success", {"ice_id", "client_id": <host_id>, "app_control_id", "iceServers", "force_relay"}]`; when refused, it acks `["failure", {"reason": "host offline" \| "controlled from another device"}]`. |
| Cloud → Host | `be-controlled` (1 binary frame) | The JSON payload carries `ice_id`, `client_id` (viewer's connection ID), `app_control_id`, `iceServers`, `force_relay`, `app_data` placeholder, optional `streamer_data`, and `controller_platform` mapped from `ConnectOptions.client_type` (`1` Windows, `2` Android, `3` iOS, `4` macOS). The binary frame forwards the raw `ConnectOptions` bytes. |
| Host ↔ Viewer | `soac`, `forward_setting` | Relays SDP offers/answers, ICE candidates, and session settings. The host addresses a viewer via top-level `client_id`; the viewer addresses the host via `data.ice_id`. After verifying session ownership, the server overwrites `client_id` in the JSON payload with the sender's verified connection ID before forwarding. |
| Cloud → Host | `bmsg_push` (`wake_on_lan`) | `{"type":"wake_on_lan","data":{"target_device":{"mac":"aa:bb:cc:dd:ee:ff","ip":"192.168.1.20","subnet_mask":"255.255.255.0"}}}`, sent to the helper host chosen by `POST /api/web/devices/{id}/wake`. The host sends a magic packet on its LAN. |
| Cloud → Host | `released` | Sent when a viewer disconnects or is displaced by `force_join`: `{"ice_id": "...", "client_id": "<viewer_id>"}`. |
| Cloud → Host | `left` (without `ice_id`) | Sent as `{}` before closing the WebSocket when a host is replaced by a newer connection or unbound. |
| Cloud → Viewer | `left` (with `ice_id`) | Sent when a host fails to reconnect within the 30-second grace period, is kicked, or the viewer is displaced by `force_join`: `{"ice_id": "...", "client_id": "<host_id>"}`. |

## 6. TURN Credential Derivation (`crates/remora-cloud/src/ice.rs`)

When `ice.servers` includes TURN URLs and `ice.turn_secret_file` is set, Remora derives short-lived credentials following `coturn`'s `use-auth-secret` (TURN REST API) scheme and includes them in the `iceServers` array of both the `control` acknowledgement and `be-controlled`. Each credential is valid for 24 hours from issuance (`CREDENTIAL_LIFETIME = 86400` seconds): `username` is `<unix_expiry_timestamp>:<user_id>` (such as `1700086400:1`), and `credential` is `base64(HMAC-SHA1(turn_secret, username))`. STUN URLs (`stun:` / `stuns:`) and TURN URLs (`turn:` / `turns:`) are emitted as separate `iceServers` objects so credentials attach only to the TURN entries.
