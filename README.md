# Remora

An unofficial, self-hostable stand-in for the NetEase UU Remote (网易 UU 远程) cloud, written in
Rust. Existing hosts and viewers that speak the UU protocol log in, find each other and exchange
signaling through it, without any NetEase service.

This project is not affiliated with or endorsed by NetEase.

## Status

Early development; nothing works yet.

## Development

```sh
nix develop
cargo build
cargo test
nix fmt
```
