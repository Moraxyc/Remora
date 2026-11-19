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

## Acknowledgements

Much of what Remora knows about the UU Remote protocol comes from
[uu-remote-linux](https://github.com/VallierDev/uu-remote-linux) by VallierDev and contributors
(GPL-3.0-or-later), a UU Remote host and viewer for Linux and the main client Remora targets.
Remora is a separate implementation and contains no code from it.

## License

| Path | License |
|---|---|
| `crates/remora-proto` | MIT OR Apache-2.0 |
| `crates/remora-cloud` | GPL-3.0-or-later |
| everything else | MIT |

See the license files in each directory.
