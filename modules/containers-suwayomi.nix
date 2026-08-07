{ config, lib, pkgs, inputs, ... }:

let
  mkPodmanVeth = import ../modules/podman-veth.nix { inherit pkgs lib inputs; };

  # nixpkgs (26.05 and master) still ships v2.1.1867, but that version cannot read
  # databases created by v2.2+/v2.3+ (CATEGORY."ORDER" was renamed to sort_order).
  suwayomiServer = pkgs.suwayomi-server.overrideAttrs (old: {
    version = "2.3.2243";
    src = pkgs.fetchurl {
      url = "https://github.com/Suwayomi/Suwayomi-Server/releases/download/v2.3.2243/Suwayomi-Server-v2.3.2243.jar";
      hash = "sha256-ghFBsy4XDUoC08vf7Vd+2PB70iOD/19BMuu1rkDpjdU=";
    };
  });

  veth = mkPodmanVeth {
    name   = "suwayomi";
    bridge = "br-lan";
    mac    = "02:00:00:00:00:07";
    mdns   = true;
  };

  suwayomiImage = pkgs.dockerTools.streamLayeredImage {
    name = "suwayomi";
    tag  = "latest";
    contents = [ suwayomiServer pkgs.bash pkgs.coreutils ];
    config = {
      Cmd = [
        "${suwayomiServer}/bin/tachidesk-server"
        "-Dsuwayomi.tachidesk.config.server.initialOpenInBrowserEnabled=false"
        "-Dsuwayomi.tachidesk.config.server.rootDir=/data/.local/share/Tachidesk/downloads"
      ];
      Env = [
        "HOME=/data"
      ];
      ExposedPorts = { "4567/tcp" = { }; };
      Volumes = { "/data" = { }; };
    };
  };
in
{
  systemd.services = veth.services // {
    "${config.virtualisation.oci-containers.containers.suwayomi.serviceName}" = {
      serviceConfig.StateDirectory = "suwayomi";
      after = [ "podman-veth-suwayomi.service" ];
      requires = [ "podman-veth-suwayomi.service" ];
    };
  };

  systemd.tmpfiles.rules = [
    "d /var/lib/suwayomi/data 0755 root root -"
    "d /var/lib/suwayomi/data/downloads 0755 root root -"
  ];

  virtualisation.oci-containers.containers.suwayomi = {
    image = "suwayomi:latest";
    imageStream = suwayomiImage;
    autoStart = true;

    volumes = [
      "/var/lib/suwayomi/data:/data/.local/share/Tachidesk"
    ];

    extraOptions = [
      "--network=${veth.arg}"
      "--hostname=suwayomi"
      "--tmpfs=/tmp"
      "--cap-drop=ALL"
      "--security-opt=no-new-privileges:true"
      "--dns=fdea:d:beef::1"
    ];
  };
}