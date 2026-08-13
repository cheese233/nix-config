{ config, lib, pkgs, inputs, ... }:

let
  # PBH shares the aria2 network namespace (see networking.aria2-netns, set by
  # containers-aria2-next.nix): same netns means loopback access to aria2's RPC
  # (127.0.0.1:6800) and the same SLAAC address on br-lan for the WebUI,
  # reachable via the aria2.local mDNS name published by containers-aria2-next.nix.

  # PeerBanHelper v9.5.0-alpha1 from the upstream .deb package
  # (jar + libraries/, JRE provided by Nix).
  pbh = pkgs.stdenv.mkDerivation {
    pname = "peerbanhelper";
    version = "9.5.0-alpha1";
    src = pkgs.fetchurl {
      url = "https://github.com/PBH-BTN/PeerBanHelper/releases/download/v9.5.0-alpha1/peerbanhelper_9.5.0-alpha1_all.deb";
      hash = "sha256-iyxxkDmHFSPFFJ8K394P9woSeE/v+9zn3qQcDUDp4+o=";
    };
    nativeBuildInputs = [ pkgs.dpkg ];
    dontUnpack = true;
    installPhase = ''
      mkdir -p $out
      dpkg-deb -x $src $out
    '';
  };

  # JVM flags mirrored from the upstream Dockerfile / deb systemd unit.
  pbhEntrypoint = pkgs.writeShellScriptBin "peerbanhelper-entrypoint" ''
    exec ${pkgs.temurin-jre-bin-25}/bin/java \
      -XX:SoftMaxHeapSize=386M \
      --enable-native-access=ALL-UNNAMED \
      -XX:+UseCompactObjectHeaders \
      -Dpbh.release=debian \
      -Dpbh.datadir=/data \
      -Dpbh.configdir=/data/config \
      -Dpbh.logsdir=/data/logs \
      -Djava.awt.headless=true \
      -Djdk.attach.allowAttachSelf=true \
      -XX:MaxRAMPercentage=85.0 \
      -XX:+UseG1GC \
      -XX:G1PeriodicGCInterval=60000 \
      -XX:MaxHeapFreeRatio=15 \
      -XX:MinHeapFreeRatio=5 \
      -Xss512k \
      -XX:+UseStringDeduplication \
      -XX:-ShrinkHeapInSteps \
      -jar PeerBanHelper.jar
  '';

  pbhImage = pkgs.dockerTools.streamLayeredImage {
    name = "peerbanhelper";
    tag  = "9.5.0-alpha1";
    contents = [ pbh pbhEntrypoint pkgs.bash pkgs.coreutils pkgs.cacert ];
    config = {
      Cmd = [ "${pbhEntrypoint}/bin/peerbanhelper-entrypoint" ];
      WorkingDir = "/usr/lib/peerbanhelper";
      Env = [
        "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
      ];
      ExposedPorts = { "9898/tcp" = { }; };
      Volumes = {
        "/data" = { };
        "/tmp" = { };
      };
    };
  };

in
{
  systemd.services = {
    "${config.virtualisation.oci-containers.containers.peerbanhelper.serviceName}" = {
      serviceConfig.StateDirectory = "peerbanhelper";
      after = [ "podman-veth-aria2.service" ];
      requires = [ "podman-veth-aria2.service" ];
    };
  };

  systemd.tmpfiles.rules = [
    "d /var/lib/peerbanhelper/data 0755 root root -"
  ];

  virtualisation.oci-containers.containers.peerbanhelper = {
    image = "peerbanhelper:9.5.0-alpha1";
    imageStream = pbhImage;
    autoStart = true;

    volumes = [
      "/var/lib/peerbanhelper/data:/data"
    ];

    environment = {
      TZ = "Asia/Shanghai";
    };

    extraOptions = [
      "--network=${config.networking.aria2-netns}"
      "--hostname=peerbanhelper"
      "--tmpfs=/tmp"
      "--cap-drop=ALL"
      "--security-opt=no-new-privileges:true"
      "--dns=fdea:d:beef::1"
    ];
  };
}
