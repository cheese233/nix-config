{ config, lib, pkgs, inputs, ... }:
{
  imports = [
    ../hardware/nixos.nix
    ../modules/zfs-kernel.nix
    inputs.microvm.nixosModules.host
    inputs.nnf.nixosModules.default
    inputs.dae.nixosModules.dae
    inputs.avahi2dns.nixosModules.default
    inputs.microdoh.nixosModules.default
    ../modules/microvm-traefik.nix
    ../modules/amneziawg.nix
  ];

  networking.hostName = "nixos";
  networking.nameservers = [ "::1" "127.0.0.1" ];

  # https://nixos.org/manual/nixos/stable/options#opt-system.stateVersion
  # Do NOT change this after initial install unless you know what you're doing.
  system.stateVersion = "26.05";

  fileSystems = {
    "/nix".options = [ "compress=zstd" "noatime" ];
    "/swap".options = [ "noatime" "nodatacow" ];
  };
  services.openssh.settings.PermitRootLogin = "yes";

  # Host-side VSOCK support for `microvm -s <vm>` (= `ssh vsock/<CID>`).
  # The ssh proxy itself is provided by systemd's 20-systemd-ssh-proxy.conf.
  boot.kernelModules = [ "vhost_vsock" ];
  # ==================== ZFS ====================
  boot.supportedFilesystems = [ "zfs" ];
  services.zfs.autoScrub.enable = true;
  networking.hostId = "deadbeef";
  boot.zfs.extraPools = [ "HDD" ];

  # ==================== Secrets ====================
  age = {
    identityPaths = [ "/var/lib/agenix/key.txt" ];
    secrets.ppp-secrets = {
      file = ../secrets/ppp-secrets.age;
      path = "/etc/ppp/chap-secrets";
    };
    secrets.ppp-name = {
      file = ../secrets/ppp-name.age;
      path = "/etc/ppp/name";
    };
    secrets.dae-sub = {
      file = ../secrets/dae-sub.age;
      path = "/etc/dae/local.sub";
    };
    secrets.awg-key = {
      file = ../secrets/awg-key.age;
      path = "/etc/wireguard/awg-key";
    };
    secrets.doh-env = {
      file = ../secrets/doh-env.age;
      path = "/run/agenix/doh-env";
    };
  };

  # ==================== PPPoE ====================
  services.pppd = {
    enable = true;
    peers = {
      pppoe = {
        autostart = true;
        enable = true;
        config = ''
          plugin pppoe.so enp2s0f0
          file /etc/ppp/name
          noauth
          persist
          maxfail 0
          holdoff 5
          defaultroute
          mtu 1492
          +ipv6
          ipv6cp-accept-local
          ifname ppp0
        '';
      };
    };
  };

  systemd.services."pppd-pppoe" = {
    preStart = "${pkgs.iproute2}/bin/ip link set enp2s0f0 up";
  };

  # ==================== Networking ====================
  boot.kernel.sysctl = {
    "net.ipv6.conf.all.forwarding" = 1;
    "net.ipv6.conf.all.accept_ra" = 2;
    "net.ipv4.conf.all.forwarding" = 1;
    "net.ipv4.conf.br-lan.send_redirects" = 0;
    "net.ipv4.ip_forward" = 1;
  };

  networking.bridges = {
    "br-lan".interfaces = [ "enp2s0f1" "enp3s0" ];
  };

  networking.interfaces."br-lan" = {
    ipv6.addresses = [ { address = "fdea:d:beef::1"; prefixLength = 64; } ];
  };

  networking.dhcpcd = {
    enable = true;
    allowInterfaces = [ "ppp0" ];
    extraConfig = ''
      noipv4
      interface ppp0
        ipv6rs
        ia_na
        iaid 1
        ia_pd 1 br-lan/0
    '';
  };

  services.radvd = {
    enable = true;
    config = ''
      interface br-lan {
        AdvSendAdvert on;
        MinRtrAdvInterval 30;
        MaxRtrAdvInterval 100;
        prefix ::/64 {
          AdvOnLink on;
          AdvAutonomous on;
          AdvRouterAddr on;
        };
        RDNSS fdea:d:beef::1 {
        };
      };
    '';
  };

  # ==================== DNS (Unbound) ====================
  # Static china domain list from dnsmasq-china-list package (consumed at runtime
  # by unbound-china-domains.service to generate forward-zone config).
  environment.etc."unbound/china-domain-list.txt" = {
    source = "${inputs.dnsmasq-china-list.packages.${pkgs.stdenv.hostPlatform.system}.default}/etc/china-domain-list.txt";
  };

  # Generate /run/unbound/china-domains.conf before unbound starts.
  # Reads the static domain list + the DOH upstream domain from agenix secret
  # and writes one forward-zone block per domain, targeting Chinese DNS servers.
  systemd.services.unbound-china-domains = {
    description = "Generate unbound china-domain forward zones";
    before = [ "unbound.service" ];
    requiredBy = [ "unbound.service" ];
    after = [ "agenix.service" ];
    serviceConfig = {
      Type = "oneshot";
      RuntimeDirectory = "unbound";
    };
    script = ''
      set -a; . /run/agenix/doh-env; set +a
      emit_zone() {
        echo "forward-zone:"
        echo "    name: \"$1.\""
        echo "    forward-addr: 119.29.29.29"
        echo "    forward-addr: 180.184.1.1"
        echo "    forward-addr: 180.184.2.2"
      }
      {
        while IFS= read -r domain; do
          [[ -z "$domain" || "$domain" == \#* ]] && continue
          emit_zone "$domain"
        done < /etc/unbound/china-domain-list.txt
        # DOH upstream domain from agenix secret — also via China DNS to avoid
        # bootstrap deadlock (microdoh needs to resolve it before it's up).
        emit_zone "$DOMAIN"
      } > /run/unbound/china-domains.conf
    '';
  };

  services.unbound = {
    enable = true;
    enableRootTrustAnchor = false;
    settings = {
      server = {
        # 1. Bind to localhost and LAN IPv6 gateway address
        interface = [ "127.0.0.1" "::1" "fdea:d:beef::1" ];
        access-control = [
          "127.0.0.0/8 allow"
          "::1/128 allow"
          "fdea:d:beef::/64 allow"
        ];
        # 2. DNS64 — synthesize AAAA from A records using NAT64 prefix
        module-config = ''"dns64 iterator"'';
        dns64-prefix = "64:ff9b::/96";
        # 3. Caching
        key-cache-size = "256m";
        msg-cache-size = "512m";
        rrset-cache-size = "256m";
        prefetch = true;
        prefetch-key = true;
      };
      # Include runtime-generated china-domain forward zones.
      # WARNING: this disables build-time checkconf (module handles this),
      # so the generated file MUST be valid unbound.conf syntax.
      include = "/run/unbound/china-domains.conf";
      # .local → avahi2dns mDNS bridge (127.0.0.1:5354)
      stub-zone = [{
        name = "local.";
        stub-addr = "127.0.0.1@5354";
      }];
      # Default fallback → microdoh (DoH client on [::1]:5443).
      # Individual china-domain forward-zone entries (included above) are more
      # specific and take priority over this catch-all.
      forward-zone = [{
        name = ".";
        forward-addr = "::1@5443";
      }];
    };
  };

  # ==================== avahi2dns ====================
  # mDNS bridge on 127.0.0.1:5354, consumed by unbound's
  # stub-zone for `.local` (see unbound config above).
  services.avahi2dns = {
    enable = true;
    address = "127.0.0.1";
    port = 5354;
    domain = "local";
  };

  # ==================== DNS-over-HTTPS client (microdoh) ====================
  services.microdoh = {
    enable = true;
    package = inputs.microdoh.packages.${pkgs.stdenv.hostPlatform.system}.microdoh-h3;
    listen = "[::1]:5443";
    upstream = "https://unset";  # overridden by ExecStart script
    bootstrapDns = "127.0.0.1";
    timeoutSecs = 30;
    tokenFile = null;
  };

  systemd.services.microdoh = {
    after = lib.mkForce [ "network.target" "unbound.service" "agenix.service" ];
    wants = lib.mkForce [ "unbound.service" ];

    serviceConfig = {
      ExecStart = lib.mkForce (
        let
          script = pkgs.writeShellScript "microdoh-start" ''
            set -a; . /run/agenix/doh-env; set +a
            exec ${config.services.microdoh.package}/bin/microdoh \
              --listen ${config.services.microdoh.listen} \
              --upstream "https://$DOMAIN$URI_PATH" \
              --bootstrap-dns ${config.services.microdoh.bootstrapDns} \
              --timeout-secs ${toString config.services.microdoh.timeoutSecs} \
              --token "$TOKEN"
          '';
        in "+${script}"
      );
      ExecStartPre = lib.mkForce null;
      RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
    };
  };

  services.resolved.enable = false;

  # ==================== Avahi mDNS ====================
  # Acts as mDNS responder (publishing) on local interfaces.
  services.avahi = {
    enable = true;
    nssmdns4 = true;
    nssmdns6 = true;
    publish = {
      enable = true;
      addresses = true;
      domain = true;
      workstation = true;
    };
  };

  # ==================== NAT64 ====================
  services.tayga = {
    enable = true;
    ipv4 = {
      address = "192.168.255.2";
      router.address = "192.168.255.1";
      pool = {
        address = "192.168.255.0";
        prefixLength = 24;
      };
    };
    ipv6 = {
      # Source address of the TAYGA server; must NOT reside inside the NAT64 prefix
      address = "fdea:d:beef::2";
      router.address = "64:ff9b::1";
      pool = {
        address = "64:ff9b::";
        prefixLength = 96;
      };
    };
    # Using private IPv4 (192.168.255.0/24) with Well-Known Prefix (64:ff9b::/96)
    # requires wkpfStrict = false. Otherwise tayga rejects mappings like
    # 64:ff9b::1 -> 0.0.0.1 (private) when the router itself sends traffic.
    wkpfStrict = false;
  };

  # ==================== Firewall ====================
  networking.firewall.enable = false;

  networking.nftables.firewall = {
    enable = true;
    snippets.nnf-common.enable = true;
    # nnf-ssh and nnf-nixos-firewall default to `from = "all"`, exposing sshd
    # to wan. Override both to lan-only below.
    snippets.nnf-ssh.enable = false;
    rules.nixos-firewall.from = lib.mkForce [ "lan" ];
    zones = {
      wan = { interfaces = [ "ppp0" ]; };
      lan = { interfaces = [ "br-lan" ]; };
      nat64 = { interfaces = [ "nat64" ]; };
      awg = { interfaces = [ "awg0" ]; };
    };
    rules = {
      lan-to-wan = { from = [ "lan" ]; to = [ "wan" ]; verdict = "accept"; };
      lan-to-nat64 = { from = [ "lan" ]; to = [ "nat64" ]; verdict = "accept"; };
      nat64-to-wan = { from = [ "nat64" ]; to = [ "wan" ]; verdict = "accept"; masquerade = true; };
      lan-to-fw-ipv6 = { from = [ "lan" ]; to = [ "fw" ]; extraLines = [ "meta l4proto icmpv6 accept comment \"Allow ICMPv6 from LAN\"" ]; };
      lan-to-fw-dns = { from = [ "lan" ]; to = [ "fw" ]; allowedUDPPorts = [ 53 ]; allowedTCPPorts = [ 53 ]; };
      lan-to-fw-mdns = { from = [ "lan" ]; to = [ "fw" ]; allowedUDPPorts = [ 5353 ]; };
      lan-to-fw-ssh = { from = [ "lan" ]; to = [ "fw" ]; allowedTCPPorts = config.services.openssh.ports; };
      lan-to-fw-awg = { from = [ "lan" ]; to = [ "fw" ]; allowedUDPPorts = [ 47999 ]; };
      lan-to-awg = { from = [ "lan" ]; to = [ "awg" ]; verdict = "accept"; };
      awg-to-lan = { from = [ "awg" ]; to = [ "lan" ]; verdict = "accept"; };
      awg-to-fw-dns = { from = [ "awg" ]; to = [ "fw" ]; allowedUDPPorts = [ 53 5443 ]; allowedTCPPorts = [ 53 5443 ]; };
      awg-to-fw-icmpv6 = { from = [ "awg" ]; to = [ "fw" ]; extraLines = [ "meta l4proto icmpv6 accept comment \"Allow ICMPv6 from AWG\"" ]; };
      wan-to-fw-ipv6 = { from = [ "wan" ]; to = [ "fw" ]; allowedUDPPorts = [ 546 ]; extraLines = [ "meta l4proto icmpv6 accept comment \"Allow ICMPv6 for RAs and ND\"" ]; };
    };
  };

  # ==================== DAE ====================
  services.dae = {
    enable = true;
    openFirewall = {
      enable = true;
      port = 10800;
    };
    config = ''
      global {
        tproxy_port: 10800
        wan_interface: ppp0 # Use "auto" to auto detect WAN interface.
        lan_interface: br-lan

        log_level: info
        allow_insecure: false
        auto_config_kernel_parameter: false
      }

      subscription {
        'file://local.sub'
      }

      group {
        proxy {
          policy: fixed(0)
        }
      }
      dns {
        upstream {
          unbound: 'udp://127.0.0.1:53'
        }
        routing {
          request {
            fallback: unbound
          }
        }
      }
      routing {
        pname(NetworkManager) -> direct
        dip(224.0.0.0/3, 'ff00::/8') -> direct
        dip(geoip:private) -> direct
        pname(unbound) -> must_rules
        pname(microdoh) -> must_rules

        dip(geoip:cn) -> direct
        domain(geosite:cn) -> direct
        domain(suffix: cloudflare.com) -> direct

        fallback: proxy
      }
    '';
  };

  environment.etc."systemd/journald@dae.conf".text = ''
    [Journal]
    Storage=volatile
    RuntimeMaxFileSize=5M
    RuntimeMaxFiles=3
  '';
  systemd.services.dae.serviceConfig.LogNamespace = "dae";

  services.amneziawg.interfaces.awg0 = {
    address = [ "fdea:d:beef:7767::1/64" ];
    listenPort = 47999;
    privateKeyFile = "/etc/wireguard/awg-key";

    extraOptions = {
      H1 = 114;
      H2 = 514;
      H3 = 1919;
      H4 = 810;
      S1 = 15;
      S2 = 16;
      S3 = 10;
      S4 = 0;
      Jc = 4;
      Jmin = 10;
      Jmax = 70;

      # Moonlight:
      I1 = "<b 0x8fff793082ff00010000ffff00000384000100000000003000000000000000000000138800000002000000024cd7f6aa91a78765>";
      I2 = "<b 0x8000><r 14>";
    };

    peers = [
      {
        publicKey = "TmOvG7hFvqGvwffqJT3qRmwdA7tGtPEgpovZEuqgqEE=";
        allowedIPs = [ "fdea:d:beef:7767:0:1::/96" ];
      }
    ];
  };

}
