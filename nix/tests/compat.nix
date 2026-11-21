# Runs remora-lab scenarios against the NixOS remora service and validates the standalone
# vs. role-split compatibility matrix.
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
    server = {
      imports = [ remoraModule ];

      services.remora = {
        enable = true;
        settings = {
          listen = "0.0.0.0:8680";
          public_url = "http://server:8680";
          sign_key_file = signKey;
          login.auto_confirm = true;
          ice = {
            servers = [ "turn:server:3478" ];
            turn_secret_file = turnSecret;
          };
        };
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

  testScript = ''
    import json

    start_all()
    server.wait_for_unit("multi-user.target")
    server.wait_for_unit("remora.socket")
    server.wait_for_open_port(8680)
    client.wait_for_unit("multi-user.target")

    with subtest("remora-lab runs all scenarios against the NixOS remora service"):
        client.succeed(
            "remora-lab run ${compatScenarios} "
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
