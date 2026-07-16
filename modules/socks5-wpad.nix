{ config, lib, pkgs, inputs, ... }:

let
  lanIp = "fdea:d:beef::1";
  socks5Port = 1080;

  # PAC file directing IPv4 requests (both native IPv4 & DNS64 synthesized) to SOCKS5
  wpadRoot = pkgs.runCommand "wpad-root" {} ''
    mkdir -p $out
    cat <<'EOF' > $out/wpad.dat
    function FindProxyForURL(url, host) {
        if (isPlainHostName(host)) {
            return "DIRECT";
        }

        var ip = dnsResolve(host);
        if (!ip) {
            return "DIRECT";
        }

        // 1. Direct native IPv4 destinations to SOCKS5
        if (ip.indexOf(".") !== -1) {
            return "SOCKS5 [${lanIp}]:${toString socks5Port}; DIRECT";
        }

        // 2. Direct DNS64 synthesized AAAA (64:ff9b::/96) to SOCKS5
        if (ip.indexOf("64:ff9b::") === 0 || ip.indexOf("64:ff9b:") === 0) {
            return "SOCKS5 [${lanIp}]:${toString socks5Port}; DIRECT";
        }

        // 3. Native IPv6 connections go DIRECT
        return "DIRECT";
    }
    EOF
    ln -s wpad.dat $out/proxy.pac
  '';

  # Custom MIME types file to ensure wpad.dat and proxy.pac are served with correct headers
  mimeTypes = pkgs.writeText "mime.types" ''
    application/x-ns-proxy-autoconfig pac
    application/x-ns-proxy-autoconfig dat
    text/plain txt log
  '';
in
{
  imports = [
    inputs.socks5.nixosModules.default
  ];

  # Enable the custom SOCKS5 service on the router's LAN address
  services.socks5 = {
    enable = true;
    host = lanIp;
    port = socks5Port;
  };

  # Host WPAD configuration via darkhttpd on LAN only
  services.darkhttpd = {
    enable = true;
    address = lanIp;
    port = 80;
    rootDir = wpadRoot;
    extraArgs = [
      "--mimetypes" "${mimeTypes}"
    ];
  };

  # Open firewall ports on the router for SOCKS5 and Nginx WPAD server
  networking.nftables.firewall.rules = {
    lan-to-fw-wpad = {
      from = [ "lan" ];
      to = [ "fw" ];
      allowedTCPPorts = [ 80 ];
    };
    lan-to-fw-socks5 = {
      from = [ "lan" ];
      to = [ "fw" ];
      allowedTCPPorts = [ socks5Port ];
    };
  };
}
