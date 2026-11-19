# Remora

An unofficial, self-hostable stand-in for the NetEase UU Remote (网易 UU 远程) cloud, written in
Rust. Existing hosts and viewers that speak the UU protocol log in, find each other and exchange
signaling through it, without any NetEase service.

This project is not affiliated with or endorsed by NetEase.

## Status

Early development. Hosts and viewers from uu-remote-linux log in, find each other and connect,
peer to peer or relayed through coturn. Logins are confirmed automatically for now, so anyone
who can reach the server can log in as its user: run it on a private network only.

## Running

```sh
remora --config remora.toml
```

[`contrib/remora.toml`](contrib/remora.toml) describes every setting, and
[`contrib/turnserver.conf`](contrib/turnserver.conf) is a coturn configuration to go with it.
For now the settings must include `login.auto_confirm = true`. Serve it over
TLS through a reverse proxy that passes WebSocket upgrades through.

Clients have to be pointed at Remora. uu-remote-linux has its API base URL in `src/auth.rs`.

### NixOS

The flake provides a module that runs Remora and, optionally, coturn next to it:

```nix
{
  imports = [ remora.nixosModules.default ];

  services.remora = {
    enable = true;
    settings = {
      public_url = "https://remora.example";
      login.auto_confirm = true;
    };
    # Shared by Remora and coturn; it only needs to be readable by root.
    turnSecretFile = "/run/secrets/remora-turn";
    coturn = {
      enable = true;
      domain = "turn.example";
      openFirewall = true;
    };
  };
}
```

## Development

```sh
nix develop
cargo build
cargo test
nix fmt
nix flake check   # includes a NixOS VM test
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
