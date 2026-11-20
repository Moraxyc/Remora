# Development and Build Guide

[简体中文](../zh/development.md) | [English](development.md) | [Overview](../../README.en.md)

## 1. Development Environment

The repository's Nix Flake provides the development shell with `cargo`, `rustc`, `clippy`, `rustfmt`, `rust-analyzer`, `nodejs`, and `treefmt`:

```sh
nix develop
```

## 2. Building and Frontend Workflow

`crates/remora-cloud/build.rs` embeds the SvelteKit static build into the `remora` binary at compile time via `rust-embed`. By default it reads `web/build/` (which must contain `app.html`), or the directory set in `REMORA_WEB_DIR` (used by Nix package builds to pass `.#remora-web`). Build `web/` once before running `cargo build` or `cargo test` in a fresh checkout:

```sh
(cd web && npm ci && npm run build)
cargo build
```

When iterating on the SvelteKit app in `web/`, run a local `remora` backend in one terminal and the Vite dev server in another. Vite serves the frontend with live reloading and proxies `/api` requests to `REMORA_URL` (configured in `web/vite.config.ts`, defaulting to `http://[::1]:8680`):

```sh
# Terminal 1: start Remora with a local development config
cargo run --package remora-cloud -- --config dev-remora.toml

# Terminal 2: start the Vite dev server
cd web
REMORA_URL=http://[::1]:8680 npm run dev
```

Run `npm run check` inside `web/` to execute `svelte-check` and TypeScript validation.

## 3. Linting, Formatting, and Tests

Run the workspace checks and formatter before committing:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
nix fmt
```

On Linux, `nix flake check` builds every flake package, runs `clippy` and `treefmt`, and executes the two-node NixOS VM integration test in [`nix/tests/remora.nix`](../../nix/tests/remora.nix). The VM test boots `services.remora` alongside `services.remora.coturn` and drives a full session with a Python test client: it verifies on-demand activation via `remora.socket`, runs QR login, room creation, and `be-controlled` / `soac` signaling, validates issued TURN credentials against `coturn` with `turnutils_uclient`, restarts the service to verify SQLite persistence, and audits `journalctl` output for secret and token redaction.

## 4. Flake Outputs and Packaging

| Flake Output | Description |
|---|---|
| `packages.<sys>.remora` | Default package dynamically linked against system `aws-lc` and `sqlite`, including systemd service and socket units. |
| `packages.<sys>.remora-web` | Static SvelteKit frontend build. |
| `packages.<sys>.remora-static` | Static musl-linked stripped Linux binary used for release tarballs. |
| `packages.<sys>.remora-deb` | Debian and Ubuntu `.deb` package tested in CI with `lintian` and systemd install/upgrade/remove/purge scripts. |
| `packages.<sys>.uu-remote-linux` | Linux host and viewer package for `uu-remote-linux` (wrapped runtime dependencies, desktop entries, systemd user unit, and udev rule). |
| `nixosModules.default` | NixOS module for `remora` and optional co-located `coturn`. |

Build individual release artifacts with:

```sh
nix build .#remora-static
nix build .#remora-deb
```
