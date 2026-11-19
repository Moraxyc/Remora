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
  # TOML has no null: unset settings are left out.
  settings = lib.filterAttrsRecursive (_: value: value != null) cfg.settings;
  configFile = format.generate "remora.toml" settings;

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
        Remora's settings file. Name secret files with
        {option}`services.remora.signKeyFile` and {option}`services.remora.turnSecretFile`
        instead of here.
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
          database = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = "/var/lib/remora/remora.db";
            description = ''
              SQLite database for registered devices and the key that signs tokens. With
              `null`, they are kept in memory and lost on every restart.
            '';
          };
          ice = lib.mkOption {
            default = { };
            description = "How host and viewer reach each other.";
            type = lib.types.submodule {
              freeformType = format.type;
              options.servers = lib.mkOption {
                type = lib.types.listOf lib.types.str;
                default = [ ];
                example = [ "turn:turn.example:3478" ];
                description = "STUN and TURN servers handed to both sides of each session.";
              };
            };
          };
        };
      };
    };

    signKeyFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        File holding the request signing key that clients are configured with. Without it,
        request signatures are not checked.
      '';
    };

    turnSecretFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        File holding the secret shared with the TURN servers (coturn's `static-auth-secret`).
        It only needs to be readable by root.
      '';
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
        services.remora.settings = {
          sign_key_file = lib.mkIf (cfg.signKeyFile != null) "${credentials}/sign-key";
          ice.turn_secret_file = lib.mkIf (cfg.turnSecretFile != null) "${credentials}/turn-secret";
        };

        systemd.services.remora = {
          description = "Remora, a stand-in for the UU Remote cloud";
          wantedBy = [ "multi-user.target" ];
          after = [ "network.target" ];
          serviceConfig = {
            ExecStart = "${lib.getExe cfg.package} --config ${configFile}";
            Restart = "on-failure";
            DynamicUser = true;
            StateDirectory = "remora";
            LoadCredential =
              lib.optional (cfg.signKeyFile != null) "sign-key:${cfg.signKeyFile}"
              ++ lib.optional (cfg.turnSecretFile != null) "turn-secret:${cfg.turnSecretFile}";

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
            assertion = cfg.turnSecretFile != null;
            message = "services.remora.coturn needs services.remora.turnSecretFile.";
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
          "static-auth-secret:${cfg.turnSecretFile}"
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
