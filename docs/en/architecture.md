# Architecture and Security

[简体中文](../zh/architecture.md) | [English](architecture.md) | [Overview](../../README.en.md)

## 1. Workspace Structure and Module Roles

The Remora workspace comprises four Rust crates, the embedded web app, and the conformance suite:

```text
remora-proto  (MIT OR Apache-2.0 · sans-IO · shared by all roles)
  ▲         ▲
  │         └── remora-cloud      (GPL-3.0-or-later · cloud roles)
  │
  └── remora-endpoint (MIT OR Apache-2.0 · endpoint I/O + str0m + Platform trait)
        ▲
        └── remora-lab            (MIT OR Apache-2.0 · conformance lab)
```

| Component | Path | License | Role |
|---|---|---|---|
| Sans-IO protocol crate | `crates/remora-proto` | MIT OR Apache-2.0 | **Strictly sans-IO**: no async runtime, sockets, system clock, or OS RNG I/O. Owns REST signing and DTOs (`control`), Engine.IO v4 / Socket.IO v5 framing and signal events (`signal`), `gv_pb` protobuf types compiled from `proto/gv_pb.proto` via `protox` and `prost-build` (`pb`), session channel codecs (`session`: `control`, `input`, `media`, `clipboard`, etc.), the `Machine` trait and pure state machines (`machine`: `auth`, `signaling`, `peer`), compile-time validated and JSON-embedded client profiles (`profile`: `macos-4.48.1`, `macos-4.48.2`, `windows-4.48.2`), stateless server helpers (`server`), and `ErrorCode` (`error`). |
| Cloud server binary | `crates/remora-cloud` | GPL-3.0-or-later | The `remora` binary, organized into `api`, `gateway`, `room`, `relay`, `store`, and `bus`. Runs Axum HTTP/WebSocket services, SQLite persistence, Argon2id and OIDC auth, GCRA rate limiting, per-room actor signaling routing, TURN credential derivation, and compile-time web asset embedding via `rust-embed`. Never depends on `remora-endpoint` or `remora-lab`. |
| Endpoint SDK | `crates/remora-endpoint` | MIT OR Apache-2.0 | High-level endpoint SDK: control-plane HTTP client and Socket.IO Tokio driver (`client`), `str0m` WebRTC transport and TURN/STUN client (`peer`), video/audio/input/clipboard abstraction traits (`platform`), `HostSession` and `ViewerSession` (`role`), and JSONL wire trace recording (`record`). Never depends on `remora-cloud`. |
| Conformance lab | `crates/remora-lab` | MIT OR Apache-2.0 | The `remora-lab` binary: headless `Platform` implementations (`headless`), YAML scenario runner (`scenario`), target orchestration for standalone/split/external/FakeCloud and embedded test TURN (`target`), cloud and session compatibility matrix runners (`matrix`), trace normalization and diffing (`normalize`, `diff`), secret-leak redaction checks (`redact`), and the JSON-RPC 2.0 external driver protocol (`driver`). Interacts with `remora-cloud` only as an external binary or URL, never linking it. |
| Web console | `web/` | MIT | SvelteKit static single-page app (`@sveltejs/adapter-static` with `app.html` fallback). Built files in `web/build` (or `$REMORA_WEB_DIR`) are embedded into the `remora` binary. |
| Compatibility suite & tools | `compat/`, `tools/protogen/` | MIT | Cloud and session YAML scenarios (`compat/scenarios/`), wire test vectors (`compat/vectors/`), trace JSON schemas (`compat/schema/`), external driver spec (`compat/driver-protocol.md`), and Mach-O schema extraction helpers (`tools/protogen/`). |

Dependency boundaries and license policies are enforced by `deny.toml`, `crates/remora-proto/deny.toml`, `crates/remora-endpoint/deny.toml`, and `crates/remora-lab/deny.toml` (banning `webpki-roots` everywhere, banning I/O/runtime crates from `remora-proto`, and banning `remora-cloud` from depending on `remora-endpoint` or `remora-lab`).

## 2. HTTP Routing, Role Isolation, and Security Headers

`remora_cloud::app::build` mounts routes and background tasks according to the enabled `CloudRole`s (`api`, `gateway`, `room`, `relay`):

1. **UU Client REST API (`/api/v1/*`, requires `api` role)**: Applies `api::middleware::profile::detect_profile` to identify embedded client profiles from `x-param-plat`, `x-param-vn`, and `x-param-vc`. When `sign_key_file` is configured, `api::middleware::signing::require_signature` wraps the group, caps request bodies at `1 MiB`, and verifies `x-param-sign`. Authenticated handlers extract `User` from `Authorization: Bearer <User JWT>`.
2. **Web Console API (`/api/web/*`, requires `api` role)**: Every response includes `Cache-Control: no-store`. Protected routes read the `remora_session` cookie, and state-changing methods (`POST`, `DELETE`) verify the `X-CSRF-Token` header against the session's CSRF token in constant time.
3. **File Upload and Serving (`/api/files*`, requires `api` role)**: `POST /api/files` authenticates with an upload token (`Authorization`) instead of `x-param-sign` and allows bodies up to `10 MiB + 64 KiB` (covering `multipart/form-data` overhead). `GET /api/files/{id}` serves images without credentials by random 128-bit `id`, sending `Cache-Control: public, max-age=31536000, immutable`, `nosniff`, and a sandbox CSP (`default-src 'none'; sandbox`).
4. **Socket.IO Signaling (`/socket.io/`, requires `gateway` role)**: Accepts `EIO=4&transport=websocket` upgrades. Before upgrading, the handler validates the Room JWT in `X-NRD-AUTH`, detects any matching client profile from headers, and queries SQLite to confirm that `devices.owner` still matches the token's `sub`, rejecting tokens for devices that were unbound after issuance.
5. **Embedded Frontend Assets (`api::web::serve_app`, requires `api` role)**: Responds to `GET` and `HEAD` requests, returning `404` for unmatched paths under `/api/` or `/socket.io/`. Hashed bundles under `/_app/immutable/*` carry `Cache-Control: public, max-age=31536000, immutable`, while `app.html` and other root assets carry `no-cache`. Every static response sets `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`, and a `Content-Security-Policy` (`frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'`) paired with SvelteKit's build-time script hashes.

## 3. Tokens, Sessions, and Login Flows

### JWT Tokens (`api/auth/token.rs`)

Remora signs `HS256` JWTs with a 32-byte random key (`token_key`) stored in SQLite's `secrets` table. Because UU clients decode JWT payloads directly to read claims such as `csrf`, Remora issues standard JWTs and separates token roles with a mandatory `kind` claim:

| Token Type | Claims | Lifetime | Validation and Revocation |
|---|---|---|---|
| **User Token** | `kind: "user"`, `sub`, `sid`, `csrf`, `iat` | Long-lived | Every authenticated request checks that `(sid, user_id)` exists in SQLite's `sessions` table. Client logout, web revocation, or device unbinding deletes the row so subsequent calls immediately receive HTTP `422` (`code: 1120`). |
| **Room Token** | `kind: "room"`, `sub`, `device_id`, `role` (`host` or `viewer { from_device, take_over }`), `exp` | 3600 seconds (1 hour) | Expires by `exp`; WebSocket admission also verifies `devices.owner == sub` in SQLite. |
| **Upload Token** | `kind: "upload"`, `sub`, `sid`, `device_id`, `exp` | Advertised TTL 3600 s + 600 s grace | Reusable until clients fetch a fresh token after `ttl`, provided the backing `sessions` row remains open. |

### Browser Sessions and Owner Password (`api/auth/browser.rs` & `api/auth/password.rs`)

Signing into the web console generates a 32-byte random hex token and stores it in the `remora_session` cookie (`HttpOnly; SameSite=Strict`, plus `Secure` when `public_url` starts with `https://`) for 30 days. SQLite's `browsers` table stores only the SHA-256 digest of that cookie value as the primary key alongside the user ID and CSRF token, keeping raw session tokens exclusively in the client browser. Owner password login verifies the submitted password against the Argon2id PHC string from `login.password_file` on Tokio's blocking thread pool (`tokio::task::spawn_blocking`) and authenticates built-in user `id = "1"` (`Owner`).

### OpenID Connect (`api/auth/oidc.rs`)

The OIDC login flow uses the Authorization Code grant with PKCE, `state`, and `nonce`, saving the pending flow parameters in a 600-second `remora_oidc` cookie (`Path=/api/web/oidc; HttpOnly; SameSite=Lax`). Remora caches the provider's discovery document and JWKS for 1 hour, applies a 10-second timeout to upstream HTTP requests, and verifies TLS certificates against the operating system trust store via `rustls-platform-verifier`. On callback, Remora upserts the verified `(issuer, subject)` pair into the `users` table with numeric IDs starting at `"2"` and updates the user's display name.

### QR Login State Machine (`api/auth/login.rs`)

Pending QR logins live in memory for 180 seconds. Calling `POST /api/v1/qrcode/gen/login` creates an entry with two independent random tokens: a page `id` embedded in `{public_url}/login/{id}` and a `status_query_ticket` returned to the client. Both tokens must match on status polls and token redemption. Loading `GET /api/web/logins/{id}` in a browser advances the state from `Waiting (1)` to `Scanned (2)`. Submitting `confirm` sets `Confirmed (4)` and wakes any waiting 25-second `qrcode/login/status` long-poll via a `tokio::sync::watch` channel; submitting `deny` removes the entry so the long-poll wakes and returns `QR_EXPIRED (1190)`. Calling `login/by_qrcode` consumes and deletes the confirmed entry on first use and issues a User Token. When `login.auto_confirm = true`, new entries start in `Confirmed (4)` bound to `Owner` (`id = "1"`).

## 4. Internal Bus, Signaling Gateway, and Room Actors (`bus/`, `gateway/`, `room/`, `relay/`)

The cloud server decouples API handling, WebSocket gateways, room state, and ICE relay credential minting into four roles communicating over a publish/subscribe `Bus`:

- **Bus Transports (`bus/local.rs` & `bus/unix.rs`)**: Single-process instances use the in-memory `LocalBus`; role-split multi-process deployments use `UnixBus` (one hub process binds `--bus-listen <PATH>` and worker processes join via `--bus-connect <PATH>`). Traffic flows over four topics:
  - `to_endpoint`: outbound frames, events, acks, or kick directives sent from rooms or API handlers to a gateway connection.
  - `room_command`: connection joins/leaves, inbound signaling events from gateways, and control-plane queries/kicks.
  - `presence`: online/offline connection transitions emitted by gateways.
  - `relay`: `RelayCommand::Grant` requests handled by the `relay` role to mint `iceServers` and `force_relay`.

- **Signaling Gateway (`gateway/mod.rs` & `gateway/presence.rs`)**: Handles `/socket.io/` WebSocket upgrades, Engine.IO heartbeats (25 s ping interval / 20 s timeout), the process-local `Connections` map (bounded 256-message outboxes), and fresh 16-byte random hex `client_id`s (which also serve as Engine.IO `sid`s). Inbound `Packet::Event` messages are forwarded to `room_command` as `RoomCommand::EndpointEvent`.

- **Per-Room Actor State Machine (`room/registry.rs`)**: `RoomRegistry` spawns an isolated `RoomActor` Tokio task per `device_id` (exiting once no host or viewer remains). Each `RoomActor` tracks `host: Option<PeerInfo>`, a `departed: Option<String>` host connection ID during reconnect grace, connected `viewers: HashMap<String, PeerInfo>`, and active `sessions: HashMap<String, Session>` keyed by `ice_id` (recording `viewer`, `from_device`, `desktop: bool`, and `started_at`).
  - When a viewer sends `soac` or `forward_setting`, `RoomActor` verifies that `ice_id` belongs to a session owned by that viewer connection before forwarding to the host; when relaying in either direction, it overwrites `client_id` in the JSON payload with the sender's verified connection ID.
  - When a viewer sends a `control` event, `RoomActor` decodes the binary `gv_pb.ConnectOptions` attachment. Payloads with `capture_type <= 2` (`CT_UNKNOWN`, `CT_DESKTOP`, `CT_WINDOW`) or invalid protobuf bytes count as a desktop control session, while utility sessions (`capture_type > 2`, such as file transfer, terminal, or port forwarding) run alongside desktop sessions. Only one viewer device (`from_device`) may hold a desktop session in a room at a time: if another device holds it and `force_join` is `false`, REST `/room/join/by_device` returns error code `2002` and signaling `control` replies `["failure", {"reason": "controlled from another device"}]`. With `force_join: true`, the actor terminates the existing desktop session by sending `released` to the host and `left` to the displaced viewer before requesting an ICE grant from `relay` and emitting `be-controlled` to the host.
  - If a host WebSocket closes while `sessions` is non-empty, `RoomActor` records `departed` and starts a 30-second `ReconnectGrace` timer. If the host reconnects within 30 seconds, it resumes ownership of all existing sessions; if the timer expires first, the actor sends `left` (with each session's `ice_id`) to the viewers and tears down the room.
  - When a new host connection replaces an existing host, or when a user unbinds a device or revokes a host session, the server sends `left` with `{}` (no `ice_id`) to the previous host and closes its WebSocket.

## 5. Client IP Extraction and GCRA Rate Limiting

### Client Address Extraction (`api/address.rs`)

The `ClientAddress` extractor resolves the caller's IP for rate limiting and login confirmation pages. On TCP connections whose peer IP is outside `trusted_proxies`, Remora uses the socket peer IP directly. When the TCP peer matches `trusted_proxies`, or when the connection arrives over a Unix domain socket, Remora scans `X-Forwarded-For` from right to left (accepting plain IPs, `IP:port`, `[IPv6]:port`, and IPv4-mapped IPv6 addresses) and picks the first address outside `trusted_proxies`. Scanning right-to-left stops at the boundary of the trusted proxy chain and ignores any client-supplied entries prepended on the left of `X-Forwarded-For`.

### GCRA Rate Limiters (`api/middleware/limit.rs`)

Unauthenticated endpoints, password checks, and wake requests enforce Generic Cell Rate Algorithm (GCRA) limits. The limiter keys IPv4 callers by single address and aggregates IPv6 callers by `/64` prefix so rotating addresses within one `/64` subnet shares the same quota. Each limiter tracks up to `MAX_TRACKED = 65_536` keys; when full, it evicts entries whose quota has fully refilled, and routes overflow while full to a shared fallback bucket to keep memory usage bounded.

| Operation | Key Dimension | Burst | Refill Rate |
|---|---|---|---|
| Owner password check | Per client IPv4 / IPv6 `/64` | 5 | 1 per 60 s |
| Owner password check | Global total | 30 | 1 per 10 s |
| QR login generation | Per client IPv4 / IPv6 `/64` | 10 | 1 per 10 s |
| Device registration | Per client IPv4 / IPv6 `/64` | 10 | 1 per 60 s |
| Wake-on-LAN request | Per client IPv4 / IPv6 `/64` | 5 | 1 per 30 s |

## 6. SQLite Storage and Log Redaction

`store/database.rs` opens SQLite with `journal_mode = WAL`, `synchronous = NORMAL`, a 5-second `busy_timeout`, and `STRICT` tables migrated sequentially via `PRAGMA user_version` (schema version `7`):

| Table | Columns | Purpose |
|---|---|---|
| `secrets` | `name TEXT PRIMARY KEY, value BLOB` | Persists the 32-byte JWT signing key `token_key`. |
| `devices` | `id TEXT PRIMARY KEY, client_id TEXT UNIQUE, name TEXT, platform INTEGER, owner TEXT, controllable INTEGER, version_name TEXT` | Registers devices idempotently by installation `client_id`; sets `owner` on `room/create`, and records reported `version_name` and `controllable` state. |
| `sessions` | `id TEXT PRIMARY KEY, user_id TEXT, device_id TEXT, created INTEGER` | Tracks active client User Token sessions; indexed by `device_id` for cascade cleanup on unbind. |
| `users` | `id TEXT PRIMARY KEY, issuer TEXT, subject TEXT, name TEXT` | Pre-seeded with `id = '1'` (`Owner`); OIDC users keyed by `UNIQUE (issuer, subject)`. |
| `browsers` | `id TEXT PRIMARY KEY, user_id TEXT, csrf TEXT, created INTEGER, expires INTEGER` | Tracks browser sessions, with the SHA-256 digest of `remora_session` as `id`. |
| `lan_reports` | `device_id TEXT PRIMARY KEY, mac TEXT, ip TEXT, subnet_mask TEXT` | Each device's last Wake-on-LAN report, kept while the device is offline; deleted on unbind. |
| `files` | `id TEXT PRIMARY KEY, user_id TEXT, device_id TEXT, mime TEXT, md5 TEXT, content BLOB` | Uploaded images. `id` is random and appears in the file's URL; the last 4 per user and device are kept, plus current wallpapers; a device's uploads are deleted on unbind. |
| `wallpapers` | `device_id TEXT PRIMARY KEY, file_id TEXT` | The image each device's card shows; deleted on unbind. |
| `device_apps` | `device_id TEXT PRIMARY KEY, apps_json TEXT` | Host-reported application list JSON; deleted on unbind. |

Log statements record only entity identifiers (`user_id`, `device_id`), session counts, byte sizes, and booleans. Secret- and PII-bearing types—including `SignKey`, `TurnSecret`, `OwnerPassword`, `Database`, `LanReport`, and `FileId` in `remora-cloud`, as well as `SignedHttpRequest`, `InputMessage`, `ClipboardMessage`, and `SignalingTransmit` in `remora-proto`—implement redacted `Debug` output so JWTs, cookies, QR URLs and tickets, password hashes, TURN secrets and derived credentials, OIDC client secrets, SDP `ufrag` / `pwd`, MAC/LAN/netmask values, upload tokens, file URLs, and typed user text stay out of logs, traces, and error reports.
