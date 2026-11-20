# Architecture and Security

[简体中文](../zh/architecture.md) | [English](architecture.md) | [Overview](../../README.en.md)

## 1. Workspace Structure and Module Roles

The workspace consists of a protocol codec library, the cloud server binary, and a static single-page web application:

| Component | Path | License | Role |
|---|---|---|---|
| Protocol codecs | `crates/remora-proto` | MIT OR Apache-2.0 | In-memory codecs: REST request signing and `{code, msg, data}` envelope parsing (`cloud.rs`), Engine.IO v4 / Socket.IO v5 packet framing (`socketio.rs`), and `gv_pb` protobuf types compiled from `proto/gv_pb.proto` via `protox` and `prost-build`. |
| Cloud server | `crates/remora-cloud` | GPL-3.0-or-later | The `remora` binary: Axum HTTP/WebSocket server, SQLite persistence, Argon2id and OIDC authentication, GCRA rate limiting, signaling room router, TURN credential derivation, and compile-time asset embedding via `rust-embed`. |
| Web console | `web/` | MIT | SvelteKit static single-page app (`@sveltejs/adapter-static` with `app.html` fallback). Built files in `web/build` (or `$REMORA_WEB_DIR`) are embedded into the server binary. |

`remora-proto` performs only serialization and signature math with zero file or network I/O under `MIT OR Apache-2.0`, so external test tools and clients can reuse the codecs directly.

## 2. HTTP Routing and Security Headers

`remora_cloud::router` divides incoming requests into four route groups by authentication model:

1. **UU Client REST API (`/api/v1/*`)**: When `sign_key_file` is configured, `signing::require_signature` wraps the group, caps request bodies at `1 MiB`, and verifies the `x-param-sign` header. Authenticated handlers extract `User` from `Authorization: Bearer <User JWT>`.
2. **Web Console API (`/api/web/*`)**: Every response includes `Cache-Control: no-store`. Protected routes read the `remora_session` cookie, and state-changing methods (`POST`, `DELETE`) verify the `X-CSRF-Token` header against the session's CSRF token in constant time.
3. **Socket.IO Signaling (`/socket.io/`)**: Accepts `EIO=4&transport=websocket` upgrades. Before upgrading, the handler validates the Room JWT in `X-NRD-AUTH` and queries SQLite to confirm that `devices.owner` still matches the token's `sub`, rejecting tokens for devices that were unbound after the token was issued.
4. **Embedded Frontend Assets (`app::serve`)**: Responds to `GET` and `HEAD` requests, returning `404` for unmatched paths under `/api/` or `/socket.io/`. Hashed bundles under `/_app/immutable/*` carry `Cache-Control: public, max-age=31536000, immutable`, while `app.html` and other root assets carry `no-cache`. Every static response sets `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`, and a `Content-Security-Policy` (`frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'`) paired with SvelteKit's build-time script hashes.

## 3. Tokens, Sessions, and Login Flows

### JWT Tokens (`token.rs`)

Remora signs `HS256` JWTs with a 32-byte random key (`token_key`) stored in SQLite's `secrets` table. Because UU clients decode JWT payloads directly to read claims such as `csrf`, Remora issues standard JWTs and separates token roles with a mandatory `kind` claim:

| Token Type | Claims | Lifetime | Validation and Revocation |
|---|---|---|---|
| **User Token** | `kind: "user"`, `sub`, `sid`, `csrf`, `iat` | Long-lived | Every authenticated request checks that `(sid, user_id)` exists in SQLite's `sessions` table. Client logout, web revocation, or device unbinding deletes the row so subsequent calls immediately receive HTTP `422` (`code: 1120`). |
| **Room Token** | `kind: "room"`, `sub`, `device_id`, `role` (`host` or `viewer { from_device, take_over }`), `exp` | 3600 seconds (1 hour) | Expires by `exp`; WebSocket admission also verifies `devices.owner == sub` in SQLite. |

### Browser Sessions and Owner Password (`browser.rs` & `password.rs`)

Signing into the web console generates a 32-byte random hex token and stores it in the `remora_session` cookie (`HttpOnly; SameSite=Strict`, plus `Secure` when `public_url` starts with `https://`) for 30 days. SQLite's `browsers` table stores only the SHA-256 digest of that cookie value as the primary key alongside the user ID and CSRF token, keeping raw session tokens exclusively in the client browser. Owner password login verifies the submitted password against the Argon2id PHC string from `login.password_file` on Tokio's blocking thread pool (`tokio::task::spawn_blocking`) and authenticates built-in user `id = "1"` (`Owner`).

### OpenID Connect (`oidc.rs`)

The OIDC login flow uses the Authorization Code grant with PKCE, `state`, and `nonce`, saving the pending flow parameters in a 600-second `remora_oidc` cookie (`Path=/api/web/oidc; HttpOnly; SameSite=Lax`). Remora caches the provider's discovery document and JWKS for 1 hour, applies a 10-second timeout to upstream HTTP requests, and verifies TLS certificates against the operating system trust store via `rustls-platform-verifier`. On callback, Remora upserts the verified `(issuer, subject)` pair into the `users` table with numeric IDs starting at `"2"` and updates the user's display name.

### QR Login State Machine (`login.rs`)

Pending QR logins live in memory for 180 seconds. Calling `POST /api/v1/qrcode/gen/login` creates an entry with two independent random tokens: a page `id` embedded in `{public_url}/login/{id}` and a `status_query_ticket` returned to the client. Both tokens must match on status polls and token redemption. Loading `GET /api/web/logins/{id}` in a browser advances the state from `Waiting (1)` to `Scanned (2)`. Submitting `confirm` sets `Confirmed (4)` and wakes any waiting 25-second `qrcode/login/status` long-poll via a `tokio::sync::watch` channel; submitting `deny` removes the entry so the long-poll wakes and returns `QR_EXPIRED (1190)`. Calling `login/by_qrcode` consumes and deletes the confirmed entry on first use and issues a User Token. When `login.auto_confirm = true`, new entries start in `Confirmed (4)` bound to `Owner` (`id = "1"`).

## 4. Signaling Router and Room State Machine (`rooms.rs` & `signaling.rs`)

`Rooms` is an in-memory routing table keyed by `device_id`. Each room tracks the currently connected `host: Option<Peer>`, a `departed: Option<String>` host connection ID during a reconnect grace window, connected `viewers: HashMap<String, Peer>`, and active control `sessions: HashMap<String, Session>` keyed by `ice_id` (recording `viewer`, `from_device`, and `desktop: bool`).

Every WebSocket connection to `/socket.io/` receives a fresh 16-byte random hex `client_id` (which also serves as its Engine.IO `sid`). When a viewer sends `soac` or `forward_setting`, the server checks that `/data/ice_id` belongs to a session owned by that viewer connection before forwarding to the host; when relaying in either direction, the server overwrites `client_id` in the JSON payload with the sender's verified connection ID.

When a viewer sends a `control` event, `rooms.rs` decodes the attached binary `gv_pb.ConnectOptions`. Payloads with `capture_type <= 2` (`CT_UNKNOWN`, `CT_DESKTOP`, `CT_WINDOW`) or invalid protobuf bytes count as a desktop control session, while utility sessions (`capture_type > 2`, such as file transfer, terminal, or port forwarding) run alongside desktop sessions. Only one viewer device (`from_device`) may hold a desktop session in a room at a time: if another device already holds the desktop and `force_join` is `false`, REST `/room/join/by_device` returns error code `2002` and signaling `control` replies `["failure", {"reason": "controlled from another device"}]`. When `force_join` is `true`, the server terminates the existing desktop session by sending `released` to the host and `left` to the displaced viewer before allocating the new session.

Because WebRTC media and control channels flow directly between host and viewer, a brief signaling disconnect on the host preserves active sessions. If a host socket closes while `sessions` is non-empty, the room records its connection ID in `departed` and starts a 30-second timer (`ReconnectGrace`). If the host reconnects within 30 seconds, it resumes ownership of all existing sessions; if the timer expires first, the room sends `left` (with each session's `ice_id`) to the viewers and removes the room.

When a new host connection replaces an already connected host, or when a user unbinds a device or revokes a host login session, the server sends `left` with an empty payload `{}` (no `ice_id`) to the previous host and closes its WebSocket.

## 5. Client IP Extraction and GCRA Rate Limiting

### Client Address Extraction (`address.rs`)

The `ClientAddress` extractor resolves the caller's IP for rate limiting and login confirmation pages. On TCP connections whose peer IP is outside `trusted_proxies`, Remora uses the socket peer IP directly. When the TCP peer matches `trusted_proxies`, or when the connection arrives over a Unix domain socket, Remora scans `X-Forwarded-For` from right to left (accepting plain IPs, `IP:port`, `[IPv6]:port`, and IPv4-mapped IPv6 addresses) and picks the first address outside `trusted_proxies`. Scanning right-to-left stops at the boundary of the trusted proxy chain and ignores any client-supplied entries prepended on the left of `X-Forwarded-For`.

### GCRA Rate Limiters (`limit.rs`)

Unauthenticated endpoints and password checks enforce Generic Cell Rate Algorithm (GCRA) limits. The limiter keys IPv4 callers by single address and aggregates IPv6 callers by `/64` prefix so rotating addresses within one `/64` subnet shares the same quota. Each limiter tracks up to `MAX_TRACKED = 65_536` keys; when full, it evicts entries whose quota has fully refilled, and routes overflow while full to a shared fallback bucket to keep memory usage bounded.

| Operation | Key Dimension | Burst | Refill Rate |
|---|---|---|---|
| Owner password check | Per client IPv4 / IPv6 `/64` | 5 | 1 per 60 s |
| Owner password check | Global total | 30 | 1 per 10 s |
| QR login generation | Per client IPv4 / IPv6 `/64` | 10 | 1 per 10 s |
| Device registration | Per client IPv4 / IPv6 `/64` | 10 | 1 per 60 s |
| Wake-on-LAN request | Per client IPv4 / IPv6 `/64` | 5 | 1 per 30 s |

## 6. SQLite Storage and Log Redaction

`database.rs` opens SQLite with `journal_mode = WAL`, `synchronous = NORMAL`, a 5-second `busy_timeout`, and `STRICT` tables migrated sequentially via `PRAGMA user_version` (schema version `6`):

| Table | Columns | Purpose |
|---|---|---|
| `secrets` | `name TEXT PRIMARY KEY, value BLOB` | Persists the 32-byte JWT signing key `token_key`. |
| `devices` | `id TEXT PRIMARY KEY, client_id TEXT UNIQUE, name TEXT, platform INTEGER, owner TEXT` | Registers devices idempotently by installation `client_id`; sets `owner` on `room/create`. |
| `sessions` | `id TEXT PRIMARY KEY, user_id TEXT, device_id TEXT, created INTEGER` | Tracks active client User Token sessions; indexed by `device_id` for cascade cleanup on unbind. |
| `users` | `id TEXT PRIMARY KEY, issuer TEXT, subject TEXT, name TEXT` | Pre-seeded with `id = '1'` (`Owner`); OIDC users keyed by `UNIQUE (issuer, subject)`. |
| `browsers` | `id TEXT PRIMARY KEY, user_id TEXT, csrf TEXT, created INTEGER, expires INTEGER` | Tracks browser sessions, with the SHA-256 digest of `remora_session` as `id`. |
| `lan_reports` | `device_id TEXT PRIMARY KEY, mac TEXT, ip TEXT, subnet_mask TEXT` | Each device's last Wake-on-LAN report, kept while the device is offline; deleted on unbind. |
| `files` | `id TEXT PRIMARY KEY, user_id TEXT, device_id TEXT, mime TEXT, md5 TEXT, content BLOB` | Uploaded images. `id` is random and appears in the file's URL; the last 4 per user and device are kept, plus current wallpapers; a device's uploads are deleted on unbind. |
| `wallpapers` | `device_id TEXT PRIMARY KEY, file_id TEXT` | The image each device's card shows; deleted on unbind. |

Log statements record only entity identifiers (`user_id`, `device_id`), session counts, byte sizes, and booleans. Types holding secret material (`SignKey`, `TurnSecret`, `OwnerPassword`, `Database`, `LanReport`, `FileId`) implement `Debug` with fixed placeholders such as `SignKey(..)`, keeping JWTs, cookies, QR URLs and tickets, password hashes, TURN secrets and derived credentials, OIDC client secrets, and SDP `ufrag` / `pwd`, devices' MAC addresses, LAN addresses and netmasks, and upload tokens and file URLs out of logs and error traces.
