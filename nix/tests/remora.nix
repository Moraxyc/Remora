# Remora and coturn from the NixOS module on one machine; a client on another plays host and
# viewer, then relays traffic through coturn with the credentials Remora handed out.
{ module }:

{ pkgs, ... }:

let
  turnSecret = pkgs.writeText "turn-secret" "vm-turn-secret\n";
  signKey = pkgs.writeText "sign-key" "vm-sign-key\n";
in
{
  name = "remora";

  nodes = {
    server =
      { config, ... }:
      let
        address = config.networking.primaryIPAddress;
      in
      {
        imports = [ module ];

        services.remora = {
          enable = true;
          settings = {
            listen = "0.0.0.0:8080";
            public_url = "http://server:8080";
            ice.force_relay = true;
            login.auto_confirm = true;
          };
          signKeyFile = signKey;
          turnSecretFile = turnSecret;
          coturn = {
            enable = true;
            domain = "server";
            # Both test machines are on a private network.
            allowPrivatePeers = true;
            openFirewall = true;
          };
        };
        services.coturn = {
          listening-ips = [ address ];
          relay-ips = [ address ];
        };
        networking.firewall.allowedTCPPorts = [ 8080 ];
      };

    client = {
      environment.systemPackages = [
        pkgs.coturn
        (pkgs.python3.withPackages (python: [ python.websockets ]))
      ];
    };
  };

  testScript =
    { nodes, ... }:
    let
      serverAddress = nodes.server.networking.primaryIPAddress;
    in
    ''
      import json
      import shlex

      start_all()
      server.wait_for_unit("remora.service")
      server.wait_for_unit("coturn.service")
      server.wait_for_open_port(8080)
      server.wait_for_open_port(3478, "${serverAddress}")

      with subtest("a host and a viewer start a session"):
          grant = json.loads(client.succeed(
              "python3 ${./session.py} http://server:8080 ${signKey}"
          ))
          assert grant["force_relay"], grant
          assert grant["urls"] == ["turn:server:3478"], grant

      with subtest("coturn accepts the credentials Remora handed out"):
          user = shlex.quote(grant["username"])
          password = shlex.quote(grant["credential"])
          report = client.succeed(
              f"turnutils_uclient -y -n 10 -u {user} -w {password} ${serverAddress}"
          )
          assert "Total lost packets 0 " in report, report
          client.fail(
              f"turnutils_uclient -y -n 10 -u {user} -w wrong ${serverAddress}"
          )

      with subtest("no secret reaches the log"):
          journal = server.succeed("journalctl -u remora.service -o cat")
          for secret in ["vm-turn-secret", "vm-sign-key", grant["credential"]]:
              assert secret not in journal, "a secret was logged"
    '';
}
