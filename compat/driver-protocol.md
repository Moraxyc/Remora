# Remora Endpoint Driver Protocol (v0)

`remora-lab` defines a newline-delimited JSON-RPC 2.0 protocol over `stdin`/`stdout` so an external endpoint (such as a native app bridge or standalone test harness) can be driven by scenarios without linking it directly.

## Transport

- **Framing:** one UTF-8 JSON object per line (`\n`-terminated) on `stdin` (requests) and `stdout` (responses and asynchronous notifications).
- **Logging:** diagnostic logs must go to `stderr`, never `stdout`.
- **Envelope:** standard JSON-RPC 2.0 (`"jsonrpc": "2.0"`).

## Requests (Lab → Driver)

| Method | Params | Result |
| --- | --- | --- |
| `initialize` | `{ "base_url": string, "profile": string, "role": "host" \| "viewer", "sign_key"?: string, "client_id"?: string, "device_name"?: string }` | `{ "ready": true }` |
| `register_device` | `{}` | `{ "device_id": string }` |
| `login` | `{}` | `{ "user_id": string }` |
| `register_host` | `{}` | `{ "connected": true }` |
| `connect` | `{ "device_id": string, "force_join"?: bool }` | `{ "connected": true }` |
| `send_control` | `{ "app_control_id"?: string, "capture_type"?: number }` | `{ "status": "success" \| "failure", "ice_id"?: string, "reason"?: string }` |
| `send_soac` | `{ "ice_id": string, "client_id"?: string, "data": object }` | `{ "sent": true }` |
| `send_input` | `{ "message": object }` | `{ "sent": true }` |
| `push_video_frame` | `{ "codec": "h264" \| "h265", "is_keyframe": bool, "pts_ms": number, "annex_b_hex": string }` | `{ "pushed": true }` |
| `close_room` | `{}` | `{ "closed": true }` |
| `shutdown` | `{}` | `{ "ok": true }` |

## Notifications (Driver → Lab)

The driver may emit unsolicited JSON-RPC notifications (`"id"` omitted):

- `signaling_event`: `{ "name": string, "payload": object }`
- `video_frame_received`: `{ "is_keyframe": bool, "pts_ms": number, "len": number }`
- `input_received`: `{ "kind": string }` (input text and key codes must be omitted or redacted)

## Redaction

Drivers must not include user tokens, room tokens, QR URLs/tickets, TURN credentials, or SDP `ice-ufrag`/`ice-pwd`/`fingerprint` fields in notifications or error messages.
