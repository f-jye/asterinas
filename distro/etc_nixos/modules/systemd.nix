{
  config,
  lib,
  pkgs,
  ...
}:

{
  systemd.package = pkgs.aster_systemd;

  # FIXME: systemd-udevd fails at `Failed at step NAMESPACE spawning` because
  # the mount namespace setup that `PrivateMounts=yes` performs is not fully
  # supported by the kernel yet. Relax the sandbox until the kernel fix lands.
  # See HACKS.md (S10) for details.
  systemd.services.systemd-udevd = {
    serviceConfig = {
      PrivateMounts = lib.mkForce false;
      ProtectHostname = lib.mkForce false;
    };
  };

  # TODO: The following services currently do not work and
  # may affect systemd startup or cause performance issues.
  # Enable them after they can run successfully.
  systemd.coredump.enable = false;
  systemd.oomd.enable = false;
  systemd.services.logrotate.enable = false;
  systemd.services.network-setup.enable = false;
  systemd.services.resolvconf.enable = false;
  systemd.services.systemd-random-seed.enable = false;
  services.timesyncd.enable = false;
  # Real udev works on the kernel (uevent broadcast, SO_ATTACH_FILTER and the
  # graphics/input sysfs topology are all verified on real desktop images).
  services.udev.enable = true;

  services.getty.autologinUser = "root";
  services.getty.loginProgram = "${pkgs.util-linux.bin}/bin/login";
  users.users.root = {
    shell = "${pkgs.bash}/bin/bash";
    hashedPassword = null;
  };

  systemd.targets.getty.wants =
    # The kernel does not implement virtual terminals (VTs), so text
    # logins are only provided on the consoles that actually exist:
    # tty1 (when X is disabled) and the virtio console hvc0.
    (lib.optional (!config.services.xserver.enable) "autovt@tty1.service") ++ [
      "autovt@hvc0.service"
    ];

  systemd.settings.Manager = {
    LogLevel = "crit";
    ShowStatus = "no";
  };
}
