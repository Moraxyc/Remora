# End-to-end interoperability test: upstream uu-remote-linux (host + viewer) against Remora
# and coturn in NixOS VMs.
{
  remoraModule,
  uuRemoteModule,
  uuRemotePackage,
}:

{ pkgs, ... }:

let
  turnSecret = pkgs.writeText "turn-secret" "vm-turn-secret\n";
  signKeyValue = "vm-sign-key";
  signKey = pkgs.writeText "sign-key" "${signKeyValue}\n";

  swayConfig = pkgs.writeText "sway-headless.conf" ''
    xwayland disable
    exec sh -c 'systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE && systemctl --user start test-wayland-session.target'
  '';
in
{
  name = "remora-interop-linux";

  node.pkgsReadOnly = false;

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
            ice = {
              turn_secret_file = turnSecret;
              force_relay = true;
            };
            login.auto_confirm = true;
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

    host = {
      imports = [ uuRemoteModule ];

      users.users.alice = {
        isNormalUser = true;
        uid = 1000;
      };

      services.getty.autologinUser = "alice";

      services.uu-remote-linux = {
        enable = true;
        uinput.enable = true;
        settings = {
          UU_DESKTOP_BACKEND = "wayland";
          UU_LOGIN_WINDOW = "0";
          UU_REMOTE_ASSIST = "0";
          UU_BASE_URL = "http://server:8680";
          UU_SIGN_KEY = signKeyValue;
        };
      };

      systemd.user.targets.test-wayland-session.unitConfig = {
        Wants = [ "graphical-session.target" ];
        Before = [ "graphical-session.target" ];
      };

      environment.systemPackages = [ pkgs.sway-unwrapped ];
    };

    viewer = {
      environment.systemPackages = [ uuRemotePackage ];
    };
  };

  testScript =
    { nodes, ... }:
    let
      serverAddress = nodes.server.networking.primaryIPAddress;
    in
    ''
      import json

      start_all()
      server.wait_for_unit("remora.socket")
      server.wait_for_unit("coturn.service")
      server.wait_for_open_port(8680)
      server.wait_for_open_port(3478, "${serverAddress}")

      host.wait_for_unit("multi-user.target")
      host.wait_for_unit("user@1000.service")
      viewer.wait_for_unit("multi-user.target")

      env_prefix = "UU_BASE_URL=http://server:8680 UU_SIGN_KEY=${signKeyValue}"

      def as_alice(cmd):
          return (
              "su - alice -c "
              "'export XDG_RUNTIME_DIR=/run/user/1000 "
              "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus "
              f"{env_prefix}; {cmd}'"
          )

      with subtest("host registers and signs in with uuremote-server login --no-gui"):
          host.succeed(as_alice("uuremote-server login --no-gui"))
          host_cfg = json.loads(host.succeed(as_alice("cat ~/.config/uuremote/config.json")))
          host_device_id = host_cfg["device_id"]
          assert host_device_id, host_cfg

      with subtest("host starts uuremote-native.service and comes online on Remora"):
          host.succeed(
              as_alice(
                  "systemd-run --user --unit=test-sway "
                  "-p Environment=WLR_BACKENDS=headless "
                  "-p Environment=WLR_RENDERER=pixman "
                  "-p Environment=WLR_LIBINPUT_NO_DEVICES=1 "
                  "-p Environment=XDG_CURRENT_DESKTOP=sway "
                  "-p Environment=XDG_SESSION_TYPE=wayland "
                  "sway -c ${swayConfig}"
              )
          )
          host.wait_until_succeeds(as_alice("systemctl --user is-active uuremote-native.service"))
          server.wait_until_succeeds(
              f"journalctl -u remora.service -o cat | grep -q 'signaling connected.*{host_device_id}.*Host'"
          )

      with subtest("viewer signs in and discovers the host in device list"):
          viewer.succeed(f"{env_prefix} uuremote-viewer login --no-gui")
          raw_devices = viewer.succeed(
              f"{env_prefix} uuremote-viewer devices GET /data/my_binded_devices /device_id /name"
          )
          devices = [json.loads(line) for line in raw_devices.splitlines() if line.strip()]
          assert any(d["device"] == host_device_id for d in devices), devices

      with subtest("viewer transfers files with host over WebRTC relayed through coturn"):
          viewer.succeed("printf 'hello-from-viewer-over-remora\n' > /tmp/viewer-hello.txt")
          viewer.succeed(
              f"{env_prefix} uuremote-viewer files {host_device_id} put /tmp/viewer-hello.txt /tmp/host-received.txt"
          )
          assert host.succeed("cat /tmp/host-received.txt") == "hello-from-viewer-over-remora\n"

          listing = viewer.succeed(f"{env_prefix} uuremote-viewer files {host_device_id} ls /tmp")
          assert "host-received.txt" in listing, listing

          viewer.succeed(
              f"{env_prefix} uuremote-viewer files {host_device_id} get /tmp/host-received.txt /tmp/viewer-got.txt"
          )
          assert viewer.succeed("cat /tmp/viewer-got.txt") == "hello-from-viewer-over-remora\n"

      with subtest("viewer unbinds the host and Remora drops it"):
          viewer.succeed(f"{env_prefix} uuremote-viewer unbind {host_device_id}")
          raw_after = viewer.succeed(
              f"{env_prefix} uuremote-viewer devices GET /data/my_binded_devices /device_id /name"
          )
          devices_after = [json.loads(line) for line in raw_after.splitlines() if line.strip()]
          assert not any(d["device"] == host_device_id for d in devices_after), devices_after

      with subtest("no secret reaches the server log"):
          journal = server.succeed("journalctl -u remora.service -o cat")
          for secret in ["vm-turn-secret", "${signKeyValue}"]:
              assert secret not in journal, "a secret was logged"
    '';
}
