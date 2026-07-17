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

  services.nginx = {
    enable = true;

    appendHttpConfig = ''
      types {
        application/x-ns-proxy-autoconfig pac dat;
      }
    '';

    virtualHosts."lan" = {
      locations."= /wpad.dat" = {
        root = wpadRoot;
      };
      locations."= /proxy.pac" = {
        root = wpadRoot;
      };
    };
  };

  # Open firewall ports on the router for SOCKS5 and WPAD
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
