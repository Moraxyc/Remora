# Runs remora-lab scenarios against the NixOS remora service (with coturn) and validates
# the standalone vs. role-split compatibility matrix.
{
  remoraModule,
  remoraPackage,
  labPackage,
}:

{ ... }:

let
  turnSecret = builtins.toFile "vm-turn-secret" "vm-compat-turn-secret\n";
  signKeyValue = "vm-compat-sign-key";
  signKey = builtins.toFile "vm-sign-key" "${signKeyValue}\n";
  compatScenarios = ../../compat/scenarios;
in
{
  name = "remora-compat";

  nodes = {
    server =
      { config, ... }:
      let
        address = config.networking.primaryIPAddress;
      in
      {
        imports = [ remoraModule ];

        services.remora = {
          enable = true;
          settings = {
            listen = "0.0.0.0:8680";
            public_url = "http://server:8680";
            sign_key_file = signKey;
            login.auto_confirm = true;
            ice.turn_secret_file = turnSecret;
          };
          coturn = {
            enable = true;
            domain = "server";
            allowPrivatePeers = true;
            openFirewall = true;
          };
        };
        services.coturn = {
          listening-ips = [ address ];
          relay-ips = [ address ];
        };
        networking.firewall.allowedTCPPorts = [ 8680 ];
      };

    client = {
      environment.systemPackages = [
        remoraPackage
        labPackage
      ];
    };
  };

  testScript =
    { nodes, ... }:
    let
      serverAddress = nodes.server.networking.primaryIPAddress;
    in
    ''
      import glob
      import json

      start_all()
      server.wait_for_unit("multi-user.target")
      server.wait_for_unit("remora.socket")
      server.wait_for_unit("coturn.service")
      server.wait_for_open_port(8680)
      server.wait_for_open_port(3478, "${serverAddress}")
      client.wait_for_unit("multi-user.target")

      cloud_files = sorted(glob.glob("${compatScenarios}/cloud/*.yaml"))
      with subtest("remora-lab runs cloud scenarios against the NixOS remora service"):
          client.succeed(
              "remora-lab run " + " ".join(cloud_files[:9]) + " "
              "--target url --url http://server:8680 --sign-key ${signKeyValue}"
          )
          server.succeed("systemctl restart remora.service")
          server.wait_for_open_port(8680)
          client.succeed(
              "remora-lab run " + " ".join(cloud_files[9:]) + " "
              "--target url --url http://server:8680 --sign-key ${signKeyValue}"
          )

      with subtest("remora-lab runs P2P and TURN-relayed session scenarios against NixOS remora + coturn"):
          server.succeed("systemctl restart remora.service")
          server.wait_for_open_port(8680)
          client.succeed(
              "remora-lab run ${compatScenarios}/session "
              "--target url --url http://server:8680 --sign-key ${signKeyValue}"
          )

      with subtest("remora-lab matrix passes both standalone and split columns with clean diff"):
          client.succeed(
              "remora-lab matrix ${compatScenarios} "
              "--remora-bin ${remoraPackage}/bin/remora "
              "--sign-key ${signKeyValue} "
              "--format both "
              "--out-json /tmp/matrix.json "
              "--out-md /tmp/matrix.md"
          )
          report = json.loads(client.succeed("cat /tmp/matrix.json"))
          assert report["all_passed"] is True, report
          assert [c["id"] for c in report["columns"]] == [
              "remora-standalone",
              "remora-split",
          ], report
          for row in report["rows"]:
              assert row["diff_identical"] is True, row
              assert all(cell["passed"] for cell in row["cells"]), row

      with subtest("no secret reaches the server log"):
          journal = server.succeed("journalctl -u remora.service -o cat")
          for secret in ["vm-compat-turn-secret", "${signKeyValue}"]:
              assert secret not in journal, "a secret was logged"
    '';
}
