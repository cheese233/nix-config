{ config, lib, pkgs, inputs, ... }:
{
  imports = [
    ../hardware/nixos.nix
    ../modules/zfs-kernel.nix
    ../modules/containers-jellyfin.nix
    ../modules/containers-vaultwarden.nix
    ../modules/containers-aria2-next.nix
    ../modules/containers-peerbanhelper.nix
    ../modules/containers-autobangumi.nix
    ../modules/containers-suwayomi.nix
    ../modules/libvirt-lxc.nix
    inputs.microvm.nixosModules.host
    inputs.nnf.nixosModules.default
    inputs.honk.nixosModules.default
    inputs.avahi2dns.nixosModules.default
    inputs.microdoh3.nixosModules.default
    inputs.dibbler.nixosModules.default
    ../modules/microvm-traefik.nix
    ../modules/amneziawg.nix
    ../modules/socks5-wpad.nix
    ../modules/cockpit.nix
    ../modules/cfst-doh-upstream.nix
  ];

  environment.systemPackages = with pkgs; [
    rsync
  ];

  networking.hostName = "nixos";
  networking.nameservers = [ "::1" "127.0.0.1" ];

  services.nginx.virtualHosts."lan".listen = [ { addr = "[fdea:d:beef::1]"; port = 80; } ];

  virtualisation.podman.enable = true;
  virtualisation.oci-containers.backend = "podman";

  # ==================== Ubuntu LXC (libvirt) ====================
  # The domain XML is generated from these attributes (see modules/libvirt-lxc.nix);
  # the rootfs itself has to be provisioned separately, e.g.
  #   debootstrap --variant=minbase resolute /var/lib/lxc/ubuntu/rootfs
  virtualisation.libvirtLxc.containers.ubuntu = {
    vcpus  = 2;
    memory = 5120;
    swap   = 4096;
    rootfs = "/var/lib/lxc/ubuntu/rootfs";
    mac    = "02:00:00:00:00:08";
    # Unprivileged (see the `idmap` option): the domain gets its own user
    # namespace, so the container's root is uid 100000 on the host instead of
    # the host's uid 0.  Mapped ids: 0..65535 inside -> 100000..165535 outside.
    #
    # The rootfs must be owned by that range *before* the domain starts, and the
    # definition is only read at libvirtd start:
    #   chown -R 100000:100000 /var/lib/lxc/ubuntu/rootfs
    #   systemctl restart libvirtd
    #   virsh -c lxc:/// destroy ubuntu && virsh -c lxc:/// start ubuntu
    # Going back to a privileged domain means chowning the rootfs to 0:0 and
    # removing this attribute.
    idmap = { target = 100000; count = 65536; };
  };

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
  boot.kernelModules = [ "vhost_vsock" "tun" ];
  # ==================== ZFS ====================
  boot.supportedFilesystems = [ "zfs" ];
  services.zfs.autoScrub.enable = true;
  networking.hostId = "deadbeef";
  boot.zfs.extraPools = [ "HDD" ];

  systemd.services.zfs-mount.enable = false;

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
    secrets.honk-sub = {
      file = ../secrets/dae-sub.age;
      path = "/etc/honk/local.sub";
    };
    secrets.awg-key = {
      file = ../secrets/awg-key.age;
      path = "/etc/wireguard/awg-key";
    };
    secrets.doh-env = {
      file = ../secrets/doh-env.age;
      path = "/run/agenix/doh-env";
    };
    secrets.vaultwarden-env = {
      file = ../secrets/vaultwarden-env.age;
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
    "net.ipv4.conf.all.send_redirects" = 0;
    "net.ipv4.conf.default.send_redirects" = 0;
    "net.ipv4.ip_forward" = 1;
  };

  networking.bridges = {
    "br-lan".interfaces = [ "enp2s0f1" "enp3s0" ];
  };

  # br-lan is IPv6-only: LAN clients address each other over the ULA /64 and
  # reach IPv4-only destinations through DNS64 + NAT64 (see tayga below).
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
        AdvManagedFlag off;
        AdvOtherConfigFlag on;
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

  # ==================== DHCPv6 (Dibbler, stateless) ====================
  # Stateless DHCPv6: clients use SLAAC for addresses (via radvd above);
  # this server announces DNS, domain search, and NAT64 prefix (PREF64).
  services.dibbler-server = {
    enable = true;
    config = ''
      log-level 7
      log-mode short
      stateless

      iface br-lan {
        option dns-server fdea:d:beef::1
        option domain local
        option lifetime 3600
        option 113 duid 80:00:00:00:00:00:00:00:00
        option 252 string "http://wpad.local/wpad.dat"
      }
    '';
  };

  # ==================== DNS (Unbound) ====================
  # Static china domain list (consumed at runtime by unbound-forward-zones.service).
  environment.etc."unbound/china-domain-list.txt" = {
    source = "${inputs.dnsmasq-china-list.packages.${pkgs.stdenv.hostPlatform.system}.default}/etc/china-domain-list.txt";
  };

  systemd.services.unbound-forward-zones = {
    description = "Generate unbound forward zones";
    before = [ "unbound.service" ];
    requiredBy = [ "unbound.service" ];
    after = [ "agenix.service" ];
    serviceConfig = {
      Type = "oneshot";
      StateDirectory = "unbound";
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
        # NOTE: $DOMAIN is only forwarded here as the *fallback*. Once
        # cfst-doh-upstream (see below) has published 优选 IPs for the name as
        # local-data, unbound answers the local zone first — local zones are
        # served in daemon/worker.c, before the module stack and before any
        # forward-zone lookup — so the forward below stops being consulted for
        # A queries while the local data exists. Keeping it means the name
        # still resolves (to whatever the anycast address is) on the very first
        # boot, before the first CloudflareSpeedTest run, which is also what
        # avoids the bootstrap deadlock: microdoh3 needs $DOMAIN resolved before
        # it can answer, and the default route below points at microdoh3.
        #
        # 1. .local → avahi2dns mDNS bridge (127.0.0.1:5354)
        echo "stub-zone:"
        echo "    name: local."
        echo "    stub-addr: 127.0.0.1@5354"
        # 2. China domains → China DNS
        while IFS= read -r domain; do
          [[ -z "$domain" || "$domain" == \#* ]] && continue
          emit_zone "$domain"
        done < /etc/unbound/china-domain-list.txt
        # DoH upstream domain from agenix secret — also via China DNS, as the
        # pre-优选 fallback described above.
        emit_zone "$DOMAIN"
        # 3. Default fallback → microdoh3 (DoH/3 client on [::1]:5443)
        echo "forward-zone:"
        echo "    name: ."
        echo "    forward-addr: ::1@5443"
      } > /var/lib/unbound/forward-zones.conf
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
          "fdea:d:beef::/48 allow"
        ];
        local-data = [
          "\"wpad.local. IN AAAA fdea:d:beef::1\""
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
        # Optimistic caching: serve expired records while refreshing in the
        # background so clients never see a cache miss latency spike.
        serve-expired = true;
        serve-expired-client-timeout = "5000";
        serve-expired-ttl = "86400";
        serve-expired-ttl-reset = false;
        do-not-query-localhost = false;
      };
      remote-control = {
        control-enable = true;
        control-port = 8953;
      };
      # Include runtime-generated forward/stub zones (see unbound-forward-zones.service).
      # cfst-doh-upstream appends its own generated include (the 优选 IP
      # local-data) through `lib.mkAfter`, which is why this is a list.
      include = [ "/var/lib/unbound/forward-zones.conf" ];
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
    v6Only = true;
  };

  # ==================== DNS-over-HTTP/3 client (microdoh3) ====================
  #
  # Upstream selection. Each worker owns exactly one QUIC connection (the hot
  # path); every 2s it publishes the request-level RTT it measured as a score
  # (1/RTT), and the supervisor turns those into per-address weights. The weighted slot
  # assignment then splits the *fleet* across addresses in proportion — with 6
  # workers, weights 3:2:1 really do place 3/2/1 workers — because unbound picks
  # a fresh source port per query, so SO_REUSEPORT spreads new queries evenly
  # over the workers. Nothing in the request path changes.
  #
  # A worker only migrates when its newly assigned address measures materially
  # better (150% weight), at most once every 3 minutes, staggered by worker
  # index, so the fleet never re-handshakes at once and a DNS TTL refresh
  # cannot disturb a healthy connection. Measured on this line, individual edge
  # addresses differ by more than 10x at the same instant (0.42s vs 5s
  # timeouts), so the selection has real work to do even with two candidates.
  #
  # A worker that cannot serve (reconnecting, still handshaking, or draining a
  # GOAWAY) closes its DNS socket, which takes it out of the kernel's reuseport
  # group. That matters because SO_REUSEPORT hashes the client's flow (source
  # address *and* port) and unbound reuses that flow when it retries: a worker
  # that stayed in the group would be fed the retries of the SERVFAILs it just
  # sent, for as long as its reconnect lasts. Out of the group, the kernel
  # rehashes those flows onto workers that can actually answer.
  #
  # The candidate set is exactly what $DOMAIN resolves to (via unbound, hence
  # under DNS control) — widen the pool by serving more addresses for the name;
  # there is deliberately no client-side seed list. To route the DoH through
  # the subscription instead, the lever is the honk rule below:
  # `pname(microdoh3) -> direct`.
  #
  # What the name resolves to is cfst-doh-upstream's job (module below): it runs
  # XIU2/CloudflareSpeedTest on this line, keeps the addresses that are fast
  # *and* actually answer this endpoint over TLS, and publishes them as unbound
  # local-data. Two consequences worth knowing:
  #
  #   * A/AAAA for the name are then answered from that local zone, which
  #     unbound serves in the worker before DNS64 and before the forward-zone,
  #     so the pinned IPv4 addresses arrive without a DNS64-synthesised twin.
  #   * with `ipv6 = false` (the default) AAAA is an authoritative NODATA — a
  #     transparent local zone does not fall through for a name it has data
  #     for — so the pool is the selected IPv4 set only. That drops the
  #     two-family comparison (native IPv6 vs IPv4 to the same edge, measured
  #     >10x apart) this block used to rely on. Set
  #     `services.cfstDohUpstream.ipv6 = true` to test Cloudflare's IPv6 ranges
  #     too and pin native AAAA, restoring it.
  #
  # The weighted selection still has real work to do either way: the pinned
  # addresses differ by >10x at the same instant (0.42s vs 5s timeouts), and
  # CFST's ranking is per-run noise on top of that.
  #
  # NOTE: --xps-cpus is deliberately NOT enabled. It is implemented (and the
  # module exposes it), but enabling it here would give 12 workers instead of 6
  # — these NICs expose 12 XPS queue CPUs, more than the 6 physical cores —
  # while microdoh3's own sockets never meet those queues: queries arrive on
  # loopback from unbound and the DoH egress leaves through the nat64 TUN and a
  # single-queue ppp0. No locality to win, and doubling the upstream
  # connections only adds handshakes. Turn on with
  # `--xps-cpus --xps-interface enp2s0f1` if that topology changes.
  services.microdoh3 = {
    enable = true;
    package = inputs.microdoh3.packages.${pkgs.stdenv.hostPlatform.system}.microdoh3;
    listen = "[::1]:5443";
    upstream = "https://unset";  # overridden by ExecStart script
    bootstrapDns = "127.0.0.1";
    timeoutSecs = 30;
    tokenFile = null;
  };

  systemd.services.microdoh3 = {
    after = lib.mkForce [ "network.target" "unbound.service" "agenix.service" ];
    wants = lib.mkForce [ "unbound.service" ];

    serviceConfig = {
      ExecStart = lib.mkForce (
        let
          script = pkgs.writeShellScript "microdoh3-start" ''
            set -a; . /run/agenix/doh-env; set +a
            exec ${config.services.microdoh3.package}/bin/microdoh3 \
              --listen ${config.services.microdoh3.listen} \
              --upstream "https://$DOMAIN$URI_PATH" \
              --bootstrap-dns ${config.services.microdoh3.bootstrapDns} \
              --timeout-secs ${toString config.services.microdoh3.timeoutSecs} \
              --workers ${toString config.services.microdoh3.workers} \
              --token "$TOKEN"
          '';
        in "+${script}"
      );
      ExecStartPre = lib.mkForce null;
      RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
    };
  };

  # ==================== Cloudflare 优选 IP for the DoH upstream ====================
  # CloudflareSpeedTest on a timer → unbound local-data → microdoh3's candidate
  # set (see the block above for how the two compose). The DoH endpoint's
  # DOMAIN/URI_PATH/TOKEN come from the same agenix file microdoh3 reads.
  #
  # `ipv6 = true` additionally pins native AAAA from Cloudflare's IPv6 ranges;
  # left off here because the native IPv6 path to the edge measured 30-60% loss
  # on this line, and the 优选 workflow is IPv4-centric.
  services.cfstDohUpstream = {
    enable = true;
    # ipv6 = true;
  };

  services.resolved.enable = false;

  # ==================== Avahi mDNS ====================
  # Acts as mDNS responder (publishing) on local interfaces.
  services.avahi = {
    enable = true;
    nssmdns4 = true;
    nssmdns6 = true;
    allowInterfaces = [ "br-lan" ];
    publish = {
      enable = true;
      addresses = true;
      domain = true;
      workstation = true;
      userServices = true;
    };
  };

  # ==================== Samba ====================
  services.samba = {
    enable = true;
    package = pkgs.samba4Full;
    openFirewall = false;
    settings = {
      global = {
        "workgroup" = "WORKGROUP";
        "server string" = "nixos";
        "netbios name" = "NIXOS";
        "security" = "user";
        "hosts allow" = "all";
        "invalid users" = lib.mkForce [ ];
        "guest account" = "nobody";
        "map to guest" = "never";
        "unix password sync" = "yes";
        "pam password change" = "yes";
      };
      "HDD" = {
        "path" = "/mnt/HDD";
        "browseable" = "yes";
        "read only" = "no";
        "guest ok" = "no";
        "create mask" = "0644";
        "directory mask" = "0755";
        "valid users" = "root";
        "force user" = "root";
        "force group" = "root";
      };
    };
  };

  services.samba-wsdd = {
    enable = true;
    openFirewall = false;
    discovery = true;
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

  systemd.services.tayga.serviceConfig.ExecStartPost = [
    "+${pkgs.iproute2}/bin/ip -6 route replace 64:ff9b::/96 dev nat64 src fdea:d:beef::1"
  ];

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
      lan-to-wan = { from = [ "lan" ]; to = [ "wan" ]; verdict = "accept"; masquerade = true; };
      lan-to-nat64 = { from = [ "lan" ]; to = [ "nat64" ]; verdict = "accept"; };
      nat64-to-wan = { from = [ "nat64" ]; to = [ "wan" ]; verdict = "accept"; masquerade = true; };
      lan-to-fw-icmp = { from = [ "lan" ]; to = [ "fw" ]; extraLines = [ "ip protocol icmp accept comment \"Allow ICMP from LAN\"" ]; };
      lan-to-fw-ipv6 = { from = [ "lan" ]; to = [ "fw" ]; extraLines = [ "meta l4proto icmpv6 accept comment \"Allow ICMPv6 from LAN\"" ]; };
      lan-to-fw-dns = { from = [ "lan" ]; to = [ "fw" ]; allowedUDPPorts = [ 53 ]; allowedTCPPorts = [ 53 ]; };
      lan-to-fw-mdns = { from = [ "lan" ]; to = [ "fw" ]; allowedUDPPorts = [ 5353 ]; };
      lan-to-lan-mdns = { from = [ "lan" ]; to = [ "lan" ]; allowedUDPPorts = [ 5353 ]; extraLines = [ "ip6 daddr ff02::fb udp dport 5353 accept" ]; };
      lan-to-fw-dhcpv6 = { from = [ "lan" ]; to = [ "fw" ]; allowedUDPPorts = [ 547 ]; };
      lan-to-fw-ssh = { from = [ "lan" ]; to = [ "fw" ]; allowedTCPPorts = config.services.openssh.ports; };
      lan-to-fw-cockpit = { from = [ "lan" ]; to = [ "fw" ]; allowedTCPPorts = [ 9090 ]; };
      # honk clash_api ([::]:9091)
      lan-to-fw-clashapi = { from = [ "lan" ]; to = [ "fw" ]; allowedTCPPorts = [ 9091 ]; };
      lan-to-fw-awg = { from = [ "lan" ]; to = [ "fw" ]; allowedUDPPorts = [ 47999 ]; };
      lan-to-fw-samba = { from = [ "lan" ]; to = [ "fw" ]; allowedTCPPorts = [ 139 445 ]; allowedUDPPorts = [ 137 138 ]; };
      lan-to-fw-wsdd = { from = [ "lan" ]; to = [ "fw" ]; allowedUDPPorts = [ 3702 ]; };
      lan-to-awg = { from = [ "lan" ]; to = [ "awg" ]; verdict = "accept"; };
      awg-to-lan = { from = [ "awg" ]; to = [ "lan" ]; verdict = "accept"; };
      awg-to-nat64 = { from = [ "awg" ]; to = [ "nat64" ]; verdict = "accept"; };
      awg-to-wan = { from = [ "awg" ]; to = [ "wan" ]; verdict = "accept"; };
      awg-to-fw = { from = [ "awg" ]; to = [ "fw" ]; verdict = "accept"; };
      wan-to-fw-ipv6 = { from = [ "wan" ]; to = [ "fw" ]; allowedUDPPorts = [ 546 ]; extraLines = [ "meta l4proto icmpv6 accept comment \"Allow ICMPv6 for RAs and ND\"" ]; };
    };
  };

  # NNF's masquerade is IPv4-only (hardcoded `meta protocol ip`).
  # Add IPv6 masquerade for AWG clients whose ULA addresses aren't globally routable.
  networking.nftables.chains.postrouting.awg-ipv6-masq = {
    after = [ "generated" ];
    rules = [ "meta protocol ip6 iifname awg0 oifname ppp0 masquerade" ];
  };


  # ==================== HONK ====================
  network.honk = {
    enable = true;
    package = inputs.honk.packages.x86_64-linux.default;
    assets = with pkgs; [
      v2ray-geoip
      (inputs.v2ray-rules-dat.packages.x86_64-linux.default)
    ];
    openFirewall = {
      enable = true;
      port = 10800;
    };
    config = ''
      global {
        tproxy_port: 10800
        wan_interface: ppp0 # Use "auto" to auto detect WAN interface.
        # nat64 (tayga) is attached as a LAN interface so post-translation
        # IPv4 packets are routed by honk; see the 64:ff9b::/96 rule below.
        lan_interface: br-lan, awg0, nat64
        data_dir: '/var/lib/honk'

        log_level: info
        allow_insecure: false
        auto_config_kernel_parameter: false
        dial_mode: domain++
        tls_implementation: utls
        # Migrated from experimental.udp_nfqueue.enabled, which the upgraded
        # honk still honors but reports as deprecated.
        nfqueue_enable: true
      }

      subscription {
        local: 'file:///etc/honk/local.sub'
      }

      group {
        proxy {
          policy: fixed(0)
        }

        resident {
          policy: fixed(1)
        }

        vowifi {
          policy: fixed(2)
        }
      }
      dns {
        upstream {
          unbound: 'tcp://127.0.0.1:53'
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
        # DNS64/NAT64 destinations must bypass the proxy entirely so they
        # reach tayga untranslated; the translated IPv4 side is intercepted
        # on the nat64 interface instead.
        dip('64:ff9b::/96') -> direct
        # `-> must_rules` (dae's match-and-continue, terminal/no-sniff) is not a
        # routing target at honk 2b5bda2; these two rules keep its outcome for
        # unbound's own traffic: CN direct, everything else via the proxy group.
        pname(unbound) && dip(geoip:cn) -> direct(must)
        pname(unbound) -> proxy(must)
        pname(microdoh3) -> direct
        # The 优选-IP machinery must measure the path microdoh3 actually uses:
        # routed through the subscription, CFST would rank proxy exits and the
        # probe would validate the proxy's reachability instead. `dohprobe` is
        # the DoH validator — a copy of curl under that name, see
        # modules/cfst-doh-upstream.nix.
        #
        # `(must)` is load-bearing. A plain `direct` decision is punted to
        # userspace, which resolves the process name asynchronously; a socket
        # that lives <100ms (every CFST probe) is gone before that lookup, so the
        # connection fell through to `fallback: proxy` — measured as a local
        # 0.03ms "handshake", and the ~24k proxied connections exhausted honk's
        # UDP transport pool. A must rule is terminal in the fast path, so the
        # name is taken while the socket still exists.
        pname(cfst) -> direct(must)
        pname(dohprobe) -> direct(must)

        dip(geoip:gb) && dport(500, 4500) && l4proto(udp) -> vowifi

        domain(geosite: google-cn) -> proxy
        domain(full: dash.cloudflare.com, suffix: withpersona.com) -> resident
        domain(full: google.com, full: www.google.com) -> resident
        domain(geosite: facebook) -> resident
        domain(geosite: openai, geosite: anthropic, geosite: xai, geosite: google-gemini) -> resident

        dip(geoip:cn) -> direct
        domain(geosite:cn) -> direct

        fallback: proxy
      }
      experimental {
        clash_api {
          external_controller: '[::]:9091'
        }
      }
    '';
  };

  environment.etc."systemd/journald@honk.conf".text = ''
    [Journal]
    Storage=volatile
    RuntimeMaxFileSize=5M
    RuntimeMaxFiles=3
  '';
  systemd.services.honk = {
    after = [ "wg-quick-awg0.service" "tayga.service" ];
    wants = [ "wg-quick-awg0.service" "tayga.service" ];
  };
  systemd.services.honk.serviceConfig.Restart = "on-failure";
  systemd.services.honk.serviceConfig.LogNamespace = "honk";

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
