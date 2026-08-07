{ config, lib, pkgs, inputs, ... }:

let
  mkPodmanVeth = import ../modules/podman-veth.nix { inherit pkgs lib inputs; };

  veth = mkPodmanVeth {
    name   = "suwayomi";
    bridge = "br-lan";
    mac    = "02:00:00:00:00:07";
    mdns   = true;
  };

  suwayomiImage = pkgs.dockerTools.streamLayeredImage {
    name = "suwayomi";
    tag  = "latest";
    contents = [ pkgs.suwayomi-server pkgs.bash pkgs.coreutils ];
    config = {
      Cmd = [
        "${pkgs.suwayomi-server}/bin/tachidesk-server"
        "-Dsuwayomi.tachidesk.config.server.initialOpenInBrowserEnabled=false"
        "-Dsuwayomi.tachidesk.config.server.rootDir=/data/downloads"
      ];
      Env = [
        "HOME=/data"
      ];
      ExposedPorts = { "8080/tcp" = { }; };
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
      "/var/lib/suwayomi/data:/data"
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