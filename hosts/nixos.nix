{ config, lib, pkgs, inputs, ... }:

{
  imports = [
    ../hardware/nixos.nix
    ../modules/zfs-kernel.nix
    inputs.microvm.nixosModules.host
    inputs.nnf.nixosModules.default
    inputs.dae.nixosModules.dae
    inputs.mdns-publisher.nixosModules.default
    inputs.mdns-repeater.nixosModules.default
    ../modules/microvm-traefik.nix
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

  # ==================== DNS (Knot Resolver) ====================
  environment.etc."knot-resolver/china-domain-list.txt" = {
    source = "${inputs.dnsmasq-china-list.packages.${pkgs.stdenv.hostPlatform.system}.default}/etc/china-domain-list.txt";
  };

  services.knot-resolver = {
    enable = true;
    extraConfig = ''
      -- Load required modules
      modules = {
        'dns64',     -- IPv6-only transition (NAT64) support
        'policy',    -- Domain policy forwarding
        'prefetch',  -- Cache prefetching for active records
        'hints'      -- Static hosts resolution
      }

      -- 1. Configure DNS64
      dns64.config({
        prefix = '64:ff9b::'  -- Same NAT64 prefix as TAYGA
      })

      -- 2. Define DNS groups
      local china_dns_group = policy.FORWARD({
        '223.5.5.5',
        '223.6.6.6'
      })

      local foreign_dns_group = policy.FORWARD({
        '8.8.8.8',
        '8.8.4.4'
      })

      -- 3. Default fallback routing to foreign group
      policy.add(policy.all(foreign_dns_group))

      -- 4. Bind listener to localhost loopback interfaces and LAN IPv6 gateway address
      net.listen('127.0.0.1', 53)
      net.listen('::1', 53)
      net.listen('fdea:d:beef::1', 53)

      -- 5. Load china-domain-list for domestic split-tunneling
      local china_domains = {}
      local file = io.open("/etc/knot-resolver/china-domain-list.txt", "r")
      if file then
        for line in file:lines() do
          if line ~= "" and not string.match(line, "^%s*#") then
            table.insert(china_domains, line)
          end
        end
        file:close()
      end

      if #china_domains > 0 then
        policy.add(policy.suffix(china_dns_group, policy.todnames(china_domains)))
      end
    '';
  };

  # ==================== mDNS Repeater ====================
  # geekman/mdns-repeater is used to repeat mDNS multicast packets between DIFFERENT
  # Layer 2 network segments (e.g. br-lan and a separate guest/IoT interface like wlan0).
  # 
  # NOTE: Since the vm-traefik MicroVM is directly bridged as a port of br-lan, the Linux kernel
  # bridge natively forwards mDNS multicast packets between them. Therefore, vm-traefik should NOT
  # be listed here.
  services.mdns-repeater = {
    enable = true;
    interfaces = [ "br-lan" "wlan0" ]; # Replace wlan0 with your other local interfaces (e.g. IoT VLAN)
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
    };
    rules = {
      lan-to-wan = { from = [ "lan" ]; to = [ "wan" ]; verdict = "accept"; };
      lan-to-nat64 = { from = [ "lan" ]; to = [ "nat64" ]; verdict = "accept"; };
      nat64-to-wan = { from = [ "nat64" ]; to = [ "wan" ]; verdict = "accept"; masquerade = true; };
      lan-to-fw-ipv6 = { from = [ "lan" ]; to = [ "fw" ]; extraLines = [ "meta l4proto icmpv6 accept comment \"Allow ICMPv6 from LAN\"" ]; };
      lan-to-fw-dns = { from = [ "lan" ]; to = [ "fw" ]; allowedUDPPorts = [ 53 ]; allowedTCPPorts = [ 53 ]; };
      lan-to-fw-mdns = { from = [ "lan" ]; to = [ "fw" ]; allowedUDPPorts = [ 5353 ]; };
      lan-to-fw-ssh = { from = [ "lan" ]; to = [ "fw" ]; allowedTCPPorts = config.services.openssh.ports; };
      wan-to-fw-ipv6 = { from = [ "wan" ]; to = [ "fw" ]; allowedUDPPorts = [ 546 ]; extraLines = [ "meta l4proto icmpv6 accept comment \"Allow ICMPv6 for RAs and ND\"" ]; };
    };
  };

  # ==================== mDNS ====================
  # Local mDNS responder: publishes this host's A/AAAA records as
  # `nixos.local` on br-lan. Pairs with mdns-repeater below so mDNS
  # packets can be repeated across local segments (e.g., LAN and MicroVM tap),
  # allowing native mDNS clients to resolve `.local` domains smoothly.
  #
  # Module provided by the ./pkgs/mdns-publisher flake (pure Go,
  # no CGO). openFirewall is off because we use nftables below;
  # the lan-to-fw-mdns rule opens UDP 5353 on br-lan.
  services.mdns-publisher = {
    enable = true;
    interface = "br-lan";
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
          kresd: 'udp://127.0.0.1:53'
        }
        routing {
          request {
            fallback: kresd
          }
        }
      }
      routing {
        pname(NetworkManager) -> direct
        dip(224.0.0.0/3, 'ff00::/8') -> direct
        dip(geoip:private) -> direct
        pname(kresd) && dport(53) -> must_direct

        dip(geoip:cn) -> direct
        domain(geosite:cn) -> direct

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
}
