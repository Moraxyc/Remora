# Remora

[简体中文](README.md) | [English](README.en.md)

Remora is an unofficial, self-hostable reimplementation of the NetEase UU Remote (网易 UU 远程) protocol ecosystem written in Rust. The workspace comprises the self-hostable cloud server `remora` (`remora-cloud`), the sans-IO wire protocol library `remora-proto`, the `str0m`-based endpoint SDK `remora-endpoint`, and the conformance lab `remora-lab`. Existing hosts (`uuremote-server`) and viewers (`uuremote-viewer`) that speak the UU protocol point their base URL at Remora to log in, list devices, create and join rooms, and exchange WebRTC signaling. Once signaling completes, desktop media, input, clipboard, terminal, and file transfer traffic flows directly between host and viewer over WebRTC, either peer-to-peer or relayed through `coturn`.

This project is developed independently and has no affiliation with or endorsement from NetEase.

```text
uuremote-viewer ──REST + Socket.IO──► remora ◄──REST + Socket.IO── uuremote-server
       └─────────────── WebRTC (P2P, or relayed through coturn) ───────────────┘
```

## Status

Remora is under active development. It supports end-to-end sessions with `uu-remote-linux`, alongside macOS and Windows client control-plane compatibility endpoints and compile-time embedded client profiles (`macos-4.48.1`, `macos-4.48.2`, `windows-4.48.2`). A client's login QR code opens Remora's embedded web app, where a user signs in with an OpenID Connect provider or the owner password and confirms the device login. From the same web app, users inspect bound devices and their desktop wallpapers, unbind hosts, wake offline devices through an online device on the same LAN (Wake-on-LAN), and revoke active client or browser sessions.

The cloud server runs either as a single process or split by role (`--roles api,gateway,room,relay`) across processes communicating over a local Unix-socket bus (`--bus-listen` / `--bus-connect`). The workspace also includes `remora-endpoint` (control plane, signaling driver, and `str0m` WebRTC host/viewer sessions) and `remora-lab` (YAML scenario runner, P2P/TURN relay verification, and cross-implementation compatibility matrix).

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

- [Deployment and Operations](docs/en/deployment.md): Reverse proxy and `trusted_proxies`, standalone vs role-split deployment (`--roles` + `--bus-listen` / `--bus-connect`), `.deb` / NixOS / static binary setup, Owner password and OIDC configuration, `coturn` relay setup, client connection, and troubleshooting.
- [Architecture and Security](docs/en/architecture.md): Workspace crate boundaries and dependency policies, sans-IO state machines (`remora-proto`), role bus and per-room actor model (`remora-cloud`), endpoint SDK (`remora-endpoint`) and lab (`remora-lab`), HTTP routing and CSP headers, JWT and browser session storage, GCRA rate limiting, and log redaction.
- [Wire Protocol and API Reference](docs/en/protocol.md): `x-param-*` request signing, `{code, msg, data}` envelope and error codes, REST endpoints (including desktop client compatibility and update endpoints), Engine.IO v4 / Socket.IO v5 frames, session data channels, and TURN credential derivation.
- [Development and Build Guide](docs/en/development.md): `nix develop` shell, embedded SvelteKit frontend workflow, `cargo-deny` boundary checks, `remora-lab` scenarios and matrix runs, and NixOS VM integration tests.
- [Endpoint Driver Protocol](compat/driver-protocol.md): JSON-RPC 2.0 protocol used by `remora-lab` to drive external endpoint processes.

## Acknowledgements

Much of what Remora knows about the UU Remote protocol comes from [uu-remote-linux](https://github.com/VallierDev/uu-remote-linux) by VallierDev and contributors (GPL-3.0-or-later), a UU Remote host and viewer for Linux and the main client Remora targets. Remora is an independent implementation.

## License

| Path | License |
|---|---|
| `crates/remora-proto`, `crates/remora-endpoint`, `crates/remora-lab` | MIT OR Apache-2.0 |
| `crates/remora-cloud` | GPL-3.0-or-later |
| everything else, including `web` and `compat` | MIT |

See the license files in each directory.
