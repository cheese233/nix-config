{ config, lib, pkgs, inputs, ... }:

let
  mdnsPublisher = inputs.mdns-publisher.packages.x86_64-linux.default;

  mkPodmanVeth = import ../modules/podman-veth.nix { inherit pkgs; };

  veth = mkPodmanVeth {
    name   = "jellyfin";
    bridge = "br-lan";
    mac    = "02:00:00:00:00:03";
  };

  nixBaseImage = pkgs.dockerTools.pullImage {
    imageName = "ghcr.io/nixos/nix";
    imageDigest = "sha256:d78540374f6a886653cba47d5c3f61c5a41d42e2a8db2607b8d68cb226fd463e";
    sha256 = "sha256-QOJlic/KKUNyCP/+NdOJmbukJwjWiiykqQTrEeLeQd4=";
    finalImageTag = "latest";
  };

  networkXml = pkgs.writeText "network.xml" ''
    <?xml version="1.0" encoding="utf-8"?>
    <NetworkConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
      <BaseUrl />
      <EnableHttps>false</EnableHttps>
      <RequireHttps>false</RequireHttps>
      <InternalHttpPort>8096</InternalHttpPort>
      <InternalHttpsPort>8920</InternalHttpsPort>
      <PublicHttpPort>8096</PublicHttpPort>
      <PublicHttpsPort>8920</PublicHttpsPort>
      <AutoDiscovery>true</AutoDiscovery>
      <EnableUPnP>false</EnableUPnP>
      <EnableIPv4>true</EnableIPv4>
      <EnableIPv6>true</EnableIPv6>
      <EnableRemoteAccess>true</EnableRemoteAccess>
      <LocalNetworkSubnets />
      <LocalNetworkAddresses />
      <KnownProxies />
      <IgnoreVirtualInterfaces>true</IgnoreVirtualInterfaces>
      <VirtualInterfaceNames>
        <string>veth</string>
      </VirtualInterfaceNames>
      <EnablePublishedServerUriByRequest>false</EnablePublishedServerUriByRequest>
      <PublishedServerUriBySubnet />
      <RemoteIPFilter />
      <IsRemoteIPFilterBlacklist>false</IsRemoteIPFilterBlacklist>
    </NetworkConfiguration>
  '';

  jellyfinImage = pkgs.dockerTools.buildImage {
    name = "jellyfin";
    tag = "latest";
    fromImage = nixBaseImage;

    contents = with pkgs; [
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

    runAsRoot = ''
      # Create default config directories and seed network.xml
      mkdir -p /config/config /config/cache
      cp ${networkXml} /config/config/network.xml
    '';

    config = {
      Cmd = [
        "/bin/sh" "-c"
        ''
          ${mdnsPublisher}/bin/mdns-publisher \
            -iface eth0 -hostname jellyfin &
          exec ${pkgs.jellyfin}/bin/jellyfin \
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
  };
in
{
  systemd.services = veth.services;

  virtualisation.oci-containers = {
    backend = "podman";
    containers.jellyfin = {
      image = "jellyfin:latest";
      imageFile = jellyfinImage;
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
  };
}
