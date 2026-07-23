{
  config,
  lib,
  pkgs,
  inputs,
  ...
}:

let
  mkPodmanVeth = import ../modules/podman-veth.nix { inherit pkgs lib inputs; };

  veth = mkPodmanVeth {
    name   = "aria2";
    bridge = "br-lan";
    mac    = "02:00:00:00:00:05";
    mdns   = true;
  };

  aria2NextPkg = inputs.aria2-next.packages.${pkgs.stdenv.hostPlatform.system}.default;

  aria2RpcSecret = "you-found-my-secret-lol";

  aria2RpcSecretB64 = builtins.readFile (pkgs.runCommand "aria2-secret-b64" {} ''
    printf '%s' ${lib.escapeShellArg aria2RpcSecret} | ${pkgs.coreutils}/bin/base64 -w0 > $out
  '');

  aria2Conf = pkgs.writeText "aria2.conf" ''
    dir=/downloads
    continue=true
    check-integrity=true

    max-concurrent-downloads=5
    max-connection-per-server=16
    min-split-size=20M
    split=16

    disk-cache=64M
    file-allocation=falloc

    enable-rpc=true
    rpc-listen-all=true
    rpc-listen-port=6800
    rpc-secret=${aria2RpcSecret}

    save-session=/var/lib/aria2/aria2.session
    save-session-interval=30
    input-file=/var/lib/aria2/aria2.session

    seed-ratio=1.0
    seed-time=60
    bt-enable-lpd=false
    bt-tracker-connect-timeout=10
    enable-dht=true
    listen-port=33888
    dht-listen-port=33888

    log-level=info
  '';

  aria2NextImage = pkgs.dockerTools.streamLayeredImage {
    name = "aria2-next";
    tag  = "latest";
    contents = [ aria2NextPkg ];
    config = {
      Cmd = [ "${aria2NextPkg}/bin/aria2-next" "--conf-path=/config/aria2.conf" ];
      Volumes = {
        "/downloads" = { };
        "/config" = { };
        "/var/lib/aria2" = { };
      };
    };
  };
in
{
  services.nginx.virtualHosts."ariang" = {
    onlySSL = lib.mkForce false;
    addSSL = lib.mkForce false;
    listen = [ { addr = "[::]"; port = 8600; } ];
    root = "${pkgs.ariang}/share/ariang";
    extraConfig = ''
      sub_filter '</head>'
        '<script>
           try {
             var o = JSON.parse(localStorage.getItem("Options") || "{}");
             if (!o.rpcHost) o.rpcHost = "aria2.local";
             if (!o.rpcPort) o.rpcPort = "6800";
             if (!o.secret)  o.secret  = "${aria2RpcSecretB64}";
             localStorage.setItem("Options", JSON.stringify(o));
           } catch (e) {}
         </script></head>';
      sub_filter_once on;
      sub_filter_types text/html;
    '';
  };

  networking.nftables.firewall.rules = {
    wan-to-lan-aria2-dht = {
      from = [ "wan" ];
      to = [ "lan" ];
      allowedTCPPorts = [ 33888 ];
      allowedUDPPorts = [ 33888 ];
    };
  };

  systemd.services = veth.services // {
    "${config.virtualisation.oci-containers.containers.aria2.serviceName}" = {
      serviceConfig.StateDirectory = "aria2";
      after = [ "podman-veth-aria2.service" ];
      requires = [ "podman-veth-aria2.service" ];
    };
  };

  systemd.tmpfiles.rules = [
    "d /var/lib/aria2/downloads 0775 root root -"
    "d /var/lib/aria2/config    0755 root root -"
  ];

  virtualisation.oci-containers.containers.aria2 = {
    image = "aria2-next:latest";
    imageStream = aria2NextImage;
    autoStart = true;

    volumes = [
      "/var/lib/aria2/downloads:/downloads"
      "/var/lib/aria2/config:/config"
      "${aria2Conf}:/config/aria2.conf"
    ];

    extraOptions = [
      "--network=${veth.arg}"
      "--hostname=aria2"
      "--tmpfs=/tmp"
      "--cap-drop=ALL"
      "--security-opt=no-new-privileges:true"
      "--dns=fdea:d:beef::1"
    ];
  };
}
