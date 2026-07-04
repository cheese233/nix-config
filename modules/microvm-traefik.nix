{ config, lib, pkgs, inputs, ... }:

let

  tapId = "vm-traefik";
in
{
  # QEMU MicroVM running Traefik, directly attached to the existing br-lan bridge.
  microvm.vms.traefik = {
    # Reuse the host's package set so /nix/store paths match the host's.
    inherit pkgs;

    config = {
      networking.hostName = "traefik";
      system.stateVersion = "26.05";

      # Fixed machine-id so host can identify this VM's journals (hex only)
      microvm.machineId = "70aef1c0000000000000000000000000";

      # Minimal base: systemd-networkd is enabled via microvm.optimize by default,
      # but we declare it explicitly below.
      microvm = {
        hypervisor = "qemu";
        vcpu = 2;
        mem = 512;
        balloon = true;

        # With a virtiofs share of /nix/store we do not need to embed the store
        # in the boot squashfs. This makes the VM tiny and guarantees it reuses
        # the host's /nix/store byte-for-byte.
        storeOnDisk = false;

        shares = [
          {
            proto = "virtiofs";
            tag = "ro-store";
            source = "/nix/store";
            mountPoint = "/nix/.ro-store";
            readOnly = true;
          }
          {
            proto = "virtiofs";
            tag = "traefik-data";
            # Relative to /var/lib/microvms/traefik on the host.
            source = "traefik-data";
            mountPoint = "/var/lib/traefik";
            socket = "traefik-data.sock";
          }
          {
            proto = "virtiofs";
            tag = "journal";
            # Source path relative to microvm.stateDir
            source = "journal";
            mountPoint = "/var/log/journal";
            socket = "journal.sock";
          }
        ];

        interfaces = [
          {
            type = "tap";
            id = tapId;
            mac = "02:00:00:77:65:62";
            # vhost-net acceleration is qemu-only and gives ~10 Gbps vs ~1.5 Gbps.
            tap.vhost = true;
          }
        ];

        # The default tap-up script creates the tap and brings it up, but does not
        # attach it to any bridge. Attach it to br-lan so the VM shares the LAN
        # segment with wired clients.
        binScripts.tap-up = lib.mkAfter ''
          ${lib.getExe' pkgs.iproute2 "ip"} link set dev '${tapId}' master br-lan
        '';
      };

      systemd.network.enable = true;

      # Bring up the first virtio ethernet adapter with a static LAN IPv6 address.
      # IPv6 default routes and DNS come from radvd on the host (IPv6AcceptRA).
      systemd.network.networks."10-lan" = {
        matchConfig.Name = "eth0";
        networkConfig = {
          DHCP = "no";
          IPv6AcceptRA = true;
          DNS = [ "fdea:d:beef::1" ];
        };
      };

      # Traefik reverse proxy / edge router.
      services.traefik = {
        enable = true;
        group = "traefik";
        dataDir = "/var/lib/traefik";
        environmentFiles = [ "/var/lib/traefik/traefik-env" ];

        staticConfigOptions = {
          global = {
            checkNewVersion = false;
            sendAnonymousUsage = false;
          };

          api.dashboard = true;

          entryPoints.web = {
            address = ":80";
            http.redirections.entryPoint = {
              to = "websecure";
              scheme = "https";
              permanent = true;
            };
          };
          entryPoints.websecure = {
            address = ":443";
          };

          certificatesResolvers.letsencrypt.acme = {
            email = "postmaster+traefik@c23.me"; # TODO: replace with your email
            storage = "${config.services.traefik.dataDir}/acme.json";
            tlsChallenge = {};
          };

          # Log to journald by default; access logs can be enabled later.
          log = {
            level = "INFO";
          };
        };

        # Dynamic configuration: add routers/services here as you add backends.
        dynamicConfigOptions = {
          http.routers.dashboard = {
            rule = "Host(`traefik.local`)";
            service = "api@internal";
            entryPoints = [ "websecure" ];
            middlewares = [ "lan-only" ];
          };
          http.middlewares.lan-only.ipAllowList.sourceRange = [ "fdea:d:beef::/64" ];
        };
      };

      systemd.tmpfiles.rules = [
        "d /var/lib/traefik 0750 traefik traefik -"
      ];

      users.users.traefik = {
        isSystemUser = true;
        group = "traefik";
      };
      users.groups.traefik = {};

      # No SSH; access the VM via the QEMU serial console from the host instead.
      users.users.root.password = "";
      services.getty.autologinUser = "root";

      networking.firewall = {
        enable = true;
        allowedTCPPorts = [ 80 443 ];
      };

    };
  };

  age.secrets.traefik-env = {
    file = ../secrets/traefik-env.age;
    path = "/var/lib/microvms/traefik/traefik-data/traefik-env";
    owner = "root";
    group = "root";
    mode = "0640";
  };

  systemd.tmpfiles.rules = [
    "d ${config.microvm.stateDir}/traefik/journal 0755 root root -"
    # Link Traefik MicroVM journals so host's journalctl --merge can see them
    "L+ /var/log/journal/70aef1c0000000000000000000000000 - - - - ${config.microvm.stateDir}/traefik/journal/70aef1c0000000000000000000000000"
  ];

  # DMZ: allow all traffic from WAN to Traefik VM.
  # Matches by EUI-64 interface ID derived from MAC 02:00:00:00:00:7a,
  # so it works regardless of the delegated prefix.
  networking.nftables.firewall = {
    zones.traefik = {
      parent = "lan";
      ingressExpression = [
        "ip6 saddr & ::ffff:ffff:ffff:ffff == ::ff:fe77:6562"
      ];
      egressExpression = [
        "ip6 daddr & ::ffff:ffff:ffff:ffff == ::ff:fe77:6562"
      ];
    };
    rules.wan-to-traefik = {
      from = [ "wan" ];
      to = [ "traefik" ];
      verdict = "accept";
    };
  };

  # Start the Traefik MicroVM automatically on host boot.
  microvm.autostart = [ "traefik" ];
}
