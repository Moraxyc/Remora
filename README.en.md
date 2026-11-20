# Remora

[简体中文](README.md) | [English](README.en.md)

Remora is an unofficial, self-hostable NetEase UU Remote (网易 UU 远程) cloud server written in Rust. Existing hosts (`uuremote-server`) and viewers (`uuremote-viewer`) that speak the UU protocol point their base URL at Remora to log in, list devices, create and join rooms, and exchange WebRTC signaling. Once signaling completes, desktop media, input, terminal, and file transfer traffic flows directly between host and viewer over WebRTC, either peer-to-peer or relayed through `coturn`.

This project is developed independently and has no affiliation with or endorsement from NetEase.

```text
uuremote-viewer ──REST + Socket.IO──► remora ◄──REST + Socket.IO── uuremote-server
       └─────────────── WebRTC (P2P, or relayed through coturn) ───────────────┘
```

## Status

Remora is in early development and supports end-to-end sessions with `uu-remote-linux`. A client's login QR code opens Remora's embedded web app, where a user signs in with an OpenID Connect provider or the owner password and confirms the device login. From the same web app, users inspect bound devices and their desktop wallpapers, unbind hosts, wake offline devices through an online device on the same LAN (Wake-on-LAN), and revoke active client or browser sessions.

## Quick Start

```sh
remora --config remora.toml
```

[`contrib/remora.toml`](contrib/remora.toml) documents every setting with examples, and [`contrib/turnserver.conf`](contrib/turnserver.conf) provides a matching `coturn` configuration. Place Remora behind a TLS reverse proxy that forwards WebSocket upgrades, and enable at least one login confirmation method (`login.password_file`, `[oidc]`, or `login.auto_confirm = true` on a private network). On internet-facing servers, set `sign_key_file` so Remora verifies the `x-param-sign` HMAC-SHA256 signature on every client REST call.

Prebuilt packages and client integrations:

- **Debian and Ubuntu**: Install `remora` from the [`pkgs.moraxyc.com`](https://pkgs.moraxyc.com) APT repository for automatic updates via `apt upgrade`, or download the `amd64` and `arm64` `.deb` packages from GitHub Releases. The package installs a sandboxed `remora.service` and socket-activated `remora.socket` that read `/etc/remora/remora.toml` and store SQLite state in `/var/lib/remora/remora.db`.
- **NixOS**: The flake exports `nixosModules.default` to run `services.remora` and an optional co-located `services.remora.coturn`, loading secret files through systemd `LoadCredential`.
- **Client (`uu-remote-linux`)**: Upstream provides its own flake (`github:VallierDev/uu-remote-linux`) with `packages.uu-remote-linux` and `nixosModules.default`. Version `0.2.62` and later natively read `UU_BASE_URL` / `"base_url"` and `UU_SIGN_KEY` / `"sign_key"`.

## Documentation

- [Deployment and Operations](docs/en/deployment.md): Reverse proxy and `trusted_proxies`, `.deb` / NixOS / static binary setup, Owner password and OIDC configuration, `coturn` relay setup, client connection, and troubleshooting.
- [Architecture and Security](docs/en/architecture.md): Workspace crates, HTTP routing and CSP headers, JWT and browser session storage, room state machine and 30-second reconnect grace, GCRA rate limiting, and log redaction.
- [Wire Protocol and API Reference](docs/en/protocol.md): `x-param-*` request signing, `{code, msg, data}` envelope and error codes, REST endpoints, Engine.IO v4 / Socket.IO v5 frames, and TURN credential derivation.
- [Development and Build Guide](docs/en/development.md): `nix develop` shell, embedded SvelteKit frontend workflow, linting, and the NixOS VM integration test.

## Acknowledgements

Much of what Remora knows about the UU Remote protocol comes from [uu-remote-linux](https://github.com/VallierDev/uu-remote-linux) by VallierDev and contributors (GPL-3.0-or-later), a UU Remote host and viewer for Linux and the main client Remora targets. Remora is an independent implementation.

## License

| Path | License |
|---|---|
| `crates/remora-proto` | MIT OR Apache-2.0 |
| `crates/remora-cloud` | GPL-3.0-or-later |
| everything else, including `web` | MIT |

See the license files in each directory.
