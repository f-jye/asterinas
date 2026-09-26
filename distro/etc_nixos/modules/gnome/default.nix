{
  config,
  lib,
  pkgs,
  ...
}:
let
  startGnome = pkgs.writeScriptBin "start_gnome" (builtins.readFile ./start_gnome.sh);

  # The system bus denies owning any name by default; punch the hole for
  # the login1 stub. Filed under services.dbus.packages so the dbus module
  # merges it through its <includedir> hooks.
  dbusLogin1Policy = pkgs.runCommand "dbus-login1-policy" { } ''
    mkdir -p $out/etc/dbus-1/system.d
    cat > $out/etc/dbus-1/system.d/10-login1-stub.conf <<'EOF'
    <!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
      "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
    <busconfig>
      <policy user="root">
        <allow own="org.freedesktop.login1"/>
      </policy>
      <policy context="default">
        <allow send_destination="org.freedesktop.login1"/>
      </policy>
    </busconfig>
    EOF
  '';

  # Minimal org.freedesktop.login1: owns the bus name and hands out device
  # fds, the piece of logind that mutter's native backend requires.
  login1Stub = pkgs.stdenv.mkDerivation {
    name = "login1-stub";
    src = ./.;
    buildInputs = [ pkgs.systemd ];
    dontConfigure = true;
    buildPhase = ''
      cc -O2 -Wall -o login1-stub $src/login1-stub.c \
        -I${pkgs.systemd.dev}/include/systemd \
        -L${pkgs.systemd}/lib -lsystemd
    '';
    installPhase = ''
      mkdir -p $out/bin
      cp login1-stub $out/bin/login1-stub
    '';
  };

  # SCM_RIGHTS regression tests, run by hand over the console to pin down
  # the TakeDevice fd loss (see HACKS.md).
  fdTest = pkgs.stdenv.mkDerivation {
    name = "fdtest";
    src = ./.;
    dontConfigure = true;
    buildPhase = ''
      cc -O2 -Wall -o fdtest $src/fdtest.c
    '';
    installPhase = ''
      mkdir -p $out/bin
      cp fdtest $out/bin/fdtest
    '';
  };

  # A minimal GDBus TakeDevice client, the mutter-shaped probe for the
  # same investigation.
  takeDevTest = pkgs.stdenv.mkDerivation {
    name = "takedevtest";
    src = ./.;
    dontConfigure = true;
    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = [ pkgs.glib ];
    buildPhase = ''
      cc -O2 -Wall -o takedevtest $src/takedevtest.c \
        $(pkg-config --cflags --libs gio-2.0)
    '';
    installPhase = ''
      mkdir -p $out/bin
      cp takedevtest $out/bin/takedevtest
    '';
  };

  # The GNOME Flashback session runtime: gnome-flashback and gnome-panel
  # wrapped with the gsettings schemas and panel module paths they need.
  # (systemd-minimal, the overlaid systemd, has no user session, so the
  # components are started directly by start_gnome instead of through
  # gnome-session's systemd integration.)
  sessionRuntime = pkgs.buildEnv {
    name = "gnome-flashback-runtime";
    paths = [
      gnome-flashback-wrapped
      gnome-panel-wrapped
    ];
  };

  gnome-flashback-wrapped = pkgs.runCommand "gnome-flashback-wrapped"
    {
      nativeBuildInputs = [ pkgs.makeWrapper ];
    }
    ''
      mkdir -p $out/bin
      makeWrapper ${pkgs.gnome-flashback}/bin/gnome-flashback $out/bin/gnome-flashback \
        --prefix XDG_DATA_DIRS : ${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name} \
        --prefix XDG_DATA_DIRS : ${pkgs.gnome-settings-daemon}/share/gsettings-schemas/${pkgs.gnome-settings-daemon.name} \
        --prefix XDG_DATA_DIRS : ${pkgs.gnome-desktop}/share
    '';

  gnome-panel-wrapped = pkgs.runCommand "gnome-panel-wrapped"
    {
      nativeBuildInputs = [ pkgs.makeWrapper ];
    }
    ''
      mkdir -p $out/bin
      makeWrapper ${pkgs.gnome-panel}/bin/gnome-panel $out/bin/gnome-panel \
        --prefix XDG_DATA_DIRS : ${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name} \
        --set NIX_GNOME_PANEL_MODULESDIR ${pkgs.gnome-panel}/lib/gnome-panel/modules
    '';
in
{
  options.services.xserver.desktopManager.gnome-flashback.enable = lib.mkEnableOption ''
    the GNOME Flashback desktop environment (X11, metacity window manager)'';

  config = lib.mkIf (config.services.xserver.enable && config.services.xserver.desktopManager.gnome-flashback.enable) {
    services.dbus.packages = [ dbusLogin1Policy ];

    # dconf commits settings through a D-Bus-activated systemd user unit;
    # systemd-minimal ships no user units, so register the shipped one
    # through the NixOS unit generator (into /etc/systemd/user).
    systemd.user.units."dconf.service" = {
      enable = true;
      text = ''
        [Unit]
        Description=User preferences database
        Documentation=man:dconf-service(1)

        [Service]
        ExecStart=${pkgs.dconf.lib or pkgs.dconf}/libexec/dconf-service
        Type=dbus
        BusName=ca.desrt.dconf
      '';
    };

    environment.systemPackages = [
      startGnome
      sessionRuntime
      fdTest
      takeDevTest
      pkgs.metacity
      pkgs.nautilus
      pkgs.gnome-terminal
      pkgs.gsettings-desktop-schemas
      pkgs.gnome-settings-daemon
      pkgs.dconf
      pkgs.dbus
      pkgs.xkeyboard-config
      # The Wayland session: mutter/gnome-shell with software rendering.
      pkgs.mutter
      pkgs.gnome-shell
      pkgs.mesa
    ];

    systemd.services."login1-stub" = {
      description = "Minimal org.freedesktop.login1 stub";
      after = [ "dbus.service" ];
      wants = [ "dbus.service" ];
      before = [ "gnome-wayland.service" ];
      wantedBy = [ "gnome-wayland.service" ];
      unitConfig.StartLimitIntervalSec = 0;
      serviceConfig = {
        ExecStart = "${login1Stub}/bin/login1-stub";
        Restart = "always";
        RestartSec = "1s";
      };
    };

    systemd.services."gnome-wayland" = {
      description = "GNOME Shell on Wayland";
      wantedBy = [ "multi-user.target" ];
      conflicts = [ "gnome-desktop.service" "getty@tty1.service" ];
      # The coldplug replay populates the udev database mutter's GPU
      # discovery reads; it must be done before the shell starts.
      after = [ "login1-stub.service" "aster-input-coldplug.service" ];
      wants = [ "login1-stub.service" ];
      unitConfig.StartLimitIntervalSec = 0;
      serviceConfig = {
        # gnome-shell is a long-running compositor and stays the main PID;
        # a oneshot start job here never completed and timed out, killing
        # the shell after TimeoutStartSec.
        Type = "simple";
        ExecStart = pkgs.writeShellScript "start-gnome-wayland" ''
          export XDG_SESSION_TYPE=wayland
          export XDG_CURRENT_DESKTOP=GNOME
          export XDG_RUNTIME_DIR=/run/user/0
          mkdir -p /run/user/0 /run/systemd/sessions /run/systemd/users
          chmod 700 /run/user/0
          # Emulate the logind state files (there is no real logind yet).
          # mutter resolves its session through XDG_SESSION_ID and the
          # sd-login files; the exact keys matter: ACTIVE=1 is what
          # sd_session_is_active checks, SEAT= what sd_session_get_seat
          # reads, DISPLAY= in users/0 what sd_uid_get_display falls back
          # to. Without all of these mutter refuses to find the session.
          mkdir -p /run/systemd/seats
          printf 'NAME=root\nSTATE=active\nACTIVE=1\nTYPE=wayland\nCLASS=user\nUID=0\nSEAT=seat0\nLEADER=%s\n' "$$" > /run/systemd/sessions/c1
          printf 'NAME=root\nSTATE=active\nTYPE=wayland\nCLASS=user\nUID=0\nDISPLAY=c1\nSESSIONS=c1\n' > /run/systemd/users/0
          printf 'STATE=active\nACTIVE_SESSION=c1\nSESSIONS=c1\n' > /run/systemd/seats/seat0
          export XDG_SESSION_ID=c1
          # Session bus; drop a stale bus from a previous run first.
          rm -f /run/user/0/bus
          ${pkgs.dbus}/bin/dbus-daemon --session --fork --address=unix:path=/run/user/0/bus
          export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus
          # Shell output goes straight to a file: the guest's journald is
          # unreliable, and this log is what the desktop bring-up reads.
          exec ${pkgs.gnome-shell}/bin/gnome-shell --wayland \
            >>/tmp/mutter.log 2>&1
        '';
        Restart = "on-failure";
        RestartSec = "2s";
        # systemd normally binds fd 0 to /dev/null; the minimal systemd
        # does not, leaving fd 0 free. Received SCM_RIGHTS fds then land on
        # fd 0, which GLib's fd list later closes twice (EBADF) and mutter
        # loses the DRM device.
        StandardInput = "null";
        StandardOutput = "null";
        StandardError = "null";
      };
    };

    # X11 fallback session; not pulled in by the boot (the Wayland session
    # above is the default), start manually with `systemctl start`.
    systemd.services."gnome-desktop" = {
      description = "GNOME Flashback Desktop Environment";
      conflicts = [ "gnome-wayland.service" "getty@tty1.service" ];
      serviceConfig = {
        Environment = "DISPLAY=:0";
        ExecStart = "${startGnome}/bin/start_gnome";
        StandardOutput = "tty";
        StandardError = "tty";
        KillMode = "process";
        Delegate = "yes";
        Restart = "no";
        Type = "simple";
      };
    };
  };
}
