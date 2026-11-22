# End-to-end interoperability test: upstream uu-remote-linux (host + viewer) and remora-lab
# against Remora and coturn in NixOS VMs, producing the 3-column session compatibility matrix.
{
  remoraModule,
  labPackage,
  uuRemoteModule,
  uuRemotePackage,
}:

{ pkgs, ... }:

let
  turnSecret = pkgs.writeText "turn-secret" "vm-turn-secret\n";
  signKeyValue = "vm-sign-key";
  signKey = pkgs.writeText "sign-key" "${signKeyValue}\n";
  sessionScenarios = ../../compat/scenarios/session;

  swayConfig = pkgs.writeText "sway-headless.conf" ''
    xwayland disable
    exec sh -c 'systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE && systemctl --user start test-wayland-session.target'
  '';

  assistDriver = pkgs.writeText "assist-driver.py" ''
    import fcntl, json, os, pty, re, select, subprocess, termios, time

    os.environ["UU_BASE_URL"] = "http://server:8680"
    os.environ["UU_SIGN_KEY"] = "${signKeyValue}"
    os.environ["UU_CONFIG"] = "/tmp/assist-config.json"
    os.environ["UU_ASSIST_UI"] = "tty"
    os.environ["UU_ASSIST_ALLOW"] = "1"
    os.environ["UU_LANG"] = "zh"
    os.environ["UU_DESKTOP_BACKEND"] = "wayland"
    os.environ["WAYLAND_DISPLAY"] = os.environ.get("WAYLAND_DISPLAY", "wayland-1")

    master, slave = pty.openpty()

    def preexec():
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

    proc = subprocess.Popen(
        ["uuremote-server", "assist"],
        stdin=slave,
        stdout=slave,
        stderr=subprocess.PIPE,
        preexec_fn=preexec,
        close_fds=True,
    )
    os.close(slave)

    buf = ""
    connect_id = None
    connect_code = None
    deadline = time.time() + 30
    while time.time() < deadline:
        r, _, _ = select.select([master], [], [], 0.2)
        if master in r:
            try:
                chunk = os.read(master, 4096).decode("utf-8", errors="replace")
            except OSError:
                break
            if not chunk:
                break
            buf += chunk
            m_id = re.search(r"设备 ID:\s*([0-9 ]+)", buf)
            m_code = re.search(r"临时验证码:\s*([A-Za-z0-9]+)", buf)
            if m_id and m_code:
                connect_id = m_id.group(1).replace(" ", "")
                connect_code = m_code.group(1).strip()
                break
        if proc.poll() is not None:
            break

    if not connect_id or not connect_code:
        rc = proc.poll()
        if rc is None:
            proc.kill()
            rc = proc.wait(timeout=5)
        err = proc.stderr.read().decode("utf-8", errors="replace")
        raise RuntimeError(
            f"uuremote-server assist failed (rc={rc}); tty={buf!r}; stderr={err!r}"
        )

    fd = os.open("/tmp/assist-ready.json.tmp", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump({"connect_id": connect_id, "code": connect_code}, f)
    os.rename("/tmp/assist-ready.json.tmp", "/tmp/assist-ready.json")

    while not os.path.exists("/tmp/assist-stop"):
        r, _, _ = select.select([master], [], [], 0.2)
        if master in r:
            try:
                os.read(master, 4096)
            except OSError:
                break
        if proc.poll() is not None:
            break

    try:
        os.write(master, b"off\n")
    except OSError:
        pass
    rc = proc.wait(timeout=15)
    os.close(master)
    if rc != 0:
        err = proc.stderr.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"uuremote-server assist exited {rc}: {err!r}")
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
              force_relay = false;
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

      environment.systemPackages = [
        pkgs.python3
        pkgs.sway-unwrapped
      ];
    };

    viewer = {
      environment.systemPackages = [
        uuRemotePackage
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

      with subtest("viewer transfers files with host over WebRTC"):
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

      with subtest("3-column session matrix passes lab<->lab (P2P), lab<->lab (relay), and lab<->uu-remote-linux"):
          viewer.succeed(
              f"remora-lab matrix ${sessionScenarios} "
              "--mode session "
              "--url http://server:8680 "
              "--sign-key ${signKeyValue} "
              f"--uu-device-id {host_device_id} "
              "--uu-force-relay "
              "--format both "
              "--out-json /tmp/interop-matrix.json "
              "--out-md /tmp/interop-matrix.md"
          )
          report = json.loads(viewer.succeed("cat /tmp/interop-matrix.json"))
          assert report["all_passed"] is True, report
          assert [c["id"] for c in report["columns"]] == [
              "lab-lab-p2p",
              "lab-lab-relay",
              "lab-uu-remote-linux",
          ], report
          for row in report["rows"]:
              assert all(cell["passed"] for cell in row["cells"]), row

      with subtest("uuremote-viewer connects to remora-lab Host and injects input"):
          viewer.succeed(
              "rm -f /tmp/lab-host.json /tmp/lab-host.rc && "
              "systemd-run --unit=lab-host --quiet "
              "sh -c '${labPackage}/bin/remora-lab serve-host "
              "--url http://server:8680 "
              "--sign-key ${signKeyValue} "
              "--ready-file /tmp/lab-host.json "
              "--force-relay "
              "--expect-inputs 2 "
              "--expect-frames 0 "
              "--timeout-secs 30; echo $? > /tmp/lab-host.rc'"
          )
          viewer.wait_until_succeeds("test -f /tmp/lab-host.json")
          lab_device_id = json.loads(viewer.succeed("cat /tmp/lab-host.json"))["device_id"]
          viewer.succeed(f"printf 'ab' | {env_prefix} uuremote-viewer type {lab_device_id} --delay-ms 50 --new-session")
          viewer.wait_until_succeeds("test -f /tmp/lab-host.rc")
          assert viewer.succeed("cat /tmp/lab-host.rc").strip() == "0", viewer.succeed("journalctl -u lab-host.service -o cat")

      with subtest("viewer unbinds the host and Remora drops it"):
          viewer.succeed(f"{env_prefix} uuremote-viewer unbind {host_device_id}")
          raw_after = viewer.succeed(
              f"{env_prefix} uuremote-viewer devices GET /data/my_binded_devices /device_id /name"
          )
          devices_after = [json.loads(line) for line in raw_after.splitlines() if line.strip()]
          assert not any(d["device"] == host_device_id for d in devices_after), devices_after

      with subtest("remora-lab assists uuremote-server guest mode over TURN"):
          host.succeed(as_alice("systemctl --user stop uuremote-native.service"))
          host.succeed(
              "rm -f /tmp/assist-config.json /tmp/assist-ready.json /tmp/assist-stop /tmp/assist-driver.rc && "
              + as_alice(
                  'systemd-run --user --unit=uu-assist-test --quiet '
                  'sh -c "python3 ${assistDriver}; echo \\$? > /tmp/assist-driver.rc"'
              )
          )
          host.wait_until_succeeds("test -f /tmp/assist-ready.json || test -f /tmp/assist-driver.rc")
          if host.succeed("test -f /tmp/assist-driver.rc && echo failed || echo ok").strip() == "failed":
              raise RuntimeError(
                  "uu-assist-test exited prematurely: "
                  + host.succeed(as_alice("journalctl --user -u uu-assist-test.service -o cat"))
              )
          assist_info = json.loads(host.succeed("cat /tmp/assist-ready.json"))
          assist_id = assist_info["connect_id"]
          assist_code = assist_info["code"]

          viewer.succeed(
              "remora-lab assist-join "
              "--url http://server:8680 "
              "--sign-key ${signKeyValue} "
              f"--connect-id {assist_id} "
              "--force-relay "
              "--expect-frames 1 "
              "--timeout-secs 30 "
              f"<<'EOF'\n{assist_code}\nEOF"
          )

          host.succeed("touch /tmp/assist-stop")
          host.wait_until_succeeds("test -f /tmp/assist-driver.rc")
          assert host.succeed("cat /tmp/assist-driver.rc").strip() == "0", host.succeed(
              as_alice("journalctl --user -u uu-assist-test.service -o cat")
          )

      with subtest("no secret reaches the server log"):
          journal = server.succeed("journalctl -u remora.service -o cat")
          for secret in ["vm-turn-secret", "${signKeyValue}", assist_code]:
              assert secret not in journal, "a secret was logged"
    '';
}
