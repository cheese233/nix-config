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
        "/bin/sh" "-c"
        ''
          ${mdnsPublisher}/bin/mdns-publisher \
            -iface eth0 -hostname jellyfin &
          exec ${pkgsUnstable.jellyfin}/bin/jellyfin \
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
  systemd.services = veth.services // {
    "jellyfin-network-seed" = {
      description = "Seed jellyfin network.xml for IPv6";
      before = [ "podman-jellyfin.service" ];
      requiredBy = [ "podman-jellyfin.service" ];
      serviceConfig.Type = "oneshot";
      script = ''
        if [ ! -f /var/lib/jellyfin/config/network.xml ]; then
          mkdir -p /var/lib/jellyfin/config /var/lib/jellyfin/cache
          cat > /var/lib/jellyfin/config/config/network.xml <<'XML'
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
XML
        fi
      '';
    };
  };

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
