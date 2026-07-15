{ config, lib, pkgs, inputs, ... }:

let
  mdnsPublisher = inputs.mdns-publisher.packages.x86_64-linux.default;

  mkPodmanVeth = import ../modules/podman-veth.nix { inherit pkgs; };

  veth = mkPodmanVeth {
    name   = "jellyfin";
    bridge = "br-lan";
    mac    = "02:00:00:00:00:03";
  };

  # Official Jellyfin image
  jellyfinImage = pkgs.dockerTools.pullImage {
    imageName = "ghcr.io/jellyfin/jellyfin";
    imageDigest = "sha256:ddf59965ae63fccc66dfe72384495df355c67ea5abd9cc04eb65988f9a6010d1";
    sha256 = "sha256-Vf8xcu7tiIXeoIwmLio7q8Dwih/luD9RsXKkYZhLZqo=";
    finalImageTag = "latest";
  };
in
{
  systemd.services = veth.services // {
    "${config.virtualisation.oci-containers.containers.jellyfin.serviceName}".serviceConfig.StateDirectory = "jellyfin";
  };

  systemd.tmpfiles.rules = [
    "d /var/lib/jellyfin/config 0755 root root -"
    "d /var/lib/jellyfin/cache 0755 root root -"
  ];

  virtualisation.oci-containers.containers.jellyfin = {
      image = "ghcr.io/jellyfin/jellyfin:latest";
      imageFile = jellyfinImage;
      autoStart = true;

      volumes = [
        "/var/lib/jellyfin/config:/config"
        "/var/lib/jellyfin/cache:/cache"
        "/mnt/HDD:/media"
      ];

      environment = {
        TZ = "Asia/Shanghai";
      };

      extraOptions = [
        "--network=${veth.arg}"
        "--hostname=jellyfin"
        "--tmpfs=/tmp"
        "--cap-drop=ALL"
        "--security-opt=no-new-privileges:true"
        "--device=/dev/dri/renderD128:/dev/dri/renderD128"
        "--device=/dev/dri/card0:/dev/dri/card0"
      ];
    };
}
