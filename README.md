# Remora

An unofficial, self-hostable stand-in for the NetEase UU Remote (网易 UU 远程) cloud, written in
Rust. Existing hosts and viewers that speak the UU protocol log in, find each other and exchange
signaling through it, without any NetEase service.

This project is not affiliated with or endorsed by NetEase.

## Status

Early development. Hosts and viewers from uu-remote-linux log in, find each other and connect,
peer to peer or relayed through coturn. A client's QR code opens Remora's web app, where a user
signs in, with an OpenID Connect provider or the owner's password, and confirms the login.

## Running

```sh
remora --config remora.toml
```

[`contrib/remora.toml`](contrib/remora.toml) describes every setting, and
[`contrib/turnserver.conf`](contrib/turnserver.conf) is a coturn configuration to go with it.
[`contrib/remora.service`](contrib/remora.service) is a sandboxed systemd unit that runs
`/usr/bin/remora` with `/etc/remora/remora.toml`. Enable
[`contrib/remora.socket`](contrib/remora.socket) as well, or instead, to have systemd listen in
Remora's place: it starts Remora on the first connection, and connections wait while Remora
restarts. Keep its `ListenStream` the same as `listen`.
Serve it over TLS through a reverse proxy that passes WebSocket upgrades through. When listening
on TCP, list the proxy in `trusted_proxies` so that Remora sees clients' addresses in
`X-Forwarded-For` (rate limits and login pages go by them); connections over a Unix socket
(`listen = "/run/remora/remora.sock"`, which requires `public_url`) trust `X-Forwarded-For` by
default, and `remora.socket` can set its group and mode for the proxy.

Users sign in to the web app in one of two ways:

- **OpenID Connect:** set `[oidc]` to a provider such as Authelia, Authentik or Keycloak, where
  Remora is registered with the redirect URI `{public_url}/api/web/oidc/callback`. Everyone the
  provider lets sign in is a Remora user, with their own devices; Remora keeps no accounts of
  its own.
- **The owner's password:** make the file with `remora hash-password > owner.password` and name
  it in `login.password_file`. Each client gets five tries, then one a minute, and guesses are
  capped in total too, which can also keep the owner out while someone is guessing; use a long
  password.

Both can be set. To send everyone to the provider, leave `login.password_file` unset: the
sign-in page then goes straight to it.

To log a client in, open the address in its QR code (scan it with a phone, or copy it), sign
in, and confirm: the client logs in as you. A browser stays signed in for 30 days. On a private
network, `login.auto_confirm = true` instead logs every client in as the owner without asking.

Registering a device and starting a QR login need no account, and device registrations are kept
for good, because clients register once and reuse the device id forever. The signing key is what
keeps strangers from calling them: on a server that the internet can reach, set `sign_key_file`
to a random key and configure your clients with it.

Clients have to be pointed at Remora. The flake also packages a patched
[uu-remote-linux](https://github.com/VallierDev/uu-remote-linux) (`packages.uu-remote-linux` and
`overlays.default`) that reads `UU_BASE_URL` or `"base_url"` in its config file (and `UU_SIGN_KEY`
/ `"sign_key"` when `sign_key_file` is set).

### Debian and Ubuntu

Each release carries a `.deb` for amd64 and arm64. It installs `remora.service`, which reads
`/etc/remora/remora.toml` and keeps its database in `/var/lib/remora`, `remora.socket`, and a
`remora` group that may read the settings and secrets there while they stay root's. The example
settings name files that do not exist yet; keep at least one way to confirm logins
(`login.password_file`, `[oidc]` or `login.auto_confirm`) and make the files it names:

```sh
sudo cp /usr/share/doc/remora/examples/remora.toml /etc/remora/
# Edit it, then give it and every secret file it names to the group.
sudo chgrp remora /etc/remora/*
sudo chmod 0640 /etc/remora/*
sudo systemctl enable --now remora
```

To start Remora on the first connection instead, enable `remora.socket`
(`sudo systemctl enable --now remora.socket`) rather than the service.

Purging the package deletes the database, which holds the key that signs tokens, and keeps
`/etc/remora`.

### NixOS

The flake provides a module that runs Remora and, optionally, coturn next to it. systemd listens
on `listen` and starts Remora on the first connection:

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
reloading and passes `/api` on to a Remora at `REMORA_URL` (default `http://[::1]:8680`);
`npm run check` type-checks it. Nix fetches its dependencies as `web/package-lock.json` lists
them.

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
