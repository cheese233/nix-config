{ config, lib, pkgs, inputs, ... }:

let
  mkPodmanVeth = import ../modules/podman-veth.nix { inherit pkgs lib inputs; };

  veth = mkPodmanVeth {
    name   = "autobangumi";
    ifName = "ab";
    bridge = "br-lan";
    mac    = "02:00:00:00:00:06";
    mdns   = true;
  };

  abPkg = inputs.auto-bangumi.packages.${pkgs.stdenv.hostPlatform.system}.default;

  abEntrypoint = pkgs.writeShellScriptBin "auto-bangumi-entrypoint" ''
    set -e
    rm -rf /data/dist
    ln -s ${abPkg}/lib/dist /data/dist
    cd /data
    exec ${abPkg}/bin/auto-bangumi "$@"
  '';

  abImage = pkgs.dockerTools.streamLayeredImage {
    name = "auto-bangumi";
    tag  = "latest";
    contents = [ abPkg abEntrypoint pkgs.bash pkgs.coreutils pkgs.cacert ];
    config = {
      Cmd = [ "${abEntrypoint}/bin/auto-bangumi-entrypoint" ];
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
      "/var/lib/aria2/downloads:/downloads"
    ];

    environment = {
      HOME = "/data";
      HOST = "::";
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
