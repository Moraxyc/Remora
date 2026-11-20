# Deployment and Operations

[简体中文](../zh/deployment.md) | [English](deployment.md) | [Overview](../../README.en.md)

Remora serves the UU client REST API, the web sign-in and management console, and Socket.IO signaling over HTTP and WebSocket. In production, a TLS-terminating reverse proxy sits in front of Remora and forwards requests to its TCP port or Unix domain socket. Once signaling completes, hosts and viewers exchange desktop media, input, and files directly over WebRTC, either peer-to-peer or relayed through `coturn` using 24-hour TURN credentials minted by Remora.

```text
Clients (Host / Viewer)
   │
   ├── HTTPS / WSS ──► TLS Reverse Proxy ──► Remora (TCP or Unix Socket)
   │                                            └─ Issues 24-hour TURN credentials
   └── STUN / TURN (UDP/TCP) ──────────► coturn (used across NAT or to hide peer IPs)
```

Remora checks two configuration rules at startup. First, `public_url` must be a bare origin consisting of scheme, host, and optional port (such as `https://remora.example` or `https://remora.example:8443`); UU clients sign the raw `/api/v1/...` request path with HMAC-SHA256, so the reverse proxy must forward paths at the root and a `public_url` with a path prefix is rejected. Second, at least one login confirmation method must be enabled in `remora.toml`: `login.password_file`, `[oidc]`, or `login.auto_confirm = true`.

On internet-facing deployments, configure `sign_key_file` as well. Device registration and QR login generation run before a client holds a user token; sharing a signing key between Remora and your clients requires every `/api/v1/*` request to carry a valid `x-param-sign` signature.

## 1. Installation

### Debian and Ubuntu (APT Repository and `.deb` Package)

Each release is indexed in the [`pkgs.moraxyc.com`](https://pkgs.moraxyc.com) APT repository for `amd64` and `arm64` so `remora` can be installed and updated automatically through `apt`; standalone `.deb` files are also attached to GitHub Releases. To add the APT repository and install `remora`:

```sh
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://pkgs.moraxyc.com/deb/keys/moraxyc-archive-keyring.gpg | sudo tee /etc/apt/keyrings/moraxyc-archive-keyring.gpg > /dev/null
printf '%s\n' 'deb [signed-by=/etc/apt/keyrings/moraxyc-archive-keyring.gpg] https://pkgs.moraxyc.com/deb stable main' | sudo tee /etc/apt/sources.list.d/moraxyc.list
sudo apt-get update && sudo apt-get install remora
```

Installing the package creates the `remora` system user and group, creates `/etc/remora` owned by `root:remora` with mode `0750`, and installs the sandboxed `remora.service` and `remora.socket` units with `/var/lib/remora/remora.db` as the SQLite state file.

Copy the example configuration, generate the secret files, grant read access to the `remora` group, and start either the service or the socket unit:

```sh
sudo cp /usr/share/doc/remora/examples/remora.toml /etc/remora/remora.toml

head -c 32 /dev/urandom | base64 | sudo tee /etc/remora/sign.key > /dev/null
head -c 32 /dev/urandom | base64 | sudo tee /etc/remora/turn.secret > /dev/null
remora hash-password | sudo tee /etc/remora/owner.password > /dev/null

# Edit /etc/remora/remora.toml for your domain and authentication method, then run:
sudo chgrp remora /etc/remora/*
sudo chmod 0640 /etc/remora/*

# Enable the always-on service, or enable remora.socket for on-demand activation
sudo systemctl enable --now remora.service
```

Running `apt remove remora` stops the units and keeps both `/etc/remora` and `/var/lib/remora`. Running `apt purge remora` deletes `/var/lib/remora` (including registered devices, client sessions, and the JWT signing key) and preserves `/etc/remora`.

### NixOS (Flake Module)

The flake exports `nixosModules.default`, which configures socket-activated `remora.socket` and `remora.service`. Secret file paths are passed through systemd `LoadCredential`, so the files on disk only need read permissions for `root`. Enabling `services.remora.coturn.enable = true` runs a local `coturn` instance sharing the TURN secret, populates `services.remora.settings.ice.servers` with matching `stun:` and `turn:` URLs, and opens the firewall ports when `openFirewall = true`.

```nix
{
  imports = [ remora.nixosModules.default ];

  services.remora = {
    enable = true;
    settings = {
      public_url = "https://remora.example";
      listen = "[::1]:8680";
      trusted_proxies = [ "127.0.0.1" "::1" ];
      sign_key_file = "/run/secrets/remora-sign";
      ice.turn_secret_file = "/run/secrets/remora-turn";
      ice.force_relay = true;
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

### Static Binary and Manual systemd

GitHub Releases also ships static tarballs (`remora-<tag>-<arch>.tar.gz`) containing the `remora` binary and the sample files under `contrib/`. Run the binary directly with:

```sh
remora --config /etc/remora/remora.toml
```

When deploying with [`contrib/remora.service`](../../contrib/remora.service) and [`contrib/remora.socket`](../../contrib/remora.socket), the service unit runs under systemd sandboxing (`DynamicUser=yes`, `MemoryDenyWriteExecute=yes`, and system call filtering). Keep `ListenStream` in `remora.socket` identical to `listen` in `remora.toml`.

## 2. Configuration

[`contrib/remora.toml`](../../contrib/remora.toml) lists every setting with examples. Relative paths in `remora.toml` resolve against the directory containing `remora.toml`, and secret loaders strip a single trailing `\n` or `\r\n`.

### Listening Address, Reverse Proxy, and Client IP

| Setting | Default | Description |
|---|---|---|
| `listen` | `"[::1]:8680"` | TCP socket address (`IP:Port`) or Unix domain socket path (such as `"/run/remora/remora.sock"`). |
| `public_url` | Derived from `listen` | External origin clients connect to (such as `https://remora.example`). Required when `listen` is `0.0.0.0`, `[::]`, or a Unix socket. |
| `trusted_proxies` | `[]` | Reverse proxy IPs or CIDRs trusted to supply `X-Forwarded-For` (such as `["127.0.0.1", "::1"]`). |

Rate limiters and the login confirmation page identify callers by client IP. On TCP listeners, Remora inspects `X-Forwarded-For` only when the immediate TCP peer matches `trusted_proxies`, scanning the header right-to-left past trusted proxies and picking the first untrusted address. If a reverse proxy is missing from `trusted_proxies`, every request arriving through it is attributed to the proxy's own IP and shares a single rate-limit bucket. Connections over a Unix domain socket trust `X-Forwarded-For` automatically; when using `remora.socket` with a Unix socket, set `SocketGroup` and `SocketMode` (for example `www-data` and `0660`) so the proxy process can connect.

Configure the reverse proxy to forward `Host`, `X-Forwarded-For`, and WebSocket upgrade headers (`Upgrade` and `Connection` on `/socket.io/`). Set the upstream read timeout to at least 60 seconds so the proxy holds open 25-second QR status long-polls and 25-second Socket.IO heartbeats:

```nginx
server {
    listen 443 ssl http2;
    server_name remora.example;

    location / {
        proxy_pass http://127.0.0.1:8680;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_read_timeout 90s;
    }
}
```

### Login Confirmation Methods

Remora supports three ways to confirm client QR logins. You can enable the Owner password, OpenID Connect, or both together, or use automatic confirmation on an isolated private network:

1. **Owner Password (`login.password_file`)**: Create the Argon2id hash file with `remora hash-password > owner.password` (or `printf '%s\n' "$PW" | remora hash-password` in scripts). Signing in authenticates the built-in `Owner` user (`id = "1"`) and sets a 30-day browser session cookie. Password checks allow a burst of 5 attempts per client (per IPv4 address or IPv6 `/64` prefix, refilling 1 per 60 seconds) and a global burst of 30 attempts (refilling 1 per 10 seconds). Choose a long password so external guessing cannot exhaust the global limit while you are signing in.
2. **OpenID Connect (`[oidc]`)**: Connect an identity provider such as Authelia, Authentik, or Keycloak registered with the redirect URI `{public_url}/api/web/oidc/callback`. Set `issuer` and `client_id`; add `client_secret_file` for a confidential client or omit it to use PKCE as a public client, and optionally set `name` for the sign-in button label (defaults to the `issuer` host name). Each authenticated `(issuer, sub)` identity maps to a distinct Remora user with an isolated device list. When `[oidc]` is configured alone, the login page redirects straight to the provider; when combined with `login.password_file`, the login page offers both methods.
3. **Automatic Confirmation (`login.auto_confirm = true`)**: Marks every generated QR login as confirmed by `Owner` (`id = "1"`) immediately so clients log in without opening a browser. Because anyone who can reach the server logs in as the owner, use this option only on isolated private networks.

### Request Signing, Database, and Logging

`sign_key_file` points to a file containing the shared request signing key (use 32 or more random bytes). When configured, Remora verifies the `x-param-sign` HMAC-SHA256 header on all `/api/v1/*` endpoints. The web console API (`/api/web/*`) authenticates with session cookies and CSRF tokens, and signaling (`/socket.io/`) authenticates with Room JWTs.

`database` sets the SQLite file path (such as `/var/lib/remora/remora.db`), storing the token signing key, registered devices, users, and active client and browser sessions; omitting it keeps state in memory until the process exits. `log` accepts an `EnvFilter` string (default `"info"`, or `"info,remora_cloud=debug"` when diagnosing issues).

## 3. TURN Relay (coturn) and ICE Configuration

Deploy `coturn` when hosts and viewers sit behind symmetric NATs or when you want to hide peer IP addresses behind a relay. Remora and `coturn` share a static secret (`ice.turn_secret_file` in `remora.toml` and `static-auth-secret` with `use-auth-secret` in `turnserver.conf`) so Remora can derive 24-hour HMAC-SHA1 credentials on each control session.

Sample [`contrib/turnserver.conf`](../../contrib/turnserver.conf):

```ini
listening-port=3478
# Public IP when coturn sits behind NAT
#external-ip=203.0.113.10
realm=turn.example
fingerprint

use-auth-secret
# Same secret as Remora's ice.turn_secret_file
static-auth-secret=CHANGE-ME

min-port=49152
max-port=65535

no-tls
no-dtls
no-cli

no-multicast-peers
denied-peer-ip=0.0.0.0-0.255.255.255
denied-peer-ip=10.0.0.0-10.255.255.255
denied-peer-ip=100.64.0.0-100.127.255.255
denied-peer-ip=127.0.0.0-127.255.255.255
denied-peer-ip=169.254.0.0-169.254.255.255
denied-peer-ip=172.16.0.0-172.31.255.255
denied-peer-ip=192.168.0.0-192.168.255.255
denied-peer-ip=::1
denied-peer-ip=fc00::-fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff
denied-peer-ip=fe80::-febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff
```

Matching `[ice]` section in `remora.toml`:

```toml
[ice]
servers = ["stun:turn.example:3478", "turn:turn.example:3478"]
turn_secret_file = "/etc/remora/turn.secret"
force_relay = true
```

Setting `force_relay = true` instructs clients to use only `relay` ICE candidates so all media flows through `coturn` and peers only see the relay's IP address; this setting requires at least one `turn:` or `turns:` URL in `servers`. Open `3478/tcp`, `3478/udp` (plus `5349` if TLS/DTLS is enabled), and UDP ports `49152-65535/udp` in the firewall. The `denied-peer-ip` rules prevent clients from using the relay to reach the server's loopback or private networks; if `coturn` and all peers run inside the same private RFC 1918 subnet during testing, remove that subnet from `denied-peer-ip`.

## 4. Connecting Clients (`uu-remote-linux`)

This repository's flake provides Nix packaging for `uu-remote-linux` (`packages.<sys>.uu-remote-linux` and `overlays.default`, including desktop entries, the systemd user service, and an optional `/dev/uinput` udev rule). Version `0.2.62` and later natively accept a custom base URL and signing key via environment variables or the JSON configuration file:

```sh
export UU_BASE_URL="https://remora.example"
export UU_SIGN_KEY="same-value-as-sign_key_file"
```

Alternatively, set `"base_url"` and `"sign_key"` in `uu-remote-linux`'s config file (see `share/doc/uu-remote-linux/config.example.json` in the package). On NixOS, add the package to `services.udev.packages` to grant `/dev/uinput` access for Wayland input, and enable the bundled `uuremote-native.service` user unit to start the host with your graphical session.

When `uuremote-server` starts on a host, it registers the device and displays a QR code pointing to `{public_url}/login/<id>`. Open that link in a browser, sign in with the Owner password or OIDC, check the displayed device name, platform, and client IP, and click **Log in**. The host receives its user token, creates a signaling room, and comes online. Sign in on `uuremote-viewer` as the same user to see the online host and start a remote desktop session.

Wake-on-LAN needs `UU_WOL_HELPER=1` in the environment of `uuremote-server` on both the device to wake and the helper that wakes it: only then does a host report its MAC, LAN address and netmask to Remora, and act on wake requests. The helper must be on the target's LAN with host networking (not a container network namespace or NAT), so that its broadcast reaches the target. The target's network card must have Wake-on-LAN enabled in its firmware and operating system.

## 5. Operations and Troubleshooting

Opening `{public_url}/` in a browser loads the management console. The **Devices** tab lists devices bound to your account with their online status; clicking **Unbind** removes the device binding, deletes its host login sessions, and kicks any connected host from signaling. Offline devices that reported their LAN show **Wake**, which asks one of your online devices on the same IPv4 network to send a magic packet; the console says when no such device is online. The **Client logins** tab lists active host and viewer tokens with a **Revoke** button for each session, and the **Browsers** tab lists active 30-day browser sessions with remote sign-out.

When `remora.socket` is enabled, systemd binds the listening socket and starts `remora.service` on the first incoming connection, passing the socket file descriptor into the process. During `systemctl restart remora.service`, the kernel queues new connections on the socket until the replacement process starts accepting them. Once started, `remora` stays running to keep active Socket.IO rooms open.

All persistent state lives in the SQLite database file (default `/var/lib/remora/remora.db`, opened in WAL mode). Run online backups with SQLite's `.backup` command so committed WAL frames are included in the snapshot:

```sh
sudo sqlite3 /var/lib/remora/remora.db ".backup /tmp/remora-backup.db"
```

To migrate Remora to another machine, copy both the backed-up `remora.db` and `/etc/remora/*` so existing device bindings, active client tokens, and the JWT signing key carry over.

### Common Errors and Fixes

| Symptom or Error Message | Cause | Fix |
|---|---|---|
| Startup error: `nobody can confirm logins` | No login confirmation method is enabled | Configure `login.password_file`, `[oidc]`, or `login.auto_confirm = true` in `remora.toml`. |
| Startup error: `public_url is required when listening on all addresses or a Unix socket` | `listen` is `0.0.0.0`, `[::]`, or a Unix socket and `public_url` is unset | Set `public_url = "https://..."` with scheme, host, and optional port only. |
| Startup error: `relaying every session needs a TURN server` | `ice.force_relay = true` is set without a `turn:` or `turns:` URL | Add a TURN URL to `ice.servers` and set `ice.turn_secret_file`. |
| Log warning: `the socket systemd passed is not on listen` | `ListenStream` in `remora.socket` differs from `listen` in `remora.toml` | Align both addresses and run `systemctl daemon-reload && systemctl restart remora.socket`. |
| HTTP `429 Too Many Requests` on password sign-in | Password rate limit reached, or reverse proxy missing from `trusted_proxies` | Wait 60 seconds; verify `trusted_proxies` includes the proxy IP and `X-Forwarded-For` is forwarded. |
| QR login page shows expired or client refreshes QR repeatedly | 180-second QR lifetime elapsed, or proxy closed the 25-second long-poll early | Scan a fresh QR code; set the reverse proxy read timeout to at least 60 seconds. |
| Client receives HTTP `403` (`code: 1002`) | `sign_key_file` differs from client key, or proxy rewrote the request path | Match `UU_SIGN_KEY` with `sign.key` and forward request paths unchanged at the root. |
| OIDC sign-in returns to `?sign_in=failed` | Redirect URI mismatch, clock skew, or untrusted IdP TLS certificate | Register `{public_url}/api/web/oidc/callback` at the IdP, sync the system clock, and check the OS CA store. |
| Host appears online, but WebRTC session fails to connect | TURN secret mismatch, blocked UDP ports, or missing `external-ip` behind NAT | Match `turn.secret` with `static-auth-secret`, open `3478` and `49152-65535/udp`, and set `external-ip` in `turnserver.conf` when behind NAT. |
