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

  aria2Entrypoint = pkgs.writeShellScriptBin "aria2-entrypoint" ''
    set -e

    echo "plat-prefix=64:ff9b::/96" > /tmp/clatd.conf

    ${pkgs.clatd}/bin/clatd -c /tmp/clatd.conf &

    i=0
    while [ $i -lt 30 ]; do
      [ -d /sys/class/net/clat ] && break
      i=$((i + 1))
      ${pkgs.coreutils}/bin/sleep 1
    done

    exec ${aria2NextPkg}/bin/aria2-next --conf-path=/config/aria2.conf
  '';

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
    rpc-allow-origin-all=true

    save-session=/config/aria2.session
    save-session-interval=30
    input-file=/config/aria2.session

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
    contents = [ aria2NextPkg pkgs.clatd aria2Entrypoint pkgs.bash pkgs.coreutils ];
    config = {
      Cmd = [ "${aria2Entrypoint}/bin/aria2-entrypoint" ];
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
             var _orig = localStorage.setItem;
             localStorage.setItem = function(k,v) {
               if (k === "AriaNg.Options") {
                 try {
                   var o = JSON.parse(v);
                   o.rpcHost = "aria2.local";
                   o.rpcPort = "6800";
                   o.secret = "${aria2RpcSecretB64}";
                   v = JSON.stringify(o);
                 } catch(e) {}
               }
               _orig.call(localStorage, k, v);
             };
           } catch(e) {}
         </script></head>';
      sub_filter_once on;
      sub_filter_types text/html;
    '';
  };

  networking.nftables.firewall.rules = {
    lan-to-fw-ariang = {
      from = [ "lan" ];
      to = [ "fw" ];
      allowedTCPPorts = [ 8600 ];
    };
    wan-to-lan-aria2-dht = {
      from = [ "wan" ];
      to = [ "lan" ];
      allowedTCPPorts = [ 33888 ];
      allowedUDPPorts = [ 33888 ];
    };
    wan-to-nat64-aria2-dht-ipv4 = {
      from = [ "wan" ];
      to = [ "nat64" ];
      extraLines = [
        "meta protocol ip tcp dport 33888 accept comment \"Allow aria2 DHT (IPv4)\""
        "meta protocol ip udp dport 33888 accept comment \"Allow aria2 DHT (IPv4)\""
      ];
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
    "f /var/lib/aria2/config/aria2.session 0644 root root -"
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
      "--cap-add=NET_ADMIN"
      "--security-opt=no-new-privileges:true"
      "--dns=fdea:d:beef::1"
    ];
  };
}
