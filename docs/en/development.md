# Development and Build Guide

[简体中文](../zh/development.md) | [English](development.md) | [Overview](../../README.en.md)

## 1. Development Environment

The repository's Nix Flake provides the development shell with `cargo`, `cargo-deny`, `rustc`, `clippy`, `rustfmt`, `rust-analyzer`, `nodejs`, and `treefmt`:

```sh
nix develop
```

Development-only inputs—including `treefmt-nix`, `moraxyc-repo`, and upstream `uu-remote-linux` (for integration testing)—live in `dev/flake.nix` so they stay out of downstream consumers' `flake.lock`.

## 2. Building and Frontend Workflow

`crates/remora-proto/build.rs` compiles `proto/gv_pb.proto` in pure Rust using `protox` and `prost-build` (no system `protoc` required) and validates and merges `profiles/*.toml` into embedded JSON at compile time.

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

## 3. Linting, Boundary Checks, and Tests

Run the workspace checks, `cargo-deny` policies, and formatter before committing:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny --config deny.toml check bans licenses sources
cargo deny --manifest-path crates/remora-proto/Cargo.toml --config crates/remora-proto/deny.toml check bans
cargo deny --manifest-path crates/remora-endpoint/Cargo.toml --config crates/remora-endpoint/deny.toml check bans licenses sources
cargo deny --manifest-path crates/remora-lab/Cargo.toml --config crates/remora-lab/deny.toml check bans licenses sources
nix fmt
```

### Running `remora-lab` Scenarios and Compatibility Matrices

The `compat/` directory holds cloud and session YAML scenarios (`compat/scenarios/`), wire test vectors (`compat/vectors/`), trace JSON schemas (`compat/schema/`), and the external endpoint driver specification ([`compat/driver-protocol.md`](../../compat/driver-protocol.md)). After building the workspace, invoke `remora-lab` directly:

```sh
# Run all cloud and session scenarios against a spawned standalone remora + test TURN server
cargo run --package remora-lab -- run compat/scenarios --remora-bin target/debug/remora

# Compare normalized traces between standalone and role-split deployments
cargo run --package remora-lab -- matrix compat/scenarios --mode cloud --remora-bin target/debug/remora

# Run the P2P and forced-TURN session matrix
cargo run --package remora-lab -- matrix compat/scenarios/session --mode session --remora-bin target/debug/remora
```

### NixOS VM End-to-End Tests

On Linux, `nix flake check` builds every flake package, runs `fmt`, `clippy`, `test`, `deny-workspace`, and `deny-proto`, and executes three NixOS VM integration suites:

- [`nix/tests/remora.nix`](../../nix/tests/remora.nix) (`nixos`): boots `services.remora` alongside `services.remora.coturn` and drives a full session with a Python client—verifying `remora.socket` activation, QR login, room signaling, `turnutils_uclient` TURN validation, SQLite persistence across restarts, and `journalctl` secret redaction.
- [`nix/tests/compat.nix`](../../nix/tests/compat.nix) (`compat`): runs `remora-lab` cloud and session scenarios against NixOS `remora` + `coturn` and asserts an identical standalone-vs-split compatibility matrix with zero secret leaks.
- [`nix/tests/interop-linux.nix`](../../nix/tests/interop-linux.nix) (`interop-linux`): runs upstream `uu-remote-linux` (host + viewer) alongside `remora-lab` against `remora` + `coturn`—verifying WebRTC file transfer, the 3-column session matrix (`lab-lab-p2p`, `lab-lab-relay`, `lab-uu-remote-linux`), and `uuremote-viewer` ↔ `remora-lab serve-host` video/input interop.

## 4. Flake Outputs and Packaging

| Flake Output | Description |
|---|---|
| `packages.<sys>.remora` | Default package dynamically linked against system `aws-lc` and `sqlite`, including systemd service and socket units. |
| `packages.<sys>.remora-lab` | `remora-lab` conformance and scenario runner binary. |
| `packages.<sys>.remora-web` | Static SvelteKit frontend build. |
| `packages.<sys>.remora-static` | Static musl-linked stripped Linux binary used for release tarballs. |
| `packages.<sys>.remora-deb` | Debian and Ubuntu `.deb` package tested in CI with `lintian` and systemd install/upgrade/remove/purge scripts. |
| `nixosModules.default` | NixOS module for `remora` and optional co-located `coturn`. |

Build individual release artifacts with:

```sh
nix build .#remora-static
nix build .#remora-deb
```
