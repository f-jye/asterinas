final: prev: {
  xorg-server = prev.xorg-server.overrideAttrs (oldAttrs: {
    mesonFlags = (oldAttrs.mesonFlags or [ ]) ++ [
      "-Dglamor=true"
      "-Doptimization=0"
    ];
  });

  xf86-video-fbdev = prev.xf86-video-fbdev.overrideAttrs (oldAttrs: {
    # The driver loads its helper modules after dlopen.
    # See https://github.com/NixOS/nixpkgs/pull/545344.
    hardeningDisable = (oldAttrs.hardeningDisable or [ ]) ++ [ "bindnow" ];
  });

  xfdesktop = prev.xfdesktop.overrideAttrs (oldAttrs: {
    patches = (oldAttrs.patches or [ ]) ++ [
      ./patches/xfdesktop4/0001-Fix-not-using-consistent-monitor-identifiers.patch
    ];
  });
}
