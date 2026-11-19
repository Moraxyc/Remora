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
Serve it over TLS through a reverse proxy that passes WebSocket upgrades through, and list the
proxy in `trusted_proxies` so that Remora sees clients' addresses in `X-Forwarded-For`: rate
limits and login pages go by them.

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
