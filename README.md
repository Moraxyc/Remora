# Remora

An unofficial, self-hostable stand-in for the NetEase UU Remote (网易 UU 远程) cloud, written in
Rust. Existing hosts and viewers that speak the UU protocol log in, find each other and exchange
signaling through it, without any NetEase service.

This project is not affiliated with or endorsed by NetEase.

## Status

Early development. Hosts and viewers from uu-remote-linux log in, find each other and connect,
peer to peer or relayed through coturn. The owner confirms each login on a web page that the
client's QR code opens, after signing in with a password.

## Running

```sh
remora --config remora.toml
```

[`contrib/remora.toml`](contrib/remora.toml) describes every setting, and
[`contrib/turnserver.conf`](contrib/turnserver.conf) is a coturn configuration to go with it.
Serve it over TLS through a reverse proxy that passes WebSocket upgrades through.

Make the owner's password file with `remora hash-password > owner.password` and name it in
`login.password_file`. To log a client in, open the address in its QR code (scan it with a
phone, or copy it), sign in, and confirm. A signed-in browser stays signed in for 30 days.
Wrong passwords are not rate limited yet, so use a long password. On a private network,
`login.auto_confirm = true` confirms every login without asking instead.

Registering a device and starting a QR login need no account, and device registrations are kept
for good, because clients register once and reuse the device id forever. The signing key is what
keeps strangers from calling them: on a server that the internet can reach, set `sign_key_file`
to a random key and configure your clients with it.

Clients have to be pointed at Remora. uu-remote-linux has its API base URL in `src/auth.rs`.

### NixOS

The flake provides a module that runs Remora and, optionally, coturn next to it:

```nix
{
  imports = [ remora.nixosModules.default ];

  services.remora = {
    enable = true;
    # Devices and logins are kept in /var/lib/remora/remora.db.
    settings = {
      public_url = "https://remora.example";
      # Secret files only need to be readable by root. coturn shares the TURN secret.
      sign_key_file = "/run/secrets/remora-sign";
      ice.turn_secret_file = "/run/secrets/remora-turn";
      # Made with `remora hash-password`.
      login.password_file = "/run/secrets/remora-owner";
    };
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
(cd web && npm ci && npm run build)   # the binary embeds the web app's build
cargo build
cargo test
nix fmt
nix flake check   # includes a NixOS VM test
```

The web app in `web/` is a SvelteKit single-page app. `npm run dev` there serves it with live
reloading and passes `/api` on to a Remora at `REMORA_URL` (default `http://127.0.0.1:8080`);
`npm run check` type-checks it. After changing its dependencies, update `npmDepsHash` in
`nix/web.nix`.

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
| everything else, including `web` | MIT |

See the license files in each directory.
