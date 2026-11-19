{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.remora;
  format = pkgs.formats.toml { };
  credentials = "/run/credentials/remora.service";

  signKeyFile = cfg.settings.sign_key_file;
  turnSecretFile = cfg.settings.ice.turn_secret_file;
  passwordFile = cfg.settings.login.password_file;
  oidcSecretFile = cfg.settings.oidc.client_secret_file or null;
  # Remora runs as a dynamic user, so secret files reach it as systemd credentials, and the
  # generated settings name those instead.
  secrets =
    lib.optional (signKeyFile != null) "sign-key:${signKeyFile}"
    ++ lib.optional (turnSecretFile != null) "turn-secret:${turnSecretFile}"
    ++ lib.optional (passwordFile != null) "owner-password:${passwordFile}"
    ++ lib.optional (oidcSecretFile != null) "oidc-secret:${oidcSecretFile}";
  settings =
    lib.recursiveUpdate cfg.settings {
      sign_key_file = if signKeyFile != null then "${credentials}/sign-key" else null;
      ice.turn_secret_file = if turnSecretFile != null then "${credentials}/turn-secret" else null;
      login.password_file = if passwordFile != null then "${credentials}/owner-password" else null;
    }
    // lib.optionalAttrs (cfg.settings.oidc != null) {
      oidc = cfg.settings.oidc // {
        client_secret_file = if oidcSecretFile != null then "${credentials}/oidc-secret" else null;
      };
    };
  # TOML has no null: unset settings are left out.
  configFile = format.generate "remora.toml" (
    lib.filterAttrsRecursive (_: value: value != null) settings
  );

  turnPort = toString config.services.coturn.listening-port;
  # Private and special-purpose ranges a relay must not reach on behalf of its clients.
  privatePeers = [
    "0.0.0.0-0.255.255.255"
    "10.0.0.0-10.255.255.255"
    "100.64.0.0-100.127.255.255"
    "127.0.0.0-127.255.255.255"
    "169.254.0.0-169.254.255.255"
    "172.16.0.0-172.31.255.255"
    "192.168.0.0-192.168.255.255"
    "::1"
    "fc00::-fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"
    "fe80::-febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff"
  ];
in
{
  options.services.remora = {
    enable = lib.mkEnableOption "Remora, a self-hostable stand-in for the UU Remote cloud";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The Remora package.";
    };

    settings = lib.mkOption {
      description = ''
        Remora's settings file. Secret files named here only need to be readable by root: they
        reach Remora as systemd credentials.
      '';
      default = { };
      type = lib.types.submodule {
        freeformType = format.type;
        options = {
          listen = lib.mkOption {
            type = lib.types.str;
            default = "127.0.0.1:8080";
            description = "Address to serve the REST API and signaling on.";
          };
          public_url = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            example = "https://remora.example";
            description = ''
              Origin clients reach Remora at, when that is not `http://` plus
              {option}`services.remora.settings.listen`. Required when listening on all
              addresses.
            '';
          };
          sign_key_file = lib.mkOption {
            type = lib.types.nullOr lib.types.path;
            default = null;
            example = "/run/secrets/remora-sign";
            description = ''
              File holding the request signing key that clients are configured with. Without
              it, request signatures are not checked, and anyone who can reach Remora can
              register devices and start logins.
            '';
          };
          database = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = "/var/lib/remora/remora.db";
            description = ''
              SQLite database for registered devices and the key that signs tokens. With
              `null`, they are kept in memory and lost on every restart.
            '';
          };
          login = lib.mkOption {
            default = { };
            description = "How QR logins get confirmed.";
            type = lib.types.submodule {
              freeformType = format.type;
              options = {
                password_file = lib.mkOption {
                  type = lib.types.nullOr lib.types.path;
                  default = null;
                  example = "/run/secrets/remora-owner";
                  description = ''
                    File holding the owner's password hash, as `remora hash-password` makes it.
                    The owner signs in with the password to confirm logins on the page their
                    QR code opens.
                  '';
                };
              };
            };
          };
          oidc = lib.mkOption {
            default = null;
            description = ''
              The OpenID Connect provider users sign in with. Register Remora there with the
              redirect URI `{public_url}/api/web/oidc/callback`.
            '';
            type = lib.types.nullOr (
              lib.types.submodule {
                freeformType = format.type;
                options = {
                  issuer = lib.mkOption {
                    type = lib.types.str;
                    example = "https://auth.example";
                    description = "The provider's issuer URL.";
                  };
                  client_id = lib.mkOption {
                    type = lib.types.str;
                    description = "Remora's client id at the provider.";
                  };
                  client_secret_file = lib.mkOption {
                    type = lib.types.nullOr lib.types.path;
                    default = null;
                    example = "/run/secrets/remora-oidc";
                    description = ''
                      File holding the client secret. Without it, Remora is a public client and
                      relies on PKCE.
                    '';
                  };
                };
              }
            );
          };
          ice = lib.mkOption {
            default = { };
            description = "How host and viewer reach each other.";
            type = lib.types.submodule {
              freeformType = format.type;
              options = {
                servers = lib.mkOption {
                  type = lib.types.listOf lib.types.str;
                  default = [ ];
                  example = [ "turn:turn.example:3478" ];
                  description = "STUN and TURN servers handed to both sides of each session.";
                };
                turn_secret_file = lib.mkOption {
                  type = lib.types.nullOr lib.types.path;
                  default = null;
                  example = "/run/secrets/remora-turn";
                  description = ''
                    File holding the secret shared with the TURN servers (coturn's
                    `static-auth-secret`).
                  '';
                };
              };
            };
          };
        };
      };
    };

    coturn = {
      enable = lib.mkEnableOption "a coturn relay on this machine, set up for Remora";

      domain = lib.mkOption {
        type = lib.types.str;
        example = "turn.example";
        description = "Name clients reach coturn at; also its realm.";
      };

      allowPrivatePeers = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Whether clients may relay to private and loopback addresses. Leave it off on a
          public server, so that the relay cannot reach the server's own networks.
        '';
      };

      openFirewall = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Whether to open coturn's listening port and relay ports.";
      };
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        systemd.services.remora = {
          description = "Remora, a stand-in for the UU Remote cloud";
          wantedBy = [ "multi-user.target" ];
          after = [ "network.target" ];
          serviceConfig = {
            ExecStart = "${lib.getExe cfg.package} --config ${configFile}";
            Restart = "on-failure";
            DynamicUser = true;
            StateDirectory = "remora";
            LoadCredential = secrets;

            CapabilityBoundingSet = "";
            LockPersonality = true;
            MemoryDenyWriteExecute = true;
            PrivateDevices = true;
            PrivateUsers = true;
            ProcSubset = "pid";
            ProtectClock = true;
            ProtectControlGroups = true;
            ProtectHome = true;
            ProtectHostname = true;
            ProtectKernelLogs = true;
            ProtectKernelModules = true;
            ProtectKernelTunables = true;
            ProtectProc = "invisible";
            RestrictAddressFamilies = [
              "AF_INET"
              "AF_INET6"
            ];
            RestrictNamespaces = true;
            RestrictRealtime = true;
            SystemCallArchitectures = "native";
            SystemCallFilter = [
              "@system-service"
              "~@privileged"
            ];
            UMask = "0077";
          };
        };
      }

      (lib.mkIf cfg.coturn.enable {
        assertions = [
          {
            assertion = turnSecretFile != null;
            message = "services.remora.coturn needs services.remora.settings.ice.turn_secret_file.";
          }
        ];

        services.remora.settings.ice.servers = lib.mkDefault [
          "stun:${cfg.coturn.domain}:${turnPort}"
          "turn:${cfg.coturn.domain}:${turnPort}"
        ];

        services.coturn = {
          enable = true;
          realm = cfg.coturn.domain;
          use-auth-secret = true;
          static-auth-secret-file = "/run/credentials/coturn.service/static-auth-secret";
          no-cli = true;
          extraConfig = ''
            fingerprint
            no-multicast-peers
          ''
          + lib.optionalString (!cfg.coturn.allowPrivatePeers) (
            lib.concatMapStrings (range: "denied-peer-ip=${range}\n") privatePeers
          );
        };
        # coturn reads its secret as its own user; the credential lets the file stay root's.
        systemd.services.coturn.serviceConfig.LoadCredential = [
          "static-auth-secret:${turnSecretFile}"
        ];

        networking.firewall = lib.mkIf cfg.coturn.openFirewall {
          allowedTCPPorts = [ config.services.coturn.listening-port ];
          allowedUDPPorts = [ config.services.coturn.listening-port ];
          allowedUDPPortRanges = [
            {
              from = config.services.coturn.min-port;
              to = config.services.coturn.max-port;
            }
          ];
        };
      })
    ]
  );
}
