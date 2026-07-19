{ config, lib, pkgs, inputs, ... }:

let
  mkPodmanVeth = import ../modules/podman-veth.nix { inherit pkgs lib inputs; };

  veth = mkPodmanVeth {
    name   = "vaultwarden";
    bridge = "br-lan";
    mac    = "02:00:00:00:00:04";
    mdns   = true;
  };

  vaultwardenImage = pkgs.dockerTools.streamLayeredImage {
    name = "vaultwarden";
    tag  = "latest";
    contents = [ pkgs.vaultwarden ];
    config = {
      Cmd = [ "vaultwarden" ];
      ExposedPorts = { "8222/tcp" = { }; };
      Volumes = { "/data" = { }; };
    };
  };
in
{
  systemd.services = veth.services // {
    "${config.virtualisation.oci-containers.containers.vaultwarden.serviceName}" = {
      serviceConfig.StateDirectory = "vaultwarden";
      after = [ "podman-veth-vaultwarden.service" ];
      requires = [ "podman-veth-vaultwarden.service" ];
    };
  };

  systemd.tmpfiles.rules = [
    "d /var/lib/vaultwarden/data 0755 root root -"
  ];

  virtualisation.oci-containers.containers.vaultwarden = {
    image = "vaultwarden:latest";
    imageFile = vaultwardenImage;
    autoStart = true;

    volumes = [
      "/var/lib/vaultwarden/data:/data"
    ];

    environment = {
      ROCKET_ADDRESS = "0.0.0.0";
      ROCKET_PORT = "8222";
      ROCKET_LOG = "critical";
      SIGNUPS_ALLOWED = "false";
    };

    extraOptions = [
      "--network=${veth.arg}"
      "--hostname=vaultwarden"
      "--tmpfs=/tmp"
      "--cap-drop=ALL"
      "--security-opt=no-new-privileges:true"
      "--dns=fdea:d:beef::1"
    ];
  };
}
