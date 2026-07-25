{ config, lib, pkgs, inputs, ... }:

let
  mkPodmanVeth = import ../modules/podman-veth.nix { inherit pkgs lib inputs; };

  veth = mkPodmanVeth {
    name   = "autobangumi";
    ifName = "autobangumi";
    bridge = "br-lan";
    mac    = "02:00:00:00:00:06";
    mdns   = true;
  };

  abPkg = inputs.auto-bangumi.packages.${pkgs.stdenv.hostPlatform.system}.default;

  abImage = pkgs.dockerTools.streamLayeredImage {
    name = "auto-bangumi";
    tag  = "latest";
    contents = [ abPkg pkgs.bash pkgs.coreutils pkgs.cacert ];
    config = {
      Cmd = [ "auto-bangumi" ];
      Env = [
        "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
      ];
      ExposedPorts = { "7892/tcp" = { }; };
      Volumes = { "/data" = { }; };
    };
  };
in
{
  systemd.services = veth.services // {
    "${config.virtualisation.oci-containers.containers.autobangumi.serviceName}" = {
      serviceConfig.StateDirectory = "auto-bangumi";
      after = [ "podman-veth-autobangumi.service" ];
      requires = [ "podman-veth-autobangumi.service" ];
    };
  };

  systemd.tmpfiles.rules = [
    "d /var/lib/auto-bangumi/data 0750 root root -"
  ];

  virtualisation.oci-containers.containers.autobangumi = {
    image = "auto-bangumi:latest";
    imageStream = abImage;
    autoStart = true;

    volumes = [
      "/var/lib/auto-bangumi/data:/data"
    ];

    environment = {
      HOME = "/data";
      HOST = "0.0.0.0";
      PORT = "7892";
    };

    extraOptions = [
      "--network=${veth.arg}"
      "--hostname=autobangumi"
      "--tmpfs=/tmp"
      "--cap-drop=ALL"
      "--security-opt=no-new-privileges:true"
      "--dns=fdea:d:beef::1"
      "--workdir=/data"
    ];
  };
}
