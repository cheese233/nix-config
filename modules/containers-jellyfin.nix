{ config, lib, pkgs, inputs, ... }:

let
  pkgsUnstable = import inputs.nixpkgs-unstable {
    system = pkgs.stdenv.hostPlatform.system;
    config.allowUnfree = true;
  };

  mdnsPublisher = inputs.mdns-publisher.packages.x86_64-linux.default;

  mkPodmanVeth = import ../modules/podman-veth.nix { inherit pkgs; };

  veth = mkPodmanVeth {
    name   = "jellyfin";
    bridge = "br-lan";
    mac    = "02:00:00:00:00:03";
  };

  jellyfinImage = pkgs.dockerTools.streamLayeredImage {
    name = "jellyfin";
    tag = "latest";
    maxLayers = 20;

    contents = with pkgsUnstable; [
      jellyfin
      jellyfin-web
      jellyfin-ffmpeg
      intel-media-driver
      intel-vaapi-driver
      intel-compute-runtime
      vpl-gpu-rt
      fontconfig
      freetype
      sqlite
      bash
    ] ++ [ mdnsPublisher ];

    config = {
      Cmd = [
        "/bin/sh" "-e" "-c"
        ''
          ${mdnsPublisher}/bin/mdns-publisher \
            -iface eth0 -hostname jellyfin &
          exec ${pkgsUnstable.jellyfin}/bin/jellyfin \
            --ffmpeg ${pkgsUnstable.jellyfin-ffmpeg}/bin/ffmpeg \
            --webdir ${pkgsUnstable.jellyfin-web}/share/jellyfin-web \
            --datadir /config \
            --cachedir /cache \
            --logdir /config/log
        ''
      ];
      Env = [
        "TZ=Asia/Shanghai"
        "LIBVA_DRIVER_NAME=iHD"
      ];
      ExposedPorts = {
        "8096/tcp" = {};
        "8920/tcp" = {};
      };
    };

    fakeRootCommands = ''
      mkdir -p /config /cache /config/log
      chmod 755 /config /cache
    '';
  };
in
{
  systemd.services = veth.services;

  virtualisation.oci-containers = {
    backend = "podman";
    containers.jellyfin = {
      image = "jellyfin:latest";
      imageStream = jellyfinImage;
      autoStart = true;

      volumes = [
        "/var/lib/jellyfin/config:/config"
        "/var/lib/jellyfin/cache:/cache"
        "/mnt/HDD:/media"
      ];

      environment = {
        TZ = "Asia/Shanghai";
        LIBVA_DRIVER_NAME = "iHD";
      };

      dependsOn = [ "podman-veth-jellyfin" ];

      extraOptions = [
        "--network=${veth.arg}"
        "--hostname=jellyfin"
        "--device=/dev/dri/renderD128:/dev/dri/renderD128"
        "--device=/dev/dri/card0:/dev/dri/card0"
      ];
    };
  };
}
